//! The request loop. Each request runs as its own task on the tokio runtime,
//! can be cancelled if it only reads or stops between batches
//! (`Command::cancellable`), and posts exactly one terminal response (preceded, for
//! long requests, by any number of `Event::Progress` responses). The one
//! exception is `SetReadOnly`, which `CoreHandle::send` applies and answers
//! itself so that switches take effect in the order they were sent.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use bson::doc;
use futures_util::FutureExt;
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::api::{Command, ConnId, Event, Request, RequestId, Response, WriteOutcome};
use crate::conn::{Conn, UpdateSpec};
use crate::error::{CoreError, CoreResult};

/// Called after every response so the UI can repaint.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct Registry {
    conns: Mutex<HashMap<ConnId, Conn>>,
    running: Mutex<HashMap<RequestId, CancellationToken>>,
}

impl Registry {
    fn conn(&self, id: Option<ConnId>) -> CoreResult<Conn> {
        let id = id.ok_or(CoreError::NotConnected)?;
        self.conns.lock().expect("conns").get(&id).cloned().ok_or(CoreError::NotConnected)
    }
}

/// The least time between two `Event::Progress` responses of one request.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

/// A running request's way of telling the UI how far it has got.
pub(crate) struct Progress {
    id: RequestId,
    conn: Option<ConnId>,
    tx: crossbeam_channel::Sender<Response>,
    waker: Waker,
    last: Mutex<Last>,
    /// Called with `done` on every report, unthrottled, so tests can act at
    /// an exact point of a long command.
    #[cfg(test)]
    hook: Option<Box<dyn Fn(u64) + Send + Sync>>,
}

/// When a request last reported, and on which of a copy's items; and the
/// item its reports are about now.
#[derive(Default)]
struct Last {
    at: Option<Instant>,
    item: Option<usize>,
    current: Option<usize>,
}

impl Progress {
    fn new(id: RequestId, conn: Option<ConnId>, tx: crossbeam_channel::Sender<Response>, waker: Waker) -> Progress {
        Progress {
            id,
            conn,
            tx,
            waker,
            last: Mutex::new(Last::default()),
            #[cfg(test)]
            hook: None,
        }
    }

    /// Report `done` of `total` (if known) units, with a short note on what is
    /// being worked on. Sent at most every 100 ms so a tight loop can call it
    /// per document, except that reaching the total is always sent, and so
    /// is the first report on a copy's next item (`item`).
    pub(crate) fn report(&self, done: u64, total: Option<u64>, note: impl Into<String>) {
        #[cfg(test)]
        if let Some(hook) = &self.hook {
            hook(done);
        }
        let now = Instant::now();
        let item = {
            let mut last = self.last.lock().expect("progress");
            let finished = Some(done) == total;
            let next_item = last.current != last.item;
            if !finished && !next_item && last.at.is_some_and(|t| now.saturating_duration_since(t) < PROGRESS_EVERY) {
                return;
            }
            last.at = Some(now);
            last.item = last.current;
            last.current
        };
        let _ = self.tx.send(Response { id: self.id, conn: self.conn, event: Event::Progress { done, total, note: note.into(), item } });
        (self.waker)();
    }

    /// A copy: the reports that follow are about its item `k` (its index in
    /// the request). Reports nothing itself.
    pub(crate) fn item(&self, k: usize) {
        self.last.lock().expect("progress").current = Some(k);
    }

    /// The last report of work whose total was an estimate, or unknown: says
    /// `done` of `done`, unless a report already reached the total.
    pub(crate) fn finish(&self, done: u64, reported_total: bool, note: impl Into<String>) {
        if !reported_total {
            self.report(done, Some(done), note);
        }
    }

    /// A sink wired to a fresh channel, for unit tests of long commands.
    #[cfg(test)]
    pub(crate) fn channel() -> (Progress, crossbeam_channel::Receiver<Response>) {
        let (tx, rx) = crossbeam_channel::unbounded();
        (Progress::new(0, None, tx, Arc::new(|| {})), rx)
    }

    /// A sink that calls `f(done)` synchronously on every report (its events
    /// go nowhere), for tests that change things at an exact point of a copy.
    #[cfg(test)]
    pub(crate) fn with_hook(f: impl Fn(u64) + Send + Sync + 'static) -> Progress {
        let (tx, _) = crossbeam_channel::unbounded();
        Progress { hook: Some(Box::new(f)), ..Progress::new(0, None, tx, Arc::new(|| {})) }
    }
}

/// A connection the shell can use directly, outside the request loop.
#[derive(Clone)]
pub struct Session {
    pub conn: Conn,
    pub rt: Handle,
}

pub struct CoreHandle {
    tx: mpsc::UnboundedSender<Request>,
    rx: crossbeam_channel::Receiver<Response>,
    /// For answers given straight from `send` (`SetReadOnly`).
    resp_tx: crossbeam_channel::Sender<Response>,
    waker: Waker,
    registry: Arc<Registry>,
    rt: Handle,
    next_id: AtomicU64,
}

