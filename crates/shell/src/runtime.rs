//! The QuickJS side: a fresh runtime and context for each run (so re-running
//! a script with `const` declarations works, and nothing of a stopped run
//! goes on), and the native dispatcher behind `__native(op, json)`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use rongo_core::bson::{Bson, Document, oid::ObjectId};
use rongo_core::conn::{DocCursor, FindOpts, UpdateSpec, writes_output};
use rongo_core::format::{self, Style};
use rongo_core::{CollKind, CoreError, CoreResult, FindSpec, Namespace, Session, Waker};
use rquickjs::context::EvalOptions;
use rquickjs::promise::PromiseState;
use rquickjs::{Context, Ctx, Function, Runtime, Value};
use serde_json::{Value as Json, json};

use crate::{Job, ResultItem, ShellError, ShellEvent, preprocess};

/// The BSON types and their codec: all a detached parse needs.
const BSON_JS: &str = include_str!("bson.js");
/// The mongosh API for shell runs, evaluated after `BSON_JS`.
const PRELUDE: &str = include_str!("prelude.js");
const MAX_RESULT_DOCS: usize = 1000;
const SHELL_FILE: &str = "<shell>";

/// Unread aggregations whose first batch is kept (see `State::held`): as
/// many as a run shows as result tabs (prelude.js `__maxTabs`), and about
/// this many bytes in all. They live outside the JS memory limit, so a loop
/// of unread aggregations must not keep them all.
const HELD_MAX: usize = 20;
#[cfg(not(test))]
pub(crate) const HELD_BYTES: usize = 64 << 20;
#[cfg(test)]
pub(crate) const HELD_BYTES: usize = 1 << 20;

/// Prints go to the tab together, not one event and one repaint each: once
/// this long after the first of them, or this many bytes of them, and
/// before any other event or a wait on the server.
const PRINT_EVERY: Duration = Duration::from_millis(16);
const PRINT_BYTES: usize = 64 << 10;

struct Output {
    tx: Sender<ShellEvent>,
    waker: Waker,
    run: u64,
    /// Prints not sent yet, their size, and when the first was printed.
    prints: Vec<String>,
    bytes: usize,
    since: Option<Instant>,
}

impl Output {
    fn new(tx: Sender<ShellEvent>, waker: Waker, run: u64) -> Self {
        Self { tx, waker, run, prints: Vec::new(), bytes: 0, since: None }
    }
}

struct State {
    session: Option<Session>,
    interrupt: Arc<AtomicBool>,
    cursors: HashMap<u64, DocCursor>,
    /// An aggregation's first batch (and about how many bytes it takes),
    /// until the script reads it or it goes to a result tab as it came.
    held: HashMap<u64, (Vec<Document>, usize)>,
    held_bytes: usize,
    next_cursor: u64,
    out: Option<Output>,
    /// Tests: how many times a writing pipeline ran (see `echo_writes`).
    #[cfg(test)]
    writing_runs: u64,
}

