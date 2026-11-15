//! Lines of text in one buffer, and the JSON view's text built into them on other threads.

use std::sync::Arc;

use rongo_core::bson::Document;
use rongo_core::format::{self, Style};

use super::docs::Docs;

/// Lines of text in one buffer, separated by `\n`, and where each line
/// starts: half the memory of a `String` per line, and any line at hand for
/// drawing only those in view (`line_view`). The JSON view's text, and the
/// shell's raw output.
#[derive(Default)]
pub(crate) struct TextLines {
    pub(super) text: String,
    pub(super) starts: Vec<usize>,
}

impl TextLines {
    /// Starts a line, and returns the buffer to write it into.
    fn new_line(&mut self) -> &mut String {
        if !self.starts.is_empty() {
            self.text.push('\n');
        }
        self.starts.push(self.text.len());
        &mut self.text
    }

    pub(crate) fn push(&mut self, line: &str) {
        self.new_line().push_str(line);
    }

    /// All of it, the lines joined by `\n`.
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// The lines of `text` (split at `\n`), written straight into the buffer.
    pub(crate) fn push_text(&mut self, text: &str) {
        let at = self.new_line().len();
        self.starts.extend(memchr::memchr_iter(b'\n', text.as_bytes()).map(|k| at + k + 1));
        self.text.push_str(text);
    }

    /// `parts` one after the other, as if their lines had been pushed here
    /// in turn.
    fn join(parts: Vec<TextLines>) -> TextLines {
        let mut parts = parts.into_iter().filter(|p| !p.starts.is_empty());
        let Some(mut out) = parts.next() else { return TextLines::default() };
        let rest: Vec<TextLines> = parts.collect();
        out.text.reserve_exact(rest.iter().map(|p| p.text.len() + 1).sum());
        out.starts.reserve_exact(rest.iter().map(|p| p.starts.len()).sum());
        for p in rest {
            out.text.push('\n');
            let at = out.text.len();
            out.starts.extend(p.starts.iter().map(|s| at + s));
            out.text.push_str(&p.text);
        }
        out
    }

    pub(crate) fn len(&self) -> usize {
        self.starts.len()
    }

    pub(crate) fn line(&self, i: usize) -> &str {
        let end = self.starts.get(i + 1).map_or(self.text.len(), |s| s - 1);
        &self.text[self.starts[i]..end]
    }

    #[cfg(test)]
    pub(super) fn from_lines<S: AsRef<str>>(lines: &[S]) -> Self {
        let mut t = Self::default();
        for l in lines {
            t.push(l.as_ref());
        }
        t
    }
}

/// A JSON text being built on other threads (`build_json`), for the view
/// or for ⌘A ⌘C. Dropping it stops the builder: it looks at `cancel` every
/// BUILD_CHECK documents of each chunk.
pub(super) struct Build {
    pub(super) rx: std::sync::mpsc::Receiver<TextLines>,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    /// Documents formatted so far, by every chunk.
    #[cfg(test)]
    pub(super) formatted: Formatted,
}