impl CoreHandle {
    pub fn start(rt: Handle, waker: Waker) -> CoreHandle {
        let (tx, mut req_rx) = mpsc::unbounded_channel::<Request>();
        let (resp_tx, rx) = crossbeam_channel::unbounded::<Response>();
        let registry = Arc::new(Registry::default());
        let reg = registry.clone();
        let (loop_tx, loop_waker) = (resp_tx.clone(), waker.clone());
        rt.spawn(async move {
            while let Some(req) = req_rx.recv().await {
                let reg = reg.clone();
                let resp_tx = loop_tx.clone();
                let waker = loop_waker.clone();
                tokio::spawn(async move {
                    let (id, conn) = (req.id, req.conn);
                    let progress = Progress::new(id, conn, resp_tx.clone(), waker.clone());
                    let event = handle(&reg, req, &progress).await;
                    let _ = resp_tx.send(Response { id, conn, event });
                    waker();
                });
            }
        });
        CoreHandle { tx, rx, resp_tx, waker, registry, rt, next_id: AtomicU64::new(1) }
    }

    pub fn send(&self, conn: Option<ConnId>, cmd: Command) -> RequestId {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if let Command::SetReadOnly(on) = cmd {
            // Applied here, not in a task of its own, so switches take effect
            // in the order they were sent: unlocking and quickly locking again
            // always ends locked. The answer still arrives through `try_recv`.
            let event = match self.registry.conn(conn) {
                Ok(c) => {
                    c.set_read_only(on);
                    Event::Done
                }
                Err(e) => Event::Failed(e),
            };
            let _ = self.resp_tx.send(Response { id, conn, event });
            (self.waker)();
            return id;
        }
        if !matches!(cmd, Command::Cancel(_)) {
            self.registry.running.lock().expect("running").insert(id, CancellationToken::new());
        }
        let _ = self.tx.send(Request { id, conn, cmd });
        id
    }

    pub fn try_recv(&self) -> Option<Response> {
        self.rx.try_recv().ok()
    }

    pub fn session(&self, conn: ConnId) -> Option<Session> {
        let conn = self.registry.conns.lock().expect("conns").get(&conn).cloned()?;
        Some(Session { conn, rt: self.rt.clone() })
    }

    pub fn runtime(&self) -> &Handle {
        &self.rt
    }
}

/// Runs one request to its terminal event. Only a read races its cancel
/// token and is dropped when it fires. A copy, an import, an export, a
/// recorded write and a restore watch the token themselves between batches,
/// so a cancel never drops them mid-batch and their answer can say exactly
/// what was written (and a recorded write never writes what it has not
/// saved). Any other write runs to completion, so its answer is what the
/// server did. A panic inside the command answers `Failed` like an error.
async fn handle(reg: &Registry, req: Request, progress: &Progress) -> Event {
    let id = req.id;
    let token = reg.running.lock().expect("running").get(&id).cloned().unwrap_or_default();
    let reads = req.cmd.only_reads();
    let work = async {
        if reads {
            tokio::select! {
                r = run(reg, req.conn, req.cmd, progress, &token) => r,
                () = token.cancelled() => Err(CoreError::Cancelled),
            }
        } else {
            run(reg, req.conn, req.cmd, progress, &token).await
        }
    };
    let result = match AssertUnwindSafe(work).catch_unwind().await {
        Ok(r) => r,
        Err(panic) => {
            let why = panic.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| panic.downcast_ref::<String>().cloned()).unwrap_or_default();
            Err(CoreError::Other(format!("internal error: {why}")))
        }
    };
    reg.running.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
    result.unwrap_or_else(Event::Failed)
}

