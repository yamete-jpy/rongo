//! rongo-shell: a mongosh-style JavaScript shell on QuickJS.
//!
//! Each shell tab owns a [`ShellHandle`], which runs scripts on its own thread.
//! [`parse_value`] and [`parse_document`] evaluate shell syntax such as
//! `{ _id: ObjectId("…"), at: ISODate("…") }` for the document editor and the
//! query bar.

mod b64;
pub mod complete;
mod preprocess;
mod runtime;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rongo_core::bson::{Bson, Document};
use rongo_core::{FindSpec, Namespace, Session, Waker};

pub use preprocess::preprocess;

/// Something a script produced that deserves its own result tab.
#[derive(Clone, Debug)]
pub enum ResultItem {
    /// A find cursor the script did not read. The tab pages through it with
    /// ordinary find requests; `max` is the cursor's own `limit()`, if any.
    Find { line: u32, spec: FindSpec, max: Option<u64> },
    /// Documents already fetched: aggregation output, or the script's final
    /// value when it is a document or an array of documents.
    Docs { line: u32, title: String, docs: Vec<Document>, truncated: bool },
}

#[derive(Clone, Debug)]
pub enum ShellEvent {
    Print { run: u64, text: String },
    Result { run: u64, item: ResultItem },
    Done { run: u64, elapsed_ms: u64, db: String, error: Option<ShellError> },
}

#[derive(Clone, Debug, PartialEq)]
pub struct ShellError {
    pub message: String,
    /// 1-based line in the editor, when known.
    pub line: Option<u32>,
}

impl std::fmt::Display for ShellError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.line {
            Some(l) => write!(f, "line {l}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

struct Job {
    run: u64,
    code: String,
    db: String,
    line_offset: u32,
}

pub struct ShellHandle {
    jobs: crossbeam_channel::Sender<Job>,
    events: crossbeam_channel::Receiver<ShellEvent>,
    interrupt: Arc<AtomicBool>,
    next_run: u64,
}

impl ShellHandle {
    pub fn spawn(session: Session, waker: Waker) -> ShellHandle {
        let (jobs, job_rx) = crossbeam_channel::unbounded::<Job>();
        let (ev_tx, events) = crossbeam_channel::unbounded::<ShellEvent>();
        let interrupt = Arc::new(AtomicBool::new(false));
        let flag = interrupt.clone();
        std::thread::Builder::new()
            .name("rongo-shell".into())
            .stack_size(16 << 20)
            .spawn(move || runtime::shell_thread(session, job_rx, ev_tx, waker, flag))
            .expect("spawn shell thread");
        ShellHandle { jobs, events, interrupt, next_run: 1 }
    }

    /// Queue `code` to run against database `db`. `line_offset` is added to
    /// reported line numbers, for running a selection out of a larger script.
    pub fn run(&mut self, code: impl Into<String>, db: impl Into<String>, line_offset: u32) -> u64 {
        let run = self.next_run;
        self.next_run += 1;
        self.interrupt.store(false, Ordering::SeqCst);
        let _ = self.jobs.send(Job { run, code: code.into(), db: db.into(), line_offset });
        run
    }

    /// Stop the running script at the next opportunity, including while it is
    /// waiting on the server.
    pub fn interrupt(&self) {
        self.interrupt.store(true, Ordering::SeqCst);
    }

    pub fn try_recv(&self) -> Option<ShellEvent> {
        self.events.try_recv().ok()
    }
}

/// Evaluate a single shell expression with no database access.
pub fn parse_value(text: &str) -> Result<Bson, ShellError> {
    runtime::eval_detached(text)
}

/// Evaluate shell text that must produce a document, e.g. a filter.
/// Empty text is an empty document.
pub fn parse_document(text: &str) -> Result<Document, ShellError> {
    if text.trim().is_empty() {
        return Ok(Document::new());
    }
    // `{ a: 1 }, { b: 2 }` is valid JS (the comma operator) and would quietly
    // mean only the last object.
    let objects = top_level_objects(text.trim()).len();
    if objects > 1 {
        return Err(ShellError { message: format!("expected one object, found {objects}"), line: None });
    }
    match parse_value(text)? {
        Bson::Document(d) => Ok(d),
        other => Err(ShellError {
            message: format!("expected an object, got {}", rongo_core::format::kind(&other).label()),
            line: None,
        }),
    }
}

/// Evaluate shell text that must produce an array of documents, e.g. a
/// pipeline.
pub fn parse_pipeline(text: &str) -> Result<Vec<Document>, ShellError> {
    match parse_value(text)? {
        Bson::Array(items) => items
            .into_iter()
            .map(|i| match i {
                Bson::Document(d) => Ok(d),
                _ => Err(ShellError { message: "every pipeline stage must be an object".into(), line: None }),
            })
            .collect(),
        _ => Err(ShellError { message: "expected an array of stages".into(), line: None }),
    }
}

/// Documents from pasted text: one document, an array of documents, or several
/// documents one after another (JSON lines, mongoexport output, or shell
/// syntax copied from a GUI). Extended JSON (`{"$oid": …}`) works too.
pub fn parse_documents_text(text: &str) -> Result<Vec<Document>, ShellError> {
    let fail = |message: &str| ShellError { message: message.into(), line: None };
    let trimmed = text.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return Err(fail("the clipboard is empty"));
    }
    // Several top-level objects, with or without commas between them, are
    // several documents (JS would read `{…}, {…}` as just the last one).
    let chunks = top_level_objects(trimmed);
    if trimmed.starts_with('{') && chunks.len() > 1 {
        return chunks.into_iter().map(parse_document).collect();
    }
    let whole = parse_value(trimmed);
    match whole {
        Ok(Bson::Document(d)) => return Ok(vec![d]),
        Ok(Bson::Array(items)) => {
            return items
                .into_iter()
                .map(|i| match i {
                    Bson::Document(d) => Ok(d),
                    _ => Err(fail("the array contains something other than documents")),
                })
                .collect();
        }
        Ok(_) => return Err(fail("the clipboard does not contain documents")),
        Err(_) => {}
    }
    if chunks.len() < 2 {
        return Err(whole.err().unwrap_or_else(|| fail("the clipboard does not contain documents")));
    }
    chunks.into_iter().map(parse_document).collect()
}

/// Split text into its top-level `{…}` blocks, skipping strings and comments.
fn top_level_objects(text: &str) -> Vec<&str> {
    let b = text.as_bytes();
    let (mut out, mut depth, mut start, mut i) = (Vec::new(), 0usize, 0usize, 0usize);
    while i < b.len() {
        match b[i] {
            q @ (b'"' | b'\'' | b'`') => {
                i += 1;
                while i < b.len() && b[i] != q {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'{' => {
                if depth == 0 {
                    start = i;
                }
                depth += 1;
            }
            b'}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    out.push(&text[start..=i]);
                }
            }
            _ => {}
        }
        i += 1;
    }
    out
}

pub(crate) fn find_spec(ns: Namespace, filter: Document, sort: Option<Document>, projection: Option<Document>, skip: u64) -> FindSpec {
    FindSpec { ns, filter, sort, projection, skip, limit: 50 }
}

#[cfg(test)]
mod tests;