/// Counts the documents a build formats, for tests (a superseded build must
/// stop part way), says when its thread has ended, and can hold it
/// (`hold_next_build`). Outside tests it holds and does nothing.
#[derive(Clone, Default)]
pub(super) struct Formatted(#[cfg(test)] Arc<Watch>);

/// Tests: what `Formatted` keeps.
#[cfg(test)]
#[derive(Default)]
pub(super) struct Watch {
    count: std::sync::atomic::AtomicUsize,
    ended: std::sync::atomic::AtomicBool,
    held: std::sync::Mutex<bool>,
    go: std::sync::Condvar,
}

#[cfg(test)]
thread_local! {
    /// Tests: the next build started on this thread is held.
    static HOLD_NEXT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Tests: the next build started on this thread formats at most
/// BUILD_CHECK documents per chunk, then waits at its next look at the
/// cancel flag until `Formatted::release`: it can't finish before the test
/// has changed the rows, however loaded the machine.
#[cfg(test)]
pub(super) fn hold_next_build() {
    HOLD_NEXT.set(true);
}

impl Formatted {
    #[cfg(not(test))]
    fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn new() -> Self {
        Self(Arc::new(Watch { held: std::sync::Mutex::new(HOLD_NEXT.take()), ..Default::default() }))
    }

    fn one(&self) {
        #[cfg(test)]
        self.0.count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Tests: waits while the build is held.
    fn wait_if_held(&self) {
        #[cfg(test)]
        {
            let mut held = self.0.held.lock().expect("hold");
            while *held {
                held = self.0.go.wait(held).expect("hold");
            }
        }
    }

    fn ended(&self) {
        #[cfg(test)]
        self.0.ended.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(super) fn count(&self) -> usize {
        self.0.count.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Tests: whether the build's thread has ended (it formats no more).
    #[cfg(test)]
    pub(super) fn has_ended(&self) -> bool {
        self.0.ended.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Tests: lets a held build go on.
    #[cfg(test)]
    pub(super) fn release(&self) {
        *self.0.held.lock().expect("hold") = false;
        self.0.go.notify_all();
    }
}

impl Build {
    pub(super) fn start(rows: Docs, offset: usize, style: Style, ctx: egui::Context) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let count = Formatted::new();
        #[cfg(test)]
        let formatted = count.clone();
        let stop = cancel.clone();
        std::thread::spawn(move || {
            let text = build_json(&rows, offset, style, &stop, &count);
            count.ended();
            if let Some(text) = text
                && tx.send(text).is_ok()
            {
                ctx.request_repaint();
            }
        });
        Self {
            rx,
            cancel,
            #[cfg(test)]
            formatted,
        }
    }
}

impl Drop for Build {
    fn drop(&mut self) {
        self.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Documents each chunk of `build_json` formats between looks at its
/// cancel flag.
const BUILD_CHECK: usize = 64;

/// The JSON view's lines: each document in shell syntax, numbered, row by
/// row. The view shows `Style::Display`; ⌘A ⌘C copies `Style::Editable`,
/// which pastes back as the same types. Built in chunks of rows, one per
/// core, each document parsed, formatted straight into its chunk's buffer
/// and dropped in turn; the chunks are then joined. None once `cancel` is
/// set. `formatted` counts the documents done (for tests).
fn build_json(rows: &Docs, offset: usize, style: Style, cancel: &std::sync::atomic::AtomicBool, formatted: &Formatted) -> Option<TextLines> {
    let n = rows.len();
    // A few hundred documents at least: fewer aren't worth a thread.
    let chunks = std::thread::available_parallelism().map_or(1, |p| p.get()).min(n.div_ceil(256)).max(1);
    let per = n.div_ceil(chunks);
    let part = |range: std::ops::Range<usize>| json_part(rows, range, offset, style, cancel, formatted);
    let parts: Vec<Option<TextLines>> = if chunks == 1 {
        vec![part(0..n)]
    } else {
        std::thread::scope(|s| {
            let part = &part;
            let running: Vec<_> = (0..chunks).map(|c| s.spawn(move || part((c * per).min(n)..((c + 1) * per).min(n)))).collect();
            running.into_iter().map(|t| t.join().unwrap_or_else(|e| std::panic::resume_unwind(e))).collect()
        })
    };
    Some(TextLines::join(parts.into_iter().collect::<Option<Vec<_>>>()?))
}

/// The lines of rows `range` (see `build_json`).
fn json_part(rows: &Docs, range: std::ops::Range<usize>, offset: usize, style: Style, cancel: &std::sync::atomic::AtomicBool, formatted: &Formatted) -> Option<TextLines> {
    use std::fmt::Write;
    // The text comes out about a third longer than the BSON it is from.
    let raw: usize = range.clone().filter_map(|i| rows.raw_doc(i)).map(|d| d.as_bytes().len()).sum();
    let mut out = TextLines { text: String::with_capacity(raw + raw / 2), starts: Vec::with_capacity(raw / 32) };
    for i in range.clone() {
        if (i - range.start).is_multiple_of(BUILD_CHECK) {
            if i > range.start {
                formatted.wait_if_held();
            }
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return None;
            }
        }
        let Some(raw) = rows.raw_doc(i) else { continue };
        if i > 0 {
            out.new_line();
        }
        let _ = write!(out.new_line(), "// {}", offset + i + 1);
        match Document::try_from(raw.as_ref()) {
            Ok(d) => out.push_text(&format::doc_to_shell(&d, style)),
            Err(e) => format!("(unreadable BSON: {e})").lines().for_each(|l| out.push(l)),
        }
        formatted.one();
    }
    Some(out)
}

/// `build_json` to the end, for tests.
#[cfg(test)]
pub(super) fn json_lines(rows: &Docs, offset: usize, style: Style) -> TextLines {
    build_json(rows, offset, style, &std::sync::atomic::AtomicBool::new(false), &Formatted::default()).expect("not cancelled")
}