impl State {
    fn new(session: Option<Session>, interrupt: Arc<AtomicBool>) -> Self {
        Self { session, interrupt, cursors: HashMap::new(), held: HashMap::new(), held_bytes: 0, next_cursor: 1, out: None, #[cfg(test)] writing_runs: 0 }
    }

    /// A held first batch, taken out.
    fn take_held(&mut self, id: u64) -> Option<Vec<Document>> {
        let (docs, bytes) = self.held.remove(&id)?;
        self.held_bytes -= bytes;
        Some(docs)
    }

    fn emit(&mut self, make: impl FnOnce(u64) -> ShellEvent) {
        self.flush();
        if let Some(o) = &self.out {
            let _ = o.tx.send(make(o.run));
            (o.waker)();
        }
    }

    fn print(&mut self, text: String) {
        let Some(o) = &mut self.out else { return };
        o.bytes += text.len();
        o.prints.push(text);
        let since = *o.since.get_or_insert_with(Instant::now);
        if o.bytes >= PRINT_BYTES || since.elapsed() >= PRINT_EVERY {
            self.flush();
        }
    }

    /// Send the prints not sent yet.
    fn flush(&mut self) {
        let Some(o) = &mut self.out else { return };
        if o.prints.is_empty() {
            return;
        }
        let texts = std::mem::take(&mut o.prints);
        (o.bytes, o.since) = (0, None);
        let _ = o.tx.send(ShellEvent::Print { run: o.run, texts });
        (o.waker)();
    }

    /// For the engine's interrupt handler, called as the script runs: a
    /// script that prints and then computes for a while shows its prints.
    fn flush_if_due(&mut self) {
        if self.out.as_ref().and_then(|o| o.since).is_some_and(|s| s.elapsed() >= PRINT_EVERY) {
            self.flush();
        }
    }
}

pub(crate) fn shell_thread(
    session: Session,
    jobs: Receiver<Job>,
    events: Sender<ShellEvent>,
    waker: Waker,
    interrupt: Arc<AtomicBool>,
) {
    let state = Rc::new(RefCell::new(State::new(Some(session), interrupt.clone())));
    for job in jobs.iter() {
        let started = Instant::now();
        state.borrow_mut().out = Some(Output::new(events.clone(), waker.clone(), job.run));
        // A runtime of its own for each run: promise jobs a stopped or failed
        // run left queued end with it, rather than running in the next one.
        let (st, flag) = (state.clone(), interrupt.clone());
        let stop = move || {
            // (Not while a native call holds the state; none runs JS.)
            if let Ok(mut st) = st.try_borrow_mut() {
                st.flush_if_due();
            }
            flag.load(Ordering::Relaxed)
        };
        let (db, error) = match new_runtime(1 << 30, stop) {
            Ok(rt) => {
                rt.set_max_stack_size(8 << 20);
                track_rejections(&rt);
                run_script(&rt, &state, &job)
            }
            Err(e) => (job.db.clone(), Some(ShellError { message: format!("could not start JavaScript: {e}"), line: None })),
        };
        {
            let mut st = state.borrow_mut();
            st.flush();
            st.cursors.clear();
            st.held.clear();
            st.held_bytes = 0;
            st.out = None;
        }
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let _ = events.send(ShellEvent::Done { run: job.run, elapsed_ms, db, error });
        waker();
    }
}

/// A runtime whose script stops (uncatchably) once `stop` says so: Stop in a
/// shell, the deadline of a detached evaluation, which runs on the UI thread.
fn new_runtime(memory: usize, stop: impl FnMut() -> bool + 'static) -> rquickjs::Result<Runtime> {
    let rt = Runtime::new()?;
    rt.set_memory_limit(memory);
    rt.set_interrupt_handler(Some(Box::new(stop)));
    Ok(rt)
}

fn script_options() -> EvalOptions {
    let mut o = EvalOptions::default();
    o.global = true;
    o.strict = false;
    o.filename = Some(SHELL_FILE.into());
    o
}

fn prelude_options(name: &str) -> EvalOptions {
    let mut o = EvalOptions::default();
    o.filename = Some(name.into());
    o
}

fn run_script(rt: &Runtime, state: &Rc<RefCell<State>>, job: &Job) -> (String, Option<ShellError>) {
    let ctx = match Context::full(rt) {
        Ok(c) => c,
        Err(e) => return (job.db.clone(), Some(ShellError { message: e.to_string(), line: None })),
    };
    let offset = i64::from(job.line_offset);
    let stop = state.borrow().interrupt.clone();
    // Stop, as the engine's own interrupt or the error a native call refused
    // with, is "Interrupted"; an error that merely has the word keeps its text.
    let stopped = |mut err: ShellError| {
        if stop.load(Ordering::Relaxed) && matches!(err.message.as_str(), "InternalError: interrupted" | "Error: interrupted") {
            err.message = "Interrupted".into();
        }
        err
    };
    ctx.with(|ctx| {
        if let Err(e) = install(&ctx, state.clone(), &job.db, offset) {
            return (job.db.clone(), Some(stopped(js_error(&ctx, e, 0))));
        }
        let code = preprocess(&job.code);
        let result = ctx.eval_with_options::<Value, _>(code, script_options()).and_then(|value| {
            // Promise jobs (what follows an `await`, `.then()` callbacks)
            // run now, until none are left, under Stop like the script.
            while ctx.execute_pending_job() {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
            }
            if stop.load(Ordering::Relaxed) {
                return Ok(None);
            }
            // A promise as the final value shows what it resolved to; its
            // rejection is the run's error.
            let value = match value.as_promise().filter(|p| p.state() != PromiseState::Pending) {
                Some(p) => p.result::<Value>().unwrap_or(Ok(value.clone()))?,
                None => value,
            };
            let finish: Function = ctx.globals().get("__finish")?;
            finish.call::<_, String>((value,)).map(Some)
        });
        match result {
            Ok(Some(db)) => (db, None),
            Ok(None) => {
                let db = ctx.eval::<String, _>("db._name").unwrap_or_else(|_| job.db.clone());
                (db, Some(ShellError { message: "Interrupted".into(), line: None }))
            }
            Err(e) => {
                let err = stopped(js_error(&ctx, e, offset));
                let db = ctx.eval::<String, _>("db._name").unwrap_or_else(|_| job.db.clone());
                (db, Some(err))
            }
        }
    })
}

/// A rejected promise nobody handles is told to the prelude (`__rejection`),
/// which reports it at the end of the run, as Node does.
fn track_rejections(rt: &Runtime) {
    rt.set_host_promise_rejection_tracker(Some(Box::new(|ctx, promise, reason, handled| {
        if let Ok(track) = ctx.globals().get::<_, Function>("__rejection")
            && track.call::<_, ()>((promise, reason, handled)).is_err()
        {
            ctx.catch();
        }
    })));
}

/// `__native` and the BSON types: everything a detached parse has.
fn install_bson(ctx: &Ctx<'_>, state: Rc<RefCell<State>>) -> rquickjs::Result<()> {
    let native = Function::new(ctx.clone(), move |op: String, args: String| -> String {
        let reply = match serde_json::from_str::<Json>(&args) {
            Ok(mut args) => {
                let st = &mut state.borrow_mut();
                match op.as_str() {
                    // Documents go to JS as text written straight from BSON.
                    "cursor_next" | "find_one" => docs_op(st, &op, &mut args),
                    _ => dispatch(st, &op, &mut args).map(|v| v.to_string()),
                }
            }
            Err(e) => Err(e.to_string().into()),
        };
        match reply {
            Ok(text) => format!("{{\"ok\":{text}}}"),
            Err(e) => error_reply(&e),
        }
    })?;
    ctx.globals().set("__native", native)?;
    ctx.eval_with_options::<(), _>(BSON_JS, prelude_options("<bson>"))
}

fn install(ctx: &Ctx<'_>, state: Rc<RefCell<State>>, db: &str, line_offset: i64) -> rquickjs::Result<()> {
    install_bson(ctx, state)?;
    ctx.eval_with_options::<(), _>(PRELUDE, prelude_options("<prelude>"))?;
    let setup = format!(
        "globalThis.__lineOffset = {line_offset}; globalThis.__maxDocs = {MAX_RESULT_DOCS}; \
         globalThis.db = __dbProxy(new Database({}));",
        serde_json::to_string(db).unwrap_or_else(|_| "\"test\"".into())
    );
    ctx.eval::<(), _>(setup)
}

/// What a detached evaluation may use, on the caller's (the UI) thread: two
/// seconds, and `memory` bytes, so that one runaway native call such as
/// `'x'.repeat(2 ** 29)` fails at once instead of stalling the window.
pub(crate) struct Limits {
    pub memory: usize,
    /// The messages when time or memory runs out.
    pub slow: &'static str,
    pub huge: &'static str,
}

/// Small text typed in: the query bar, the dialogs' fields, Edit Value.
pub(crate) const TYPED: Limits = Limits {
    memory: 64 << 20,
    slow: "the text took longer than 2 seconds to evaluate",
    huge: "the text needs more than 64 MiB of memory to evaluate",
};

/// Shell syntax of many small values takes ten to forty times its size in
/// the engine, so documents of a few MB need more than 64 MiB: the
/// document editor and Paste get more. (Plain JSON pasted doesn't go
/// through the engine at all.)
pub(crate) const EDITED: Limits = Limits {
    memory: 512 << 20,
    slow: "the document took longer than 2 seconds to read",
    huge: "the document needs more than 512 MiB of memory to read",
};

pub(crate) const PASTED: Limits = Limits {
    memory: 512 << 20,
    slow: "the clipboard took longer than 2 seconds to read: paste fewer documents at a time",
    huge: "the clipboard needs more than 512 MiB of memory to read: paste fewer documents at a time",
};

pub(crate) enum DetachedError {
    /// Time or memory ran out: trying again, or in parts, won't help.
    Limit(ShellError),
    Script(ShellError),
}

impl From<DetachedError> for ShellError {
    fn from(e: DetachedError) -> ShellError {
        match e {
            DetachedError::Limit(e) | DetachedError::Script(e) => e,
        }
    }
}

/// Evaluate `text` without a connection, as the elements of one array
/// literal, so `{ a: 1 }, { b: 2 }` is two values rather than (JS's comma
/// operator) the last one. For the document editor, the query bar, the
/// dialogs and Paste. Runs on the caller's thread, within `limits`.
pub(crate) fn eval_detached(text: &str, limits: &Limits) -> Result<Vec<Bson>, DetachedError> {
    let fail = |message: String| DetachedError::Script(ShellError { message, line: None });
    let limit = |message: &str| DetachedError::Limit(ShellError { message: message.into(), line: None });
    let deadline = Instant::now() + Duration::from_secs(2);
    let rt = new_runtime(limits.memory, move || Instant::now() > deadline).map_err(|e| fail(e.to_string()))?;
    let ctx = Context::full(&rt).map_err(|e| fail(e.to_string()))?;
    let state = Rc::new(RefCell::new(State::new(None, Arc::new(AtomicBool::new(false)))));
    let text = text.trim().trim_end_matches(';');
    ctx.with(|ctx| {
        install_bson(&ctx, state).map_err(|e| fail(js_error(&ctx, e, 0).message))?;
        // The text starts on line 2, inside brackets, so `{ a: 1 }` is an
        // object literal rather than a block.
        let code = format!("__ejsonString([\n{text}\n])");
        let out: String = ctx.eval_with_options(code, script_options()).map_err(|e| {
            let err = js_error(&ctx, e, -1);
            if Instant::now() > deadline {
                limit(limits.slow)
            } else if err.message.contains("out of memory") || err.message == "InternalError: " || err.message == "Uncaught null" {
                // (Out of memory, the error may come without its message, or
                // as null when there was no room to make it.)
                limit(limits.huge)
            } else {
                DetachedError::Script(err)
            }
        })?;
        let json: Json = serde_json::from_str(&out).map_err(|e| fail(e.to_string()))?;
        match Bson::try_from(json).map_err(|e| fail(e.to_string()))? {
            Bson::Array(items) => Ok(items),
            other => Err(fail(format!("expected a list, got {}", format::kind(&other).label()))),
        }
    })
}

fn js_error(ctx: &Ctx<'_>, e: rquickjs::Error, offset: i64) -> ShellError {
    if !matches!(e, rquickjs::Error::Exception) {
        return ShellError { message: e.to_string(), line: None };
    }
    let thrown = ctx.catch();
    let (message, stack) = match thrown.as_exception() {
        Some(exc) => {
            let name: String = exc.get("name").unwrap_or_else(|_| "Error".into());
            let msg = exc.message().unwrap_or_default();
            (format!("{name}: {msg}"), exc.stack().unwrap_or_default())
        }
        None => {
            // As JS would write it (`throw null` is "null").
            let text = thrown.get::<rquickjs::Coerced<String>>().map(|s| s.0).unwrap_or_else(|_| {
                ctx.catch();
                format!("{thrown:?}")
            });
            (format!("Uncaught {text}"), String::new())
        }
    };
    let line = shell_line(&stack).and_then(|l| u32::try_from(i64::from(l) + offset).ok()).filter(|l| *l > 0);
    ShellError { message, line }
}

/// The first `<shell>:LINE` in a QuickJS stack trace.
fn shell_line(stack: &str) -> Option<u32> {
    let at = stack.find("<shell>:")? + "<shell>:".len();
    let digits: String = stack[at..].chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

// ---------------------------------------------------------------- dispatch

fn ejson(b: Bson) -> Json {
    b.into_canonical_extjson()
}

/// About how much memory a document takes in Rust, for `HELD_BYTES`.
fn doc_bytes(d: &Document) -> usize {
    d.iter().map(|(k, v)| 32 + k.len() + value_bytes(v)).sum()
}

fn value_bytes(v: &Bson) -> usize {
    match v {
        Bson::Document(d) => doc_bytes(d),
        Bson::Array(items) => items.iter().map(|v| 16 + value_bytes(v)).sum(),
        Bson::String(s) | Bson::Symbol(s) | Bson::JavaScriptCode(s) => s.len(),
        Bson::Binary(b) => b.bytes.len(),
        Bson::JavaScriptCodeWithScope(c) => c.code.len() + doc_bytes(&c.scope),
        Bson::RegularExpression(r) => r.pattern.as_str().len() + r.options.as_str().len(),
        _ => 16,
    }
}

/// A document for the JS side, written straight to JSON text: what
/// `__revive` makes of it is exactly what it makes of canonical Extended
/// JSON, but an Int32 or a finite double is a bare JSON number (JS reads
/// either as the same Number), so most fields need no work in the reviver,
/// and no `serde_json::Value` is built.
fn write_js_doc(out: &mut String, d: &Document) {
    out.push('{');
    for (i, (k, v)) in d.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_json_string(out, k);
        out.push(':');
        write_js(out, v);
    }
    out.push('}');
}

fn write_js(out: &mut String, v: &Bson) {
    use std::fmt::Write;
    match v {
        Bson::Int32(n) => {
            let _ = write!(out, "{n}");
        }
        // (Rust writes the shortest digits that read back as this double;
        // -0.0 as "-0", which JSON.parse reads as -0.)
        Bson::Double(f) if f.is_finite() => {
            let _ = write!(out, "{f}");
        }
        Bson::Int64(n) => {
            let _ = write!(out, "{{\"$numberLong\":\"{n}\"}}");
        }
        Bson::String(s) => write_json_string(out, s),
        Bson::Boolean(b) => out.push_str(if *b { "true" } else { "false" }),
        Bson::Null => out.push_str("null"),
        Bson::Document(d) => write_js_doc(out, d),
        Bson::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_js(out, item);
            }
            out.push(']');
        }
        Bson::ObjectId(id) => {
            let _ = write!(out, "{{\"$oid\":\"{}\"}}", id.to_hex());
        }
        Bson::DateTime(d) => {
            let _ = write!(out, "{{\"$date\":{{\"$numberLong\":\"{}\"}}}}", d.timestamp_millis());
        }
        // NaN and the infinities, binaries and the rarer types, as before.
        other => out.push_str(&other.clone().into_canonical_extjson().to_string()),
    }
}

