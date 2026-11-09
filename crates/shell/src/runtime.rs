//! The QuickJS side: one runtime per shell thread, a fresh context per run
//! (so re-running a script with `const` declarations works), and the native
//! dispatcher behind `__native(op, json)`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use rongo_core::bson::{Bson, Document, oid::ObjectId};
use rongo_core::conn::{DocCursor, FindOpts, UpdateSpec};
use rongo_core::format::{self, Style};
use rongo_core::{CoreResult, Namespace, Session, Waker};
use rquickjs::context::EvalOptions;
use rquickjs::{Context, Ctx, Function, Runtime, Value};
use serde_json::{Value as Json, json};

use crate::{Job, ResultItem, ShellError, ShellEvent, b64, find_spec, preprocess};

const PRELUDE: &str = include_str!("prelude.js");
const MAX_RESULT_DOCS: usize = 1000;
const SHELL_FILE: &str = "<shell>";

struct Output {
    tx: Sender<ShellEvent>,
    waker: Waker,
    run: u64,
}

struct State {
    session: Option<Session>,
    interrupt: Arc<AtomicBool>,
    cursors: HashMap<u64, DocCursor>,
    next_cursor: u64,
    out: Option<Output>,
}

impl State {
    fn new(session: Option<Session>, interrupt: Arc<AtomicBool>) -> Self {
        Self { session, interrupt, cursors: HashMap::new(), next_cursor: 1, out: None }
    }