async fn run(reg: &Registry, conn_id: Option<ConnId>, cmd: Command, progress: &Progress, cancel: &CancellationToken) -> CoreResult<Event> {
    match cmd {
        Command::Connect(req) => {
            let id = req.profile.conn_id();
            let conn = Conn::open(&req).await?;
            // Opening a client is lazy; this is the first real round trip, so
            // bad hosts or credentials fail here.
            let info = conn.server_info().await?;
            reg.conns.lock().expect("conns").insert(id, conn);
            Ok(Event::Connected(info))
        }
        Command::Disconnect => {
            if let Some(id) = conn_id {
                reg.conns.lock().expect("conns").remove(&id);
            }
            Ok(Event::Disconnected)
        }
        // `CoreHandle::send` applies and answers this itself, in send order.
        Command::SetReadOnly(on) => {
            reg.conn(conn_id)?.set_read_only(on);
            Ok(Event::Done)
        }
        Command::Cancel(target) => {
            if let Some(t) = reg.running.lock().expect("running").get(&target) {
                t.cancel();
            }
            Ok(Event::Done)
        }
        Command::ListDatabases => Ok(Event::Databases(reg.conn(conn_id)?.list_databases().await?)),
        Command::ListCollections { db } => {
            let items = reg.conn(conn_id)?.list_collections(&db).await?;
            Ok(Event::Collections { db, items })
        }
        Command::ListIndexes { ns } => {
            let items = reg.conn(conn_id)?.list_indexes(&ns).await?;
            Ok(Event::Indexes { ns, items })
        }
        Command::Find(spec) => {
            let started = Instant::now();
            let (docs, has_more) = reg.conn(conn_id)?.find_page(&spec).await?;
            Ok(Event::Page { docs, has_more, elapsed_ms: started.elapsed().as_millis() as u64 })
        }
        Command::Count { ns, filter } => {
            let conn = reg.conn(conn_id)?;
            // Without a filter, collection metadata answers instantly; an exact
            // count would scan the whole collection.
            let n = if filter.is_empty() { conn.estimated_count(&ns).await? } else { conn.count(&ns, filter).await? };
            Ok(Event::Count(n))
        }
        Command::Insert { ns, doc } => {
            let id = reg.conn(conn_id)?.insert_one(&ns, doc).await?;
            Ok(Event::Written(WriteOutcome { inserted_id: Some(id), inserted: 1, ..Default::default() }))
        }
        Command::Replace { ns, id, doc } => {
            let out = reg.conn(conn_id)?.replace_one(&ns, doc! { "_id": id }, doc, false).await?;
            if out.matched == 0 {
                return Err(CoreError::Other("the document no longer exists".into()));
            }
            Ok(Event::Written(out))
        }
        Command::Delete { ns, ids } => {
            let deleted = reg.conn(conn_id)?.delete(&ns, doc! { "_id": { "$in": ids } }, true).await?;
            Ok(Event::Written(WriteOutcome { deleted, ..Default::default() }))
        }
        Command::CreateCollection { ns } => {
            reg.conn(conn_id)?.create_collection(&ns).await?;
            Ok(Event::Done)
        }
        Command::DropCollection { ns } => {
            reg.conn(conn_id)?.drop_collection(&ns).await?;
            Ok(Event::Done)
        }
        Command::DropDatabase { db } => {
            reg.conn(conn_id)?.drop_database(&db).await?;
            Ok(Event::Done)
        }
        Command::InsertMany { ns, docs } => {
            if docs.is_empty() {
                return Err(CoreError::InvalidInput("nothing to insert".into()));
            }
            let ids = reg.conn(conn_id)?.insert_many(&ns, docs).await?;
            Ok(Event::Written(WriteOutcome { inserted: ids.len() as u64, ..Default::default() }))
        }
        Command::UpdateMany { ns, filter, update } => {
            let out = reg.conn(conn_id)?.update(&ns, filter, UpdateSpec::Doc(update), true, false).await?;
            Ok(Event::Written(out))
        }
        Command::Update { ns, filter, update, many, upsert } => {
            let spec = match update {
                crate::api::UpdateBody::Doc(d) => UpdateSpec::Doc(d),
                crate::api::UpdateBody::Pipeline(p) => UpdateSpec::Pipeline(p),
            };
            Ok(Event::Written(reg.conn(conn_id)?.update(&ns, filter, spec, many, upsert).await?))
        }
        Command::DeleteWhere { ns, filter, many } => {
            let deleted = reg.conn(conn_id)?.delete(&ns, filter, many).await?;
            Ok(Event::Written(WriteOutcome { deleted, ..Default::default() }))
        }
        Command::ClearCollection { ns } => {
            let deleted = reg.conn(conn_id)?.delete(&ns, doc! {}, true).await?;
            Ok(Event::Written(WriteOutcome { deleted, ..Default::default() }))
        }
        Command::RenameCollection { ns, to } => {
            check_collection_name(&to)?;
            reg.conn(conn_id)?.rename_collection(&ns, &to).await?;
            Ok(Event::Done)
        }
        Command::DuplicateCollection { ns, to } => {
            check_collection_name(&to)?;
            reg.conn(conn_id)?.duplicate_collection(&ns, &to, progress, cancel).await?;
            Ok(Event::Done)
        }
        // `existingIndex`: an identical index was already there, nothing was built.
        Command::CreateIndex { ns, keys, options } => {
            let index = reg.conn(conn_id)?.create_index(&ns, keys, options).await?;
            Ok(Event::Document(if index.created { doc! { "createdIndex": index.name } } else { doc! { "existingIndex": index.name } }))
        }
        Command::DropIndex { ns, name } => {
            reg.conn(conn_id)?.drop_index(&ns, &name).await?;
            Ok(Event::Done)
        }
        Command::GetValidator { ns } => Ok(Event::Document(reg.conn(conn_id)?.validator(&ns).await?)),
        Command::SetValidator { ns, validator, level, action } => {
            reg.conn(conn_id)?.set_validator(&ns, validator, &level, &action).await?;
            Ok(Event::Done)
        }
        Command::CollStats { ns } => Ok(Event::Document(reg.conn(conn_id)?.coll_stats(&ns).await?)),
        Command::IndexDetails { ns } => {
            let items = reg.conn(conn_id)?.index_details(&ns).await?;
            Ok(Event::IndexDetails { ns, items })
        }
        Command::ModifyIndex { ns, name, hidden, expire_after_seconds } => {
            reg.conn(conn_id)?.modify_index(&ns, &name, hidden, expire_after_seconds).await?;
            Ok(Event::Done)
        }
        Command::RunCommand { db, cmd } => Ok(Event::Document(reg.conn(conn_id)?.run_command(&db, cmd).await?)),
        Command::Export(spec) => {
            let count = crate::transfer::export(&reg.conn(conn_id)?, &spec, progress, cancel).await?;
            Ok(Event::Exported { count, path: spec.path.clone() })
        }
        Command::Import { ns, path, format } => Ok(Event::Written(crate::transfer::import(&reg.conn(conn_id)?, &ns, &path, format, progress, cancel).await?)),
        Command::CopyCollections { to, items, options } => {
            let (src, dst) = (reg.conn(conn_id)?, reg.conn(Some(to))?);
            let report = crate::copy::copy_collections(&src, &dst, conn_id == Some(to), &items, options, progress, cancel).await?;
            Ok(Event::Copied(report))
        }
        Command::Recorded { record, cmd } => {
            let outcome = crate::recorded::run(&reg.conn(conn_id)?, &record, *cmd, progress, cancel).await?;
            Ok(Event::Written(outcome))
        }
        Command::HistoryConflicts { record, entry } => {
            let c = crate::restore::conflicts(&reg.conn(conn_id)?, &record, &entry, progress).await?;
            Ok(Event::HistoryConflicts { total: c.total, conflicts: c.conflicts, sample: c.sample })
        }
        Command::HistoryRestore { record, entry, on_conflict } => {
            let r = crate::restore::restore(&reg.conn(conn_id)?, &record, &entry, on_conflict, progress, cancel).await?;
            Ok(Event::Restored { restored: r.restored, skipped: r.skipped, overwritten: r.overwritten, inserted_new: r.inserted_new, recorded: r.recorded, cancelled: r.cancelled, warning: r.warning, unconfirmed: r.unconfirmed, unconfirmed_docs: r.unconfirmed_docs })
        }
    }
}