/// `s` as a JSON string literal.
fn write_json_string(out: &mut String, s: &str) {
    use std::fmt::Write;
    out.push('"');
    let mut plain = 0;
    for (i, b) in s.bytes().enumerate() {
        let escape = match b {
            b'"' => "\\\"",
            b'\\' => "\\\\",
            b'\n' => "\\n",
            b'\r' => "\\r",
            b'\t' => "\\t",
            0..0x20 => "",
            _ => continue,
        };
        out.push_str(&s[plain..i]);
        if escape.is_empty() {
            let _ = write!(out, "\\u{b:04x}");
        } else {
            out.push_str(escape);
        }
        plain = i + 1;
    }
    out.push_str(&s[plain..]);
    out.push('"');
}

/// The session for a call that reaches the database (a detached parse has
/// none), once whatever the script printed has gone out.
fn db_session(st: &mut State) -> Result<Session, Fail> {
    // After Stop, every call fails, so a script that catches the first
    // "interrupted" error can't go on writing.
    if st.interrupt.load(Ordering::Relaxed) {
        return Err("interrupted".into());
    }
    let session = st.session.clone().ok_or("no database here: this text is evaluated without a connection")?;
    // What it printed shows while it waits on the server.
    st.flush();
    Ok(session)
}

/// The calls that hand documents to the script, answered as JSON text.
fn docs_op(st: &mut State, op: &str, a: &mut Json) -> Result<String, Fail> {
    let s = db_session(st)?;
    let stop = st.interrupt.clone();
    let mut out = String::new();
    match op {
        "cursor_next" => {
            let id = a.get("cursor").and_then(Json::as_u64).ok_or("bad cursor id")?;
            let docs = match st.take_held(id) {
                Some(docs) => {
                    if docs.is_empty() {
                        st.cursors.remove(&id);
                    }
                    docs
                }
                None => match st.cursors.remove(&id) {
                    Some(mut cursor) => {
                        let n = a.get("n").and_then(Json::as_u64).unwrap_or(101).clamp(1, 100_000) as usize;
                        let docs = wait(&stop, &s, cursor.next_batch(n))?;
                        if !docs.is_empty() {
                            st.cursors.insert(id, cursor);
                        }
                        docs
                    }
                    None => Vec::new(),
                },
            };
            out.push_str("{\"docs\":[");
            for (i, d) in docs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_js_doc(&mut out, d);
            }
            out.push_str("]}");
        }
        "find_one" => {
            let opts = FindOpts { projection: opt_doc(a, "projection")?, limit: Some(1), ..Default::default() };
            let ns = ns_arg(a)?;
            let filter = doc_arg(a, "filter")?;
            let docs = wait(&stop, &s, async {
                let mut c = s.conn.open_find(&ns, filter, opts).await?;
                c.next_batch(1).await
            })?;
            match docs.first() {
                Some(d) => write_js_doc(&mut out, d),
                None => out.push_str("null"),
            }
        }
        other => return Err(format!("unknown shell operation {other}").into()),
    }
    Ok(out)
}

