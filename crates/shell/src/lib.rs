//! rongo-shell: a mongosh-style JavaScript shell on QuickJS.
//!
//! Each shell tab owns a [`ShellHandle`], which runs scripts on its own thread.
//! [`parse_value`], [`parse_document`] and [`parse_pipeline`] evaluate shell
//! syntax such as `{ _id: ObjectId("…"), at: ISODate("…") }` for the document
//! editor, the query bar and the dialogs, and [`parse_documents_text`] reads
//! pasted documents; all without a connection, on the caller's thread.

pub mod complete;
mod preprocess;
mod runtime;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rongo_core::bson::{Bson, Document};
use rongo_core::{FindSpec, Session, Waker};

pub use preprocess::preprocess;
use runtime::{DetachedError, EDITED, PASTED, TYPED};

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
    /// What `print()` calls wrote, one string per call, several at a time.
    Print { run: u64, texts: Vec<String> },
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

/// A dropped handle (its tab closed, or dropped on Disconnect) stops its
/// script as Stop does; the thread then ends, its jobs channel closed.
impl Drop for ShellHandle {
    fn drop(&mut self) {
        self.interrupt();
    }
}

impl ShellError {
    fn msg(message: impl Into<String>) -> ShellError {
        ShellError { message: message.into(), line: None }
    }
}

/// The one value text evaluated to (detached parses read text as a list, so
/// JS's comma operator can't quietly keep only the last of several).
fn one(mut items: Vec<Bson>, what: &str) -> Result<Bson, ShellError> {
    match items.len() {
        0 => Err(ShellError::msg("the value is empty")),
        1 => Ok(items.remove(0)),
        n => Err(ShellError::msg(format!("expected one {what}, found {n}"))),
    }
}

fn docs_of(items: Vec<Bson>, message: &str) -> Result<Vec<Document>, ShellError> {
    items
        .into_iter()
        .map(|i| match i {
            Bson::Document(d) => Ok(d),
            _ => Err(ShellError::msg(message)),
        })
        .collect()
}

/// Evaluate a single shell expression with no database access.
pub fn parse_value(text: &str) -> Result<Bson, ShellError> {
    one(runtime::eval_detached(text, &TYPED)?, "value")
}

/// Evaluate shell text that must produce a document, e.g. a filter.
/// Empty text is an empty document.
pub fn parse_document(text: &str) -> Result<Document, ShellError> {
    document_within(text, &TYPED)
}

/// A whole document from the document editor: like [`parse_document`], with
/// room for one of several MB (MongoDB allows 16).
pub fn parse_editor_document(text: &str) -> Result<Document, ShellError> {
    document_within(text, &EDITED)
}

fn document_within(text: &str, limits: &runtime::Limits) -> Result<Document, ShellError> {
    if text.trim().is_empty() {
        return Ok(Document::new());
    }
    match one(runtime::eval_detached(text, limits)?, "object")? {
        Bson::Document(d) => Ok(d),
        other => Err(ShellError::msg(format!("expected an object, got {}", rongo_core::format::kind(&other).label()))),
    }
}

/// Evaluate shell text that must produce an array of documents, e.g. a
/// pipeline.
pub fn parse_pipeline(text: &str) -> Result<Vec<Document>, ShellError> {
    match one(runtime::eval_detached(text, &TYPED)?, "array of stages")? {
        Bson::Array(items) => docs_of(items, "every pipeline stage must be an object"),
        _ => Err(ShellError::msg("expected an array of stages")),
    }
}