pub(crate) fn check_collection_name(name: &str) -> CoreResult<()> {
    if crate::names::bad_collection_name(name) {
        return Err(CoreError::InvalidInput(format!("'{name}' is not a valid collection name")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::api::{ConnectRequest, FindSpec, Namespace};
    use crate::profile::ConnectionProfile;
    use crate::testkit::TempDir;

    /// The request's terminal response; progress events are skipped.
    fn wait(core: &CoreHandle, id: RequestId) -> Response {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(r) = core.try_recv() {
                if r.id == id && !matches!(r.event, Event::Progress { .. }) {
                    return r;
                }
                continue;
            }
            assert!(Instant::now() < deadline, "no response for {id}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn progress_is_throttled_but_ends_exact() {
        let (p, rx) = Progress::channel();
        for i in 0..=5000 {
            p.report(i, Some(5000), "x");
        }
        let events: Vec<Event> = rx.try_iter().map(|r| r.event).collect();
        assert!(events.len() < 10, "{} events", events.len());
        assert!(matches!(events.last(), Some(Event::Progress { done: 5000, total: Some(5000), .. })), "{:?}", events.last());
        // Only a copy names an item.
        assert!(events.iter().all(|e| matches!(e, Event::Progress { item: None, .. })), "{events:?}");
    }

    #[test]
    fn connect_browse_and_cancel_against_mock() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let core = CoreHandle::start(rt.handle().clone(), Arc::new(|| {}));
        let profile = ConnectionProfile::mock();
        let conn = Some(profile.conn_id());

        let id = core.send(None, Command::Connect(Box::new(ConnectRequest { profile, password: None, ssh_secret: None })));
        assert!(matches!(wait(&core, id).event, Event::Connected(_)));

        let id = core.send(conn, Command::ListDatabases);
        let Event::Databases(dbs) = wait(&core, id).event else { panic!() };
        assert!(dbs.iter().any(|d| d.name == "demo-games"));

        let spec = FindSpec::all(Namespace::new("demo-games", "bet_logs"), 50);
        let id = core.send(conn, Command::Find(spec.clone()));
        let Event::Page { docs, has_more, .. } = wait(&core, id).event else { panic!() };
        assert_eq!((docs.len(), has_more), (50, true));

        let id = core.send(conn, Command::Find(spec));
        core.send(None, Command::Cancel(id));
        let ev = wait(&core, id).event;
        assert!(matches!(ev, Event::Failed(CoreError::Cancelled) | Event::Page { .. }), "{ev:?}");

        let id = core.send(None, Command::ListDatabases);
        assert!(matches!(wait(&core, id).event, Event::Failed(CoreError::NotConnected)));
        assert!(conn.and_then(|c| core.session(c)).is_some());
    }

    #[test]
    fn set_read_only_locks_and_unlocks_writes() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let core = CoreHandle::start(rt.handle().clone(), Arc::new(|| {}));
        let profile = ConnectionProfile::mock();
        let conn = Some(profile.conn_id());
        let id = core.send(None, Command::Connect(Box::new(ConnectRequest { profile, password: None, ssh_secret: None })));
        assert!(matches!(wait(&core, id).event, Event::Connected(_)));

        let insert = || Command::Insert { ns: Namespace::new("demo-games", "advertisers"), doc: bson::doc! { "x": 1 } };
        let id = core.send(conn, Command::SetReadOnly(true));
        assert!(matches!(wait(&core, id).event, Event::Done));
        let id = core.send(conn, insert());
        assert!(matches!(wait(&core, id).event, Event::Failed(CoreError::ReadOnly(_))));
        let id = core.send(conn, Command::SetReadOnly(false));
        assert!(matches!(wait(&core, id).event, Event::Done));
        let id = core.send(conn, insert());
        assert!(matches!(wait(&core, id).event, Event::Written(_)), "insert after unlock");
    }

    #[test]
    fn read_only_switches_apply_in_send_order() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let core = CoreHandle::start(rt.handle().clone(), Arc::new(|| {}));
        let profile = ConnectionProfile::mock();
        let conn = Some(profile.conn_id());
        let id = core.send(None, Command::Connect(Box::new(ConnectRequest { profile, password: None, ssh_secret: None })));
        assert!(matches!(wait(&core, id).event, Event::Connected(_)));

        for round in 0..100 {
            let mut left = vec![core.send(conn, Command::SetReadOnly(false)), core.send(conn, Command::SetReadOnly(true))];
            let deadline = Instant::now() + Duration::from_secs(60);
            while !left.is_empty() {
                match core.try_recv() {
                    Some(r) => {
                        if let Some(i) = left.iter().position(|id| *id == r.id) {
                            assert!(matches!(r.event, Event::Done), "{:?}", r.event);
                            left.remove(i);
                        }
                    }
                    None => {
                        assert!(Instant::now() < deadline, "no answer");
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            }
            let locked = conn.and_then(|c| core.session(c)).unwrap().conn.is_read_only();
            assert!(locked, "round {round}: unlock then lock ended unlocked");
        }

        let id = core.send(None, Command::SetReadOnly(true));
        assert!(matches!(wait(&core, id).event, Event::Failed(CoreError::NotConnected)));
    }

    /// Two demo connections, `a` (the mock profile) and `b`.
    fn two_mock_connections() -> (tokio::runtime::Runtime, CoreHandle, ConnId, ConnId) {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let core = CoreHandle::start(rt.handle().clone(), Arc::new(|| {}));
        let a = ConnectionProfile::mock();
        let mut b = ConnectionProfile::mock();
        b.id = uuid::Uuid::new_v4();
        let ids = (a.conn_id(), b.conn_id());
        for profile in [a, b] {
            let id = core.send(None, Command::Connect(Box::new(ConnectRequest { profile, password: None, ssh_secret: None })));
            assert!(matches!(wait(&core, id).event, Event::Connected(_)));
        }
        (rt, core, ids.0, ids.1)
    }

    fn copy_item(src: Namespace, dst: Namespace) -> crate::api::CopyItem {
        crate::api::CopyItem { src, dst, if_exists: crate::api::IfExists::Append(Default::default()) }
    }

    #[test]
    fn copy_collections_between_connections() {
        let (_rt, core, a, b) = two_mock_connections();
        let items = vec![copy_item(Namespace::new("demo-games", "providers"), Namespace::new("copy", "providers"))];
        let id = core.send(Some(a), Command::CopyCollections { to: b, items, options: Default::default() });
        let mut progress = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(60);
        let report = loop {
            match core.try_recv() {
                Some(Response { id: rid, event: Event::Progress { done, total, .. }, .. }) if rid == id => progress.push((done, total)),
                Some(Response { id: rid, event, .. }) if rid == id => match event {
                    Event::Copied(report) => break report,
                    other => panic!("{other:?}"),
                },
                Some(_) => {}
                None => {
                    assert!(Instant::now() < deadline, "no answer");
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        };
        assert!(!report.cancelled);
        assert_eq!((report.items[0].inserted, report.items[0].error.as_deref()), (8, None));
        assert_eq!(progress.last(), Some(&(8, Some(8))));
        let id = core.send(Some(b), Command::Count { ns: Namespace::new("copy", "providers"), filter: doc! {} });
        assert!(matches!(wait(&core, id).event, Event::Count(8)));
    }

    /// A registry with two connected demo servers, driven through `handle`
    /// directly so a test can cancel from inside the operation.
    async fn connected_registry() -> (Registry, ConnId, ConnId) {
        let reg = Registry::default();
        let a = ConnectionProfile::mock();
        let mut b = ConnectionProfile::mock();
        b.id = uuid::Uuid::new_v4();
        let ids = (a.conn_id(), b.conn_id());
        for profile in [a, b] {
            let req = Request { id: 0, conn: None, cmd: Command::Connect(Box::new(ConnectRequest { profile, password: None, ssh_secret: None })) };
            assert!(matches!(handle(&reg, req, &Progress::channel().0).await, Event::Connected(_)));
        }
        (reg, ids.0, ids.1)
    }

    /// Runs `cmd` as request 1000 through the engine's `handle`, with its
    /// token cancelled the moment the operation reports its first batch. No
    /// wall-clock race: the cancel lands at the same point on any machine.
    async fn run_cancelled_after_first_batch(reg: &Registry, conn: ConnId, cmd: Command) -> Event {
        let token = CancellationToken::new();
        reg.running.lock().unwrap().insert(1000, token.clone());
        let progress = Progress::with_hook(move |done| {
            if done > 0 {
                token.cancel();
            }
        });
        handle(reg, Request { id: 1000, conn: Some(conn), cmd }, &progress).await
    }

    async fn count(reg: &Registry, conn: ConnId, ns: Namespace) -> u64 {
        let req = Request { id: 0, conn: Some(conn), cmd: Command::Count { ns, filter: doc! {} } };
        let Event::Count(n) = handle(reg, req, &Progress::channel().0).await else { panic!() };
        n
    }

    /// Runs `cmd` through `handle` with its token already cancelled.
    async fn run_cancelled(reg: &Registry, conn: ConnId, cmd: Command) -> Event {
        let token = CancellationToken::new();
        token.cancel();
        reg.running.lock().unwrap().insert(2000, token);
        handle(reg, Request { id: 2000, conn: Some(conn), cmd }, &Progress::channel().0).await
    }

    /// A write that a cancel can't stop on the server runs to completion
    /// and answers what it did: never "cancelled" while the server carries
    /// on (a cleared collection went on being deleted after "cancelled").
    #[tokio::test]
    async fn cancelling_a_write_lets_it_finish() {
        let (reg, a, _) = connected_registry().await;
        let ns = |c: &str| Namespace::new("demo-games", c);
        let event = run_cancelled(&reg, a, Command::ClearCollection { ns: ns("bet_logs") }).await;
        assert!(matches!(event, Event::Written(ref w) if w.deleted == 25_000 && !w.cancelled), "{event:?}");
        assert_eq!(count(&reg, a, ns("bet_logs")).await, 0);
        let event = run_cancelled(&reg, a, Command::RenameCollection { ns: ns("providers"), to: "providers2".into() }).await;
        assert!(matches!(event, Event::Done), "{event:?}");
        assert_eq!(count(&reg, a, ns("providers2")).await, 8);
        let event = run_cancelled(&reg, a, Command::CreateIndex { ns: ns("users"), keys: doc! { "email": 1 }, options: None }).await;
        assert!(matches!(event, Event::Document(_)), "{event:?}");
        let event = run_cancelled(&reg, a, Command::InsertMany { ns: ns("fresh"), docs: vec![doc! { "a": 1 }, doc! { "a": 2 }] }).await;
        assert!(matches!(event, Event::Written(ref w) if w.inserted == 2), "{event:?}");
        let event = run_cancelled(&reg, a, Command::DropCollection { ns: ns("fresh") }).await;
        assert!(matches!(event, Event::Done), "{event:?}");
        assert!(reg.running.lock().unwrap().is_empty(), "each request is forgotten once answered");
    }

    /// The collection names of a database, as the engine lists them.
    async fn names(reg: &Registry, conn: ConnId, db: &str) -> Vec<String> {
        let req = Request { id: 0, conn: Some(conn), cmd: Command::ListCollections { db: db.into() } };
        let Event::Collections { items, .. } = handle(reg, req, &Progress::channel().0).await else { panic!() };
        items.into_iter().map(|c| c.name).collect()
    }

    /// Duplicate streams its documents in batches: it reports each one, and
    /// a cancel stops it between batches and drops the partial copy, so
    /// "cancelled" leaves nothing behind.
    #[tokio::test]
    async fn duplicate_reports_progress_and_stops_between_batches() {
        let (reg, a, _) = connected_registry().await;
        let ns = |c: &str| Namespace::new("demo-games", c);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let progress = Progress::with_hook(move |done| log.lock().unwrap().push(done));
        let req = Request { id: 0, conn: Some(a), cmd: Command::DuplicateCollection { ns: ns("bet_logs"), to: "bets_copy".into() } };
        assert!(matches!(handle(&reg, req, &progress).await, Event::Done));
        assert_eq!(*seen.lock().unwrap(), (1..=25).map(|k| k * 1000).collect::<Vec<u64>>());
        assert_eq!(count(&reg, a, ns("bets_copy")).await, 25_000);

        let event = run_cancelled_after_first_batch(&reg, a, Command::DuplicateCollection { ns: ns("bet_logs"), to: "bets_stopped".into() }).await;
        assert!(matches!(event, Event::Failed(CoreError::Cancelled)), "{event:?}");
        assert!(!names(&reg, a, "demo-games").await.contains(&"bets_stopped".to_string()), "the partial copy was dropped");
        assert!(reg.running.lock().unwrap().is_empty());
    }

    /// A duplicate that fails part way drops its partial copy; when that
    /// drop fails too (the connection was locked meanwhile), the error says
    /// the copy remains, names it and how many documents it holds.
    #[tokio::test]
    async fn a_failed_duplicate_says_when_its_partial_copy_remains() {
        let (reg, a, _) = connected_registry().await;
        let conn = reg.conn(Some(a)).unwrap();
        let lock = conn.clone();
        let progress = Progress::with_hook(move |done| {
            if done > 0 {
                lock.set_read_only(true);
            }
        });
        let req = Request { id: 0, conn: Some(a), cmd: Command::DuplicateCollection { ns: Namespace::new("demo-games", "bet_logs"), to: "bets_half".into() } };
        let Event::Failed(e) = handle(&reg, req, &progress).await else { panic!("failed") };
        let m = e.to_string();
        assert!(m.contains("read-only") && m.contains("demo-games.bets_half") && m.contains("remains") && m.contains("1000 documents"), "{m}");
        conn.set_read_only(false);
        assert_eq!(count(&reg, a, Namespace::new("demo-games", "bets_half")).await, 1000);
    }

    /// A read is dropped mid-flight: it changes nothing.
    #[tokio::test]
    async fn cancelling_a_read_drops_it() {
        let (reg, a, _) = connected_registry().await;
        let ns = Namespace::new("demo-games", "bet_logs");
        for cmd in [
            Command::Find(FindSpec::all(ns.clone(), 50)),
            Command::Count { ns: ns.clone(), filter: doc! { "status": "pending" } },
            Command::ListCollections { db: "demo-games".into() },
            Command::RunCommand { db: "demo-games".into(), cmd: doc! { "dbStats": 1 } },
        ] {
            let event = run_cancelled(&reg, a, cmd.clone()).await;
            assert!(matches!(event, Event::Failed(CoreError::Cancelled)), "{cmd:?}: {event:?}");
        }
    }

    /// A panic inside a command answers `Failed` and forgets the request,
    /// rather than leaving the UI waiting for an answer that never comes.
    #[tokio::test]
    async fn a_panic_answers_failed_and_forgets_the_request() {
        let (reg, a, b) = connected_registry().await;
        reg.running.lock().unwrap().insert(3000, CancellationToken::new());
        let progress = Progress::with_hook(|_| panic!("boom"));
        let items = vec![copy_item(Namespace::new("demo-games", "providers"), Namespace::new("copy", "providers"))];
        let event = handle(&reg, Request { id: 3000, conn: Some(a), cmd: Command::CopyCollections { to: b, items, options: Default::default() } }, &progress).await;
        assert!(matches!(event, Event::Failed(CoreError::Other(ref m)) if m.starts_with("internal error") && m.contains("boom")), "{event:?}");
        assert!(reg.running.lock().unwrap().is_empty(), "forgotten");
        // A page as large as the type allows asks for one more than that.
        let event = handle(&reg, Request { id: 0, conn: Some(a), cmd: Command::Find(FindSpec::all(Namespace::new("demo-games", "providers"), i64::MAX)) }, &Progress::channel().0).await;
        assert!(matches!(event, Event::Page { ref docs, has_more: false, .. } if docs.len() == 8), "{event:?}");
    }

    /// An import stops between batches and says exactly how many went in.
    #[tokio::test]
    async fn cancelling_an_import_stops_between_batches() {
        let (reg, a, _) = connected_registry().await;
        let dir = TempDir::made();
        let path = dir.0.join("rows.jsonl");
        let text: String = (0..3500).map(|i| format!("{{\"_id\": {i}, \"n\": {i}}}\n")).collect();
        std::fs::write(&path, text).unwrap();
        let ns = Namespace::new("scratch", "imported");
        let event = run_cancelled_after_first_batch(&reg, a, Command::Import { ns: ns.clone(), path, format: crate::api::ImportFormat::Auto }).await;
        let Event::Written(w) = event else { panic!("a cancelled import answers Written: {event:?}") };
        assert!(w.cancelled && w.inserted == 1000, "stopped after the first batch: {w:?}");
        assert_eq!(count(&reg, a, ns).await, 1000);
    }

    /// An export stops between batches and deletes what it had written.
    #[tokio::test]
    async fn cancelling_an_export_stops_between_batches() {
        let (reg, a, _) = connected_registry().await;
        let dir = TempDir::made();
        let path = dir.0.join("bets.json");
        let spec = crate::api::ExportSpec { ns: Namespace::new("demo-games", "bet_logs"), filter: doc! {}, sort: None, projection: None, format: crate::api::ExportFormat::JsonArray, path: path.clone(), fields: None };
        let event = run_cancelled_after_first_batch(&reg, a, Command::Export(Box::new(spec))).await;
        assert!(matches!(event, Event::Failed(CoreError::Cancelled)), "{event:?}");
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 0, "no file, no part");
    }

    #[tokio::test]
    async fn cancelling_a_copy_still_answers_copied() {
        let (reg, a, b) = connected_registry().await;
        let target = Namespace::new("copy", "bet_logs");
        let items = vec![copy_item(Namespace::new("demo-games", "bet_logs"), target.clone())];
        let event = run_cancelled_after_first_batch(&reg, a, Command::CopyCollections { to: b, items, options: Default::default() }).await;
        let Event::Copied(report) = event else { panic!("a cancelled copy answers Copied: {event:?}") };
        assert!(report.cancelled, "{report:?}");
        let inserted = report.items[0].inserted;
        assert!(inserted > 0 && inserted < 25_000, "stopped between batches: {inserted}");
        assert_eq!(count(&reg, b, target).await, inserted);
        assert!(reg.running.lock().unwrap().is_empty(), "the request is forgotten once answered");
    }

    #[tokio::test]
    async fn cancelling_a_recorded_write_still_answers_written() {
        let (reg, a, _) = connected_registry().await;
        let t = TempDir::new();
        let root = t.root();
        let key = crate::history::HistoryKey::new([7; 32]);
        let record = crate::api::Record { root: root.clone(), key: key.clone(), profile: uuid::Uuid::new_v4(), conn_name: "demo".into(), keep_days: 30, max_bytes: 1 << 30 };
        let ns = Namespace::new("demo-games", "bet_logs");
        let before = count(&reg, a, ns.clone()).await;
        let cmd = Command::DeleteWhere { ns: ns.clone(), filter: doc! {}, many: true };
        let event = run_cancelled_after_first_batch(&reg, a, Command::Recorded { record: record.clone(), cmd: Box::new(cmd) }).await;
        let Event::Written(w) = event else { panic!("a cancelled recorded write answers Written: {event:?}") };
        assert!(w.cancelled && w.deleted == 1000, "stopped after the first batch: {w:?}");
        let entries = crate::history::Store::new(root.clone(), key).list(record.profile, &ns).unwrap();
        assert_eq!((entries.len(), entries[0].count, Some(&entries[0].id)), (1, w.deleted, w.recorded.as_ref()));
        assert_eq!(count(&reg, a, ns).await, before - w.deleted);
    }

    #[tokio::test]
    async fn cancelling_a_restore_still_answers_restored() {
        let (reg, a, _) = connected_registry().await;
        let t = TempDir::new();
        let root = t.root();
        let key = crate::history::HistoryKey::new([7; 32]);
        let record = crate::api::Record { root: root.clone(), key: key.clone(), profile: uuid::Uuid::new_v4(), conn_name: "demo".into(), keep_days: 30, max_bytes: 1 << 30 };
        // An entry of 3,000 deleted documents, written directly: three batches,
        // so a cancel after the first leaves two undone.
        let ns = Namespace::new("scratch", "rows");
        let store = crate::history::Store::new(root.clone(), key.clone());
        let mut w = store.begin(record.profile, "demo", &ns, crate::history::Kind::Delete, doc! {}).unwrap();
        for b in 0..3 {
            let docs: Vec<bson::RawDocumentBuf> = (b * 1000..(b + 1) * 1000).map(|i| bson::RawDocumentBuf::try_from(&doc! { "_id": i, "i": i }).unwrap()).collect();
            w.save(&docs).unwrap();
        }
        let entry = w.finish(false).unwrap();

        let req = Request { id: 0, conn: Some(a), cmd: Command::HistoryConflicts { record: record.clone(), entry: entry.clone() } };
        let ev = handle(&reg, req, &Progress::channel().0).await;
        assert!(matches!(ev, Event::HistoryConflicts { total: 3000, conflicts: 0, ref sample } if sample.is_empty()), "{ev:?}");

        let event = run_cancelled_after_first_batch(&reg, a, Command::HistoryRestore { record: record.clone(), entry: entry.clone(), on_conflict: crate::api::OnConflict::Skip }).await;
        let Event::Restored { restored, skipped, overwritten, inserted_new, recorded, cancelled, warning, unconfirmed, unconfirmed_docs } = event else { panic!("a cancelled restore answers Restored: {event:?}") };
        assert!(cancelled && restored == 1000, "stopped after the first batch: {restored}");
        assert_eq!((skipped, overwritten, inserted_new, recorded, warning, unconfirmed, unconfirmed_docs), (0, 0, 0, None, None, false, 0));
        assert_eq!(count(&reg, a, ns.clone()).await, restored);
        let entries = crate::history::Store::new(root.clone(), key).list(record.profile, &ns).unwrap();
        assert!(!entries[0].restored, "a cancelled restore does not mark the entry");
    }
}