/// Each value revived in JS from the text `write_js` writes and from its
/// canonical Extended JSON; the first that differs, described (tests).
#[cfg(test)]
pub(crate) fn revive_difference(values: &[Bson]) -> Option<String> {
    let rt = new_runtime(1 << 30, || false).unwrap();
    let ctx = Context::full(&rt).unwrap();
    let state = Rc::new(RefCell::new(State::new(None, Arc::new(AtomicBool::new(false)))));
    ctx.with(|ctx| {
        install_bson(&ctx, state).unwrap();
        let compare: Function = ctx
            .eval(
                r#"(a, b) => {
                    const revive = globalThis.__bson.revive;
                    const same = (x, y, at) => {
                        if (typeof x !== typeof y) return at + ": " + typeof x + " / " + typeof y;
                        if (typeof x === "number") return Object.is(x, y) ? null : at + ": " + x + " / " + y;
                        if (x === null || y === null || typeof x !== "object") return x === y ? null : at + ": " + String(x) + " / " + String(y);
                        if (Object.getPrototypeOf(x) !== Object.getPrototypeOf(y)) return at + ": " + x.constructor.name + " / " + y.constructor.name;
                        if (x instanceof Date) return Object.is(x.getTime(), y.getTime()) ? null : at + ": dates";
                        if (x instanceof RegExp) return x.source === y.source && x.flags === y.flags ? null : at + ": regexes";
                        const kx = Object.keys(x), ky = Object.keys(y);
                        if (kx.join("\u0000") !== ky.join("\u0000")) return at + ": keys " + kx + " / " + ky;
                        for (const k of kx) { const d = same(x[k], y[k], at + "." + k); if (d) return d; }
                        return null;
                    };
                    return same(JSON.parse(a, revive), JSON.parse(b, revive), "value");
                }"#,
            )
            .unwrap();
        values.iter().find_map(|v| {
            let doc = rongo_core::bson::doc! { "v": v.clone() };
            let mut text = String::new();
            write_js_doc(&mut text, &doc);
            let canonical = Bson::Document(doc).into_canonical_extjson().to_string();
            let diff: Option<String> = compare.call((text.clone(), canonical.clone())).unwrap();
            diff.map(|d| format!("{d}\n  written: {text}\n  canonical: {canonical}"))
        })
    })
}