    fn emit(&self, make: impl FnOnce(u64) -> ShellEvent) {
        if let Some(o) = &self.out {
            let _ = o.tx.send(make(o.run));
            (o.waker)();
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
    let rt = match new_runtime(interrupt.clone(), None) {
        Ok(rt) => rt,
        Err(e) => {
            for job in jobs.iter() {
                let error = Some(ShellError { message: format!("could not start JavaScript: {e}"), line: None });
                let _ = events.send(ShellEvent::Done { run: job.run, elapsed_ms: 0, db: job.db, error });
                waker();
            }
            return;
        }
    };
    rt.set_max_stack_size(8 << 20);
    let state = Rc::new(RefCell::new(State::new(Some(session), interrupt)));
    for job in jobs.iter() {
        let started = Instant::now();
        state.borrow_mut().out = Some(Output { tx: events.clone(), waker: waker.clone(), run: job.run });
        let (db, error) = run_script(&rt, &state, &job);
        {
            let mut st = state.borrow_mut();
            st.cursors.clear();
            st.out = None;
        }
        rt.run_gc();
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let _ = events.send(ShellEvent::Done { run: job.run, elapsed_ms, db, error });
        waker();
    }
}

/// `deadline` bounds detached evaluations, which run on the UI thread.
fn new_runtime(interrupt: Arc<AtomicBool>, deadline: Option<Instant>) -> rquickjs::Result<Runtime> {
    let rt = Runtime::new()?;
    rt.set_memory_limit(1 << 30);
    rt.set_interrupt_handler(Some(Box::new(move || {
        interrupt.load(Ordering::Relaxed) || deadline.is_some_and(|d| Instant::now() > d)
    })));
    Ok(rt)
}

fn script_options() -> EvalOptions {
    let mut o = EvalOptions::default();
    o.global = true;
    o.strict = false;
    o.filename = Some(SHELL_FILE.into());
    o
}

fn prelude_options() -> EvalOptions {
    let mut o = EvalOptions::default();
    o.filename = Some("<prelude>".into());
    o
}

fn run_script(rt: &Runtime, state: &Rc<RefCell<State>>, job: &Job) -> (String, Option<ShellError>) {
    let ctx = match Context::full(rt) {
        Ok(c) => c,
        Err(e) => return (job.db.clone(), Some(ShellError { message: e.to_string(), line: None })),
    };
    let offset = i64::from(job.line_offset);
    ctx.with(|ctx| {
        if let Err(e) = install(&ctx, state.clone(), &job.db, offset) {
            return (job.db.clone(), Some(js_error(&ctx, e, 0)));
        }
        let code = preprocess(&job.code);
        let result = ctx.eval_with_options::<Value, _>(code, script_options()).and_then(|value| {
            let finish: Function = ctx.globals().get("__finish")?;
            finish.call::<_, String>((value,))
        });
        match result {
            Ok(db) => (db, None),
            Err(e) => {
                let err = js_error(&ctx, e, offset);
                let db = ctx.eval::<String, _>("db._name").unwrap_or_else(|_| job.db.clone());
                (db, Some(err))
            }
        }
    })
}

fn install(ctx: &Ctx<'_>, state: Rc<RefCell<State>>, db: &str, line_offset: i64) -> rquickjs::Result<()> {
    let native = Function::new(ctx.clone(), move |op: String, args: String| -> String {
        let reply = match serde_json::from_str::<Json>(&args) {
            Ok(args) => dispatch(&mut state.borrow_mut(), &op, &args),
            Err(e) => Err(e.to_string()),
        };
        match reply {
            Ok(v) => json!({ "ok": v }).to_string(),
            Err(e) => json!({ "err": e }).to_string(),
        }
    })?;
    ctx.globals().set("__native", native)?;
    ctx.eval_with_options::<(), _>(PRELUDE, prelude_options())?;
    let setup = format!(
        "globalThis.__lineOffset = {line_offset}; globalThis.__maxDocs = {MAX_RESULT_DOCS}; \
         globalThis.db = __dbProxy(new Database({}));",
        serde_json::to_string(db).unwrap_or_else(|_| "\"test\"".into())
    );
    ctx.eval::<(), _>(setup)
}

/// Evaluate one expression without a connection, for the document editor and
/// the query bar. Runs on the caller's thread, bounded to two seconds.
pub(crate) fn eval_detached(text: &str) -> Result<Bson, ShellError> {
    let fail = |message: String| ShellError { message, line: None };
    let deadline = Instant::now() + Duration::from_secs(2);
    let rt = new_runtime(Arc::new(AtomicBool::new(false)), Some(deadline)).map_err(|e| fail(e.to_string()))?;
    let ctx = Context::full(&rt).map_err(|e| fail(e.to_string()))?;
    let state = Rc::new(RefCell::new(State::new(None, Arc::new(AtomicBool::new(false)))));
    let text = text.trim().trim_end_matches(';');
    ctx.with(|ctx| {
        install(&ctx, state, "test", 0).map_err(|e| js_error(&ctx, e, 0))?;
        // The text starts on line 2, inside parentheses, so `{ a: 1 }` is an
        // object literal rather than a block.
        let code = format!("__ejsonString((\n{text}\n))");
        let out: String = ctx.eval_with_options(code, script_options()).map_err(|e| js_error(&ctx, e, -1))?;
        let json: Json = serde_json::from_str(&out).map_err(|e| fail(e.to_string()))?;
        Bson::try_from(json).map_err(|e| fail(e.to_string()))
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
            let text = thrown
                .as_string()
                .and_then(|s| s.to_string().ok())
                .unwrap_or_else(|| format!("{thrown:?}"));
            (format!("Uncaught {text}"), String::new())
        }
    };
    let message = if message.contains("interrupted") { "Interrupted".to_string() } else { message };
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

fn docs_json(docs: Vec<Document>) -> Json {
    Json::Array(docs.into_iter().map(|d| ejson(Bson::Document(d))).collect())
}

fn arg(a: &Json, key: &str) -> Result<Bson, String> {
    Bson::try_from(a.get(key).cloned().unwrap_or(Json::Null)).map_err(|e| format!("{key}: {e}"))
}

fn doc_arg(a: &Json, key: &str) -> Result<Document, String> {
    match arg(a, key)? {
        Bson::Document(d) => Ok(d),
        Bson::Null | Bson::Undefined => Ok(Document::new()),
        other => Err(format!("{key} must be an object, not {}", format::kind(&other).label())),
    }
}

fn opt_doc(a: &Json, key: &str) -> Result<Option<Document>, String> {
    match arg(a, key)? {
        Bson::Null | Bson::Undefined => Ok(None),
        Bson::Document(d) => Ok(Some(d)),
        other => Err(format!("{key} must be an object, not {}", format::kind(&other).label())),
    }
}

fn docs_arg(a: &Json, key: &str) -> Result<Vec<Document>, String> {
    match arg(a, key)? {
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

fn num_arg(a: &Json, key: &str) -> Result<Option<i64>, String> {
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

/// Run a driver future, giving up when the user presses Stop.
fn wait<T>(interrupt: &Arc<AtomicBool>, s: &Session, fut: impl Future<Output = CoreResult<T>>) -> Result<T, String> {
    if interrupt.load(Ordering::Relaxed) {
        return Err("interrupted".to_string());
    }
    let flag = interrupt.clone();
    s.rt.block_on(async move {
        let stopped = async {
            while !flag.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        tokio::select! {
            r = fut => r.map_err(|e| e.to_string()),
            () = stopped => Err("interrupted".to_string()),
        }
    })
}

fn dispatch(st: &mut State, op: &str, a: &Json) -> Result<Json, String> {
    // After Stop, every call fails, so a script that catches the first
    // "interrupted" error can't go on writing.
    if st.interrupt.load(Ordering::Relaxed) {
        return Err("interrupted".into());
    }
    match op {
        "new_oid" => Ok(json!(ObjectId::new().to_hex())),
        "new_uuid" => Ok(json!(uuid::Uuid::new_v4().simple().to_string())),
        "hex_to_base64" => {
            let bytes = b64::from_hex(str_arg(a, "hex")?).ok_or("invalid hex")?;
            Ok(json!(b64::encode(&bytes)))
        }
        "base64_to_hex" => {
            let bytes = b64::decode(str_arg(a, "base64")?).ok_or("invalid base64")?;
            Ok(json!(b64::to_hex(&bytes)))
        }
        "format" => Ok(json!(format::value_to_shell_pretty(&arg(a, "value")?, Style::Display))),
        "print" => {
            let text = str_arg(a, "text")?.to_string();
            st.emit(|run| ShellEvent::Print { run, text });
            Ok(Json::Null)
        }
        "sleep" => {
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
            let spec = find_spec(
                ns_arg(a)?,
                doc_arg(a, "filter")?,
                opt_doc(a, "sort")?,
                opt_doc(a, "projection")?,
                num_arg(a, "skip")?.unwrap_or(0).max(0) as u64,
            );
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
                truncated: a.get("truncated").and_then(Json::as_bool).unwrap_or(false),
            };
            st.emit(|run| ShellEvent::Result { run, item });
            Ok(Json::Null)
        }
        _ => {
            let session = st.session.clone().ok_or("no database here: this text is evaluated without a connection")?;
            db_op(st, &session, op, a)
        }
    }
}

fn db_op(st: &mut State, s: &Session, op: &str, a: &Json) -> Result<Json, String> {
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
            let cursor = wait(&stop, s, conn.open_aggregate(&ns_arg(a)?, docs_arg(a, "pipeline")?))?;
            Ok(json!(st.add_cursor(cursor)))
        }
        "cursor_next" => {
            let id = a.get("cursor").and_then(Json::as_u64).ok_or("bad cursor id")?;
            let n = a.get("n").and_then(Json::as_u64).unwrap_or(101).clamp(1, 100_000) as usize;
            let Some(mut cursor) = st.cursors.remove(&id) else { return Ok(json!({ "docs": [] })) };
            let docs = wait(&stop, s, cursor.next_batch(n))?;
            if !docs.is_empty() {
                st.cursors.insert(id, cursor);
            }
            Ok(json!({ "docs": docs_json(docs) }))
        }
        "find_one" => {
            let opts = FindOpts { projection: opt_doc(a, "projection")?, limit: Some(1), ..Default::default() };
            let ns = ns_arg(a)?;
            let filter = doc_arg(a, "filter")?;
            let docs = wait(&stop, s, async {
                let mut c = conn.open_find(&ns, filter, opts).await?;
                c.next_batch(1).await
            })?;
            Ok(docs.into_iter().next().map_or(Json::Null, |d| ejson(Bson::Document(d))))
        }
        "count" => Ok(json!(wait(&stop, s, conn.count(&ns_arg(a)?, doc_arg(a, "filter")?))?)),
        "estimated_count" => Ok(json!(wait(&stop, s, conn.estimated_count(&ns_arg(a)?))?)),
        "distinct" => {
            let values = wait(&stop, s, conn.distinct(&ns_arg(a)?, str_arg(a, "key")?, doc_arg(a, "filter")?))?;
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
                Bson::Array(_) => UpdateSpec::Pipeline(docs_arg(a, "update")?),
                _ => return Err("update must be an object or a pipeline".into()),
            };
            let many = a.get("many").and_then(Json::as_bool).unwrap_or(false);
            let upsert = a.get("upsert").and_then(Json::as_bool).unwrap_or(false);
            let r = wait(&stop, s, conn.update(&ns_arg(a)?, doc_arg(a, "filter")?, update, many, upsert))?;
            Ok(write_result(r))
        }
        "replace" => {
            let upsert = a.get("upsert").and_then(Json::as_bool).unwrap_or(false);
            let r = wait(&stop, s, conn.replace_one(&ns_arg(a)?, doc_arg(a, "filter")?, doc_arg(a, "doc")?, upsert))?;
            Ok(write_result(r))
        }
        "delete" => {
            let many = a.get("many").and_then(Json::as_bool).unwrap_or(false);
            let n = wait(&stop, s, conn.delete(&ns_arg(a)?, doc_arg(a, "filter")?, many))?;
            Ok(json!({ "acknowledged": true, "deletedCount": n }))
        }
        "command" => {
            let reply = wait(&stop, s, conn.run_command(str_arg(a, "db")?, doc_arg(a, "cmd")?))?;
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
        "create_index" => {
            let name = wait(&stop, s, conn.create_index(&ns_arg(a)?, doc_arg(a, "keys")?, opt_doc(a, "options")?))?;
            Ok(json!(name))
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
        other => Err(format!("unknown shell operation {other}")),
    }
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
        "upsertedId": r.inserted_id.map_or(Json::Null, ejson),
    })
}