/// Documents from pasted text: one document, an array of documents, or several
/// documents one after another (JSON lines, mongoexport output, or shell
/// syntax copied from a GUI). Extended JSON (`{"$oid": …}`) works too.
pub fn parse_documents_text(text: &str) -> Result<Vec<Document>, ShellError> {
    let trimmed = text.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return Err(ShellError::msg("the clipboard is empty"));
    }
    // Plain (Extended) JSON, such as mongoexport's, is read without the JS
    // engine: much faster, and it keeps integer-like keys where they are and
    // reads every Extended JSON type. (`5.0` is a Double here, as written.)
    if let Some(docs) = json_documents(trimmed) {
        return docs;
    }
    // Several objects with commas between them are several values of the list.
    let whole = match runtime::eval_detached(trimmed, &PASTED) {
        Ok(items) => return documents_in(items),
        Err(DetachedError::Limit(e)) => return Err(e),
        Err(DetachedError::Script(e)) => e,
    };
    // Objects one after another with no commas between them: their `{…}`
    // blocks are read as one list, in one go. Only blanks and comments may
    // separate them; anything else would be dropped, so it is an error.
    let chunks = top_level_objects(trimmed);
    if chunks.len() < 2 {
        return Err(whole);
    }
    let mut from = 0;
    for (start, end) in chunks.iter().map(|c| (c.start, c.end)).chain([(trimmed.len(), trimmed.len())]) {
        let gap = &trimmed[from..start];
        if !blank(gap) {
            let what = gap.trim().chars().take(40).collect::<String>();
            return Err(ShellError::msg(format!("not a document: {what}")));
        }
        from = end;
    }
    let (mut joined, mut starts) = (String::with_capacity(trimmed.len() + 2 * chunks.len()), Vec::with_capacity(chunks.len()));
    let mut line = 1;
    for c in &chunks {
        let text = &trimmed[c.clone()];
        starts.push(line);
        line += 1 + text.matches('\n').count() as u32;
        joined.push_str(text);
        joined.push_str(",\n");
    }
    match runtime::eval_detached(&joined, &PASTED) {
        Ok(items) if items.len() == chunks.len() => docs_of(items, "the clipboard contains something other than documents"),
        Ok(items) => Err(ShellError::msg(format!("expected {} documents, found {}", chunks.len(), items.len()))),
        Err(DetachedError::Limit(e)) => Err(e),
        // Name the document at fault, by the line the error is on.
        Err(DetachedError::Script(e)) => match e.line.map(|l| starts.partition_point(|s| *s <= l)) {
            Some(n) if n > 0 => Err(ShellError::msg(format!("document {n}: {}", e.message))),
            _ => Err(ShellError::msg(e.message)),
        },
    }
}

/// Documents from what pasted text evaluated to: one document, one array of
/// documents, or several documents.
fn documents_in(mut items: Vec<Bson>) -> Result<Vec<Document>, ShellError> {
    match items.len() {
        0 => Err(ShellError::msg("the clipboard does not contain documents")),
        1 => match items.remove(0) {
            Bson::Document(d) => Ok(vec![d]),
            Bson::Array(a) => docs_of(a, "the array contains something other than documents"),
            _ => Err(ShellError::msg("the clipboard does not contain documents")),
        },
        _ => docs_of(items, "the clipboard contains something other than documents"),
    }
}

/// Pasted text read as plain (Extended) JSON: one value, or several one
/// after another (JSON lines). `None` when it isn't, so the JS engine
/// reads it (and names what is wrong).
fn json_documents(text: &str) -> Option<Result<Vec<Document>, ShellError>> {
    let mut items = Vec::new();
    for value in serde_json::Deserializer::from_str(text).into_iter::<serde_json::Value>() {
        items.push(Bson::try_from(value.ok()?).ok()?);
    }
    Some(documents_in(items))
}

/// Whether text between documents is only blanks, commas, semicolons and
/// comments.
fn blank(gap: &str) -> bool {
    let b = gap.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b' ' | b'\t' | b'\r' | b'\n' | b',' | b';' => i += 1,
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => match gap[i + 2..].find("*/") {
                Some(n) => i += 2 + n + 2,
                None => return false,
            },
            _ => return false,
        }
    }
    true
}

/// The byte ranges of text's top-level `{…}` blocks, skipping strings and
/// comments.
fn top_level_objects(text: &str) -> Vec<std::ops::Range<usize>> {
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
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i += 1;
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
                    out.push(start..i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests;