fn docs_json(docs: Vec<Document>) -> Json {
    Json::Array(docs.into_iter().map(|d| ejson(Bson::Document(d))).collect())
}

fn arg(a: &mut Json, key: &str) -> Result<Bson, String> {
    // Taken out of the arguments, not copied: `docs` can be a large array.
    Bson::try_from(a.get_mut(key).map_or(Json::Null, Json::take)).map_err(|e| format!("{key}: {e}"))
}

/// A boolean option: false when absent or not a boolean.
fn flag(a: &Json, key: &str) -> bool {
    a.get(key).and_then(Json::as_bool).unwrap_or(false)
}

fn doc_arg(a: &mut Json, key: &str) -> Result<Document, String> {
    Ok(opt_doc(a, key)?.unwrap_or_default())
}

fn opt_doc(a: &mut Json, key: &str) -> Result<Option<Document>, String> {
    match arg(a, key)? {
        Bson::Null | Bson::Undefined => Ok(None),
        Bson::Document(d) => Ok(Some(d)),
        other => Err(format!("{key} must be an object, not {}", format::kind(&other).label())),
    }
}

fn docs_arg(a: &mut Json, key: &str) -> Result<Vec<Document>, String> {
    docs_in(arg(a, key)?, key)
}

/// `value` (argument `key`, already taken) as an array of documents.
fn docs_in(value: Bson, key: &str) -> Result<Vec<Document>, String> {
    match value {
        Bson::Array(items) => items
            .into_iter()
            .map(|i| match i {
                Bson::Document(d) => Ok(d),
                _ => Err(format!("{key} must contain only objects")),
            })
            .collect(),
        _ => Err(format!("{key} must be an array")),
    }
}

fn num_arg(a: &mut Json, key: &str) -> Result<Option<i64>, String> {
    Ok(match arg(a, key)? {
        Bson::Int32(n) => Some(i64::from(n)),
        Bson::Int64(n) => Some(n),
        Bson::Double(f) if f.fract() == 0.0 => Some(f as i64),
        Bson::Null | Bson::Undefined => None,
        _ => return Err(format!("{key} must be an integer")),
    })
}

fn str_arg<'a>(a: &'a Json, key: &str) -> Result<&'a str, String> {
    a.get(key).and_then(Json::as_str).ok_or_else(|| format!("{key} must be a string"))
}

fn ns_arg(a: &Json) -> Result<Namespace, String> {
    let ns = a.get("ns").ok_or("missing namespace")?;
    Ok(Namespace::new(str_arg(ns, "db")?, str_arg(ns, "coll")?))
}

fn human_size(bytes: u64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < units.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{bytes} B") } else { format!("{v:.2} {}", units[u]) }
}

/// Why a native call failed: thrown into the script as an `Error` with this
/// message, and a server error's (or a write-concern error's) `code` and
/// `codeName`, as mongosh has them.
struct Fail {
    msg: String,
    code: Option<i32>,
}

impl From<String> for Fail {
    fn from(msg: String) -> Fail {
        Fail { msg, code: None }
    }
}

impl From<&str> for Fail {
    fn from(msg: &str) -> Fail {
        msg.to_string().into()
    }
}

impl From<CoreError> for Fail {
    fn from(e: CoreError) -> Fail {
        let code = match &e {
            CoreError::Server { code, .. } | CoreError::WriteConcern { code, .. } => Some(*code),
            _ => None,
        };
        Fail { msg: e.to_string(), code }
    }
}

/// What `__native` answers for a failure: the message, and a server error's
/// code and code name (the prelude puts them on the `Error` it throws).
fn error_reply(e: &Fail) -> String {
    let mut reply = json!({ "err": e.msg });
    if let Some(code) = e.code {
        reply["code"] = json!(code);
        if let Some(name) = code_name(code) {
            reply["codeName"] = json!(name);
        }
    }
    reply.to_string()
}

/// Tests: what `__native` answers when a core call fails with `e`.
#[cfg(test)]
pub(crate) fn core_error_reply(e: CoreError) -> String {
    error_reply(&Fail::from(e))
}

/// The server's name for an error code. The core keeps only the code, so
/// this covers the common ones; others have no `codeName`.
fn code_name(code: i32) -> Option<&'static str> {
    Some(match code {
        1 => "InternalError",
        2 => "BadValue",
        4 => "NoSuchKey",
        6 => "HostUnreachable",
        7 => "HostNotFound",
        8 => "UnknownError",
        9 => "FailedToParse",
        11 => "UserNotFound",
        13 => "Unauthorized",
        14 => "TypeMismatch",
        15 => "Overflow",
        16 => "InvalidLength",
        17 => "ProtocolError",
        18 => "AuthenticationFailed",
        20 => "IllegalOperation",
        22 => "InvalidBSON",
        24 => "LockTimeout",
        26 => "NamespaceNotFound",
        27 => "IndexNotFound",
        28 => "PathNotViable",
        29 => "NonExistentPath",
        30 => "InvalidPath",
        31 => "RoleNotFound",
        40 => "ConflictingUpdateOperators",
        43 => "CursorNotFound",
        48 => "NamespaceExists",
        50 => "MaxTimeMSExpired",
        52 => "DollarPrefixedFieldName",
        53 => "InvalidIdField",
        54 => "NotSingleValueField",
        55 => "InvalidDBRef",
        56 => "EmptyFieldName",
        57 => "DottedFieldName",
        59 => "CommandNotFound",
        61 => "ShardKeyNotFound",
        64 => "WriteConcernFailed",
        66 => "ImmutableField",
        67 => "CannotCreateIndex",
        68 => "IndexAlreadyExists",
        72 => "InvalidOptions",
        73 => "InvalidNamespace",
        85 => "IndexOptionsConflict",
        86 => "IndexKeySpecsConflict",
        89 => "NetworkTimeout",
        91 => "ShutdownInProgress",
        96 => "OperationFailed",
        112 => "WriteConflict",
        115 => "CommandNotSupported",
        116 => "DocTooLargeForCapped",
        117 => "ConflictingOperationInProgress",
        121 => "DocumentValidationFailure",
        125 => "CommandFailed",
        133 => "FailedToSatisfyReadPreference",
        136 => "CappedPositionLost",
        146 => "ExceededMemoryLimit",
        166 => "CommandNotSupportedOnView",
        167 => "OptionNotSupportedOnView",
        168 => "InvalidPipelineOperator",
        171 => "CannotIndexParallelArrays",
        175 => "QueryPlanKilled",
        189 => "PrimarySteppedDown",
        197 => "InvalidIndexSpecificationOption",
        211 => "KeyNotFound",
        215 => "DatabaseDropPending",
        237 => "CursorKilled",
        238 => "NotImplemented",
        241 => "ConversionFailure",
        251 => "NoSuchTransaction",
        262 => "ExceededTimeLimit",
        263 => "OperationNotSupportedInTransaction",
        276 => "IndexBuildAborted",
        292 => "QueryExceededMemoryLimitNoDiskUseAllowed",
        10107 => "NotWritablePrimary",
        10334 => "BSONObjectTooLarge",
        11000 => "DuplicateKey",
        11600 => "InterruptedAtShutdown",
        11601 => "Interrupted",
        11602 => "InterruptedDueToReplStateChange",
        13388 => "StaleConfig",
        13435 => "NotPrimaryNoSecondaryOk",
        13436 => "NotPrimaryOrSecondary",
        14031 => "OutOfDiskSpace",
        _ => return None,
    })
}

/// Run a driver future, giving up when the user presses Stop.
fn wait<T>(interrupt: &Arc<AtomicBool>, s: &Session, fut: impl Future<Output = CoreResult<T>>) -> Result<T, Fail> {
    if interrupt.load(Ordering::Relaxed) {
        return Err("interrupted".into());
    }
    let flag = interrupt.clone();
    s.rt.block_on(async move {
        let stopped = async {
            while !flag.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        tokio::select! {
            r = fut => r.map_err(Fail::from),
            () = stopped => Err("interrupted".into()),
        }
    })
}

fn dispatch(st: &mut State, op: &str, a: &mut Json) -> Result<Json, Fail> {
    // After Stop, every call fails, so a script that catches the first
    // "interrupted" error can't go on writing.
    if st.interrupt.load(Ordering::Relaxed) {
        return Err("interrupted".into());
    }
    match op {
        "new_oid" => Ok(json!(ObjectId::new().to_hex())),
        "new_uuid" => Ok(json!(uuid::Uuid::new_v4().simple().to_string())),
        #[cfg(test)]
        "held_stats" => Ok(json!({ "count": st.held.len(), "bytes": st.held_bytes, "writing_runs": st.writing_runs })),
        "format" => Ok(json!(format::value_to_shell_pretty(&arg(a, "value")?, Style::Display))),
        "print" => {
            let text = str_arg(a, "text")?.to_string();
            st.print(text);
            Ok(Json::Null)
        }
        "sleep" => {
            // The document editor and the dialogs evaluate text without a
            // shell, on the window's thread: waiting there freezes the app.
            if st.session.is_none() {
                return Err("sleep() runs only in IntelliShell".into());
            }
            st.flush();
            let until = Instant::now() + Duration::from_millis(num_arg(a, "ms")?.unwrap_or(0).max(0) as u64);
            while Instant::now() < until {
                if st.interrupt.load(Ordering::Relaxed) {
                    return Err("interrupted".into());
                }
                std::thread::sleep(Duration::from_millis(10).min(until.saturating_duration_since(Instant::now())));
            }
            Ok(Json::Null)
        }
        "result_find" => {
            let spec = FindSpec {
                ns: ns_arg(a)?,
                filter: doc_arg(a, "filter")?,
                sort: opt_doc(a, "sort")?,
                projection: opt_doc(a, "projection")?,
                skip: num_arg(a, "skip")?.unwrap_or(0).max(0) as u64,
                limit: 50,
            };
            let max = num_arg(a, "limit")?.filter(|l| *l != 0).map(i64::unsigned_abs);
            let line = num_arg(a, "line")?.unwrap_or(0) as u32;
            st.emit(|run| ShellEvent::Result { run, item: ResultItem::Find { line, spec, max } });
            Ok(Json::Null)
        }
        "result_docs" => {
            let item = ResultItem::Docs {
                line: num_arg(a, "line")?.unwrap_or(0) as u32,
                title: str_arg(a, "title")?.to_string(),
                docs: docs_arg(a, "docs")?,
                truncated: flag(a, "truncated"),
            };
            st.emit(|run| ShellEvent::Result { run, item });
            Ok(Json::Null)
        }
        _ => {
            let session = db_session(st)?;
            db_op(st, &session, op, a)
        }
    }
}

fn db_op(st: &mut State, s: &Session, op: &str, a: &mut Json) -> Result<Json, Fail> {
    let conn = &s.conn;
    let stop = st.interrupt.clone();
    match op {
        "find_open" => {
            let opts = FindOpts {
                sort: opt_doc(a, "sort")?,
                projection: opt_doc(a, "projection")?,
                skip: num_arg(a, "skip")?.map(|n| n.max(0) as u64),
                limit: num_arg(a, "limit")?,
                batch_size: num_arg(a, "batchSize")?.map(|n| n.clamp(1, 100_000) as u32),
            };
            let cursor = wait(&stop, s, conn.open_find(&ns_arg(a)?, doc_arg(a, "filter")?, opts))?;
            Ok(json!(st.add_cursor(cursor)))
        }
        "agg_open" => {
            // The pipeline runs now, in script order; its first batch waits
            // here, unconverted. `hold` (from `aggregate()` itself): kept
            // only within HELD_MAX/HELD_BYTES, else let go, server cursor and
            // all, to run again if the script reads it or it is shown.
            // Otherwise (that second run) it is read at once, so it is
            // always kept. A pipeline that writes (`$out`/`$merge`, as the
            // read-only guard spells them) runs exactly once: never let go,
            // whatever the cap (a server's first batch for one is empty, so
            // it holds nothing), and never run a second time.
            let (ns, pipeline) = (ns_arg(a)?, docs_arg(a, "pipeline")?);
            let hold = flag(a, "hold");
            let writes = writes_output(&pipeline);
            if writes && !hold {
                return Err("a pipeline with $out or $merge runs only once".into());
            }
            #[cfg(test)]
            let pipeline = echo_writes(st, pipeline);
            let n = num_arg(a, "batchSize")?.map_or(101, |n| n.clamp(1, 100_000) as usize);
            let (cursor, first) = wait(&stop, s, async {
                let mut c = conn.open_aggregate(&ns, pipeline).await?;
                let first = c.next_batch(n).await?;
                Ok((c, first))
            })?;
            if first.is_empty() {
                return Ok(json!({ "id": null, "empty": true }));
            }
            let bytes = first.iter().map(doc_bytes).sum::<usize>();
            if hold && !writes && (st.held.len() >= HELD_MAX || st.held_bytes + bytes > HELD_BYTES) {
                return Ok(json!({ "id": null, "dropped": true }));
            }
            let id = st.add_cursor(cursor);
            st.held.insert(id, (first, bytes));
            st.held_bytes += bytes;
            Ok(json!({ "id": id }))
        }
        "result_cursor" => {
            // An aggregation the script never read: its documents, as the
            // server sent them, up to what a result tab holds.
            let id = a.get("cursor").and_then(Json::as_u64).ok_or("bad cursor id")?;
            let mut docs = st.take_held(id).unwrap_or_default();
            if let Some(mut cursor) = st.cursors.remove(&id) {
                while docs.len() <= MAX_RESULT_DOCS {
                    let more = wait(&stop, s, cursor.next_batch(500))?;
                    if more.is_empty() {
                        break;
                    }
                    docs.extend(more);
                }
            }
            let truncated = docs.len() > MAX_RESULT_DOCS;
            docs.truncate(MAX_RESULT_DOCS);
            let line = num_arg(a, "line")?.unwrap_or(0) as u32;
            let title = str_arg(a, "title")?.to_string();
            st.emit(|run| ShellEvent::Result { run, item: ResultItem::Docs { line, title, docs, truncated } });
            Ok(Json::Null)
        }
        "count" => Ok(json!(wait(&stop, s, conn.count(&ns_arg(a)?, doc_arg(a, "filter")?))?)),
        "estimated_count" => Ok(json!(wait(&stop, s, conn.estimated_count(&ns_arg(a)?))?)),
        "distinct" => {
            let (filter, key) = (doc_arg(a, "filter")?, str_arg(a, "key")?);
            let values = wait(&stop, s, conn.distinct(&ns_arg(a)?, key, filter))?;
            Ok(ejson(Bson::Array(values)))
        }
        "insert_one" => {
            let doc = doc_arg(a, "doc")?;
            Ok(ejson(wait(&stop, s, conn.insert_one(&ns_arg(a)?, doc))?))
        }
        "insert_many" => {
            let docs = docs_arg(a, "docs")?;
            Ok(ejson(Bson::Array(wait(&stop, s, conn.insert_many(&ns_arg(a)?, docs))?)))
        }
        "update" => {
            let update = match arg(a, "update")? {
                Bson::Document(d) => UpdateSpec::Doc(d),
                stages @ Bson::Array(_) => UpdateSpec::Pipeline(docs_in(stages, "update")?),
                _ => return Err("update must be an object or a pipeline".into()),
            };
            let (many, upsert) = (flag(a, "many"), flag(a, "upsert"));
            let r = wait(&stop, s, conn.update(&ns_arg(a)?, doc_arg(a, "filter")?, update, many, upsert))?;
            Ok(write_result(r))
        }
        "replace" => {
            let upsert = flag(a, "upsert");
            let r = wait(&stop, s, conn.replace_one(&ns_arg(a)?, doc_arg(a, "filter")?, doc_arg(a, "doc")?, upsert))?;
            Ok(write_result(r))
        }
        "delete" => {
            let many = flag(a, "many");
            let n = wait(&stop, s, conn.delete(&ns_arg(a)?, doc_arg(a, "filter")?, many))?;
            Ok(json!({ "acknowledged": true, "deletedCount": n }))
        }
        "command" => {
            let cmd = doc_arg(a, "cmd")?;
            let reply = wait(&stop, s, conn.run_command(str_arg(a, "db")?, cmd))?;
            Ok(ejson(Bson::Document(reply)))
        }
        "list_dbs" => {
            let dbs = wait(&stop, s, conn.list_databases())?;
            Ok(Json::Array(dbs.into_iter().map(|d| json!({ "name": d.name, "size": human_size(d.size_on_disk) })).collect()))
        }
        "list_colls" => {
            let colls = wait(&stop, s, conn.list_collections(str_arg(a, "db")?))?;
            Ok(Json::Array(colls.into_iter().map(|c| json!(c.name)).collect()))
        }
        "coll_infos" => {
            let colls = wait(&stop, s, conn.list_collections(str_arg(a, "db")?))?;
            let kind = |k: CollKind| match k {
                CollKind::Collection => "collection",
                CollKind::View => "view",
                CollKind::Timeseries => "timeseries",
            };
            Ok(Json::Array(colls.into_iter().map(|c| json!({ "name": c.name, "type": kind(c.kind) })).collect()))
        }
        "create_index" => {
            // The name, built or already there, as mongosh returns it.
            let index = wait(&stop, s, conn.create_index(&ns_arg(a)?, doc_arg(a, "keys")?, opt_doc(a, "options")?))?;
            Ok(json!(index.name))
        }
        "get_indexes" => Ok(docs_json(wait(&stop, s, conn.list_indexes(&ns_arg(a)?))?)),
        "drop_index" => {
            wait(&stop, s, conn.drop_index(&ns_arg(a)?, str_arg(a, "name")?))?;
            Ok(Json::Null)
        }
        "drop_coll" => {
            wait(&stop, s, conn.drop_collection(&ns_arg(a)?))?;
            Ok(Json::Null)
        }
        "create_coll" => {
            wait(&stop, s, conn.create_collection(&ns_arg(a)?))?;
            Ok(Json::Null)
        }
        "drop_db" => {
            wait(&stop, s, conn.drop_database(str_arg(a, "db")?))?;
            Ok(Json::Null)
        }
        other => Err(format!("unknown shell operation {other}").into()),
    }
}

/// Tests: a last stage `{ $merge: "__echo" }` stands for a writing stage
/// whose first batch has documents (a server's never has): the mock, which
/// has no `$merge`, runs the stages before it, and the run is counted.
#[cfg(test)]
fn echo_writes(st: &mut State, mut pipeline: Vec<Document>) -> Vec<Document> {
    if pipeline.last().is_some_and(|s| s.get_str("$merge").is_ok_and(|t| t == "__echo")) {
        pipeline.pop();
        st.writing_runs += 1;
    }
    pipeline
}

impl State {
    fn add_cursor(&mut self, cursor: DocCursor) -> u64 {
        let id = self.next_cursor;
        self.next_cursor += 1;
        self.cursors.insert(id, cursor);
        id
    }
}

fn write_result(r: rongo_core::WriteOutcome) -> Json {
    json!({
        "acknowledged": true,
        "matchedCount": r.matched,
        "modifiedCount": r.modified,
        "upsertedCount": u64::from(r.inserted_id.is_some()),
        "upsertedId": r.inserted_id.map_or(Json::Null, ejson),
    })
}
