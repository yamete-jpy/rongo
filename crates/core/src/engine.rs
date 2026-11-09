//! The request loop. Each request runs as its own task on the tokio runtime,
//! can be cancelled, and posts exactly one response.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bson::doc;
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

/// A connection the shell can use directly, outside the request loop.
#[derive(Clone)]
pub struct Session {
    pub conn: Conn,
    pub rt: Handle,
}

pub struct CoreHandle {
    tx: mpsc::UnboundedSender<Request>,
    rx: crossbeam_channel::Receiver<Response>,
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
        rt.spawn(async move {
            while let Some(req) = req_rx.recv().await {
                let reg = reg.clone();
                let resp_tx = resp_tx.clone();
                let waker = waker.clone();
                tokio::spawn(async move {
                    let (id, conn) = (req.id, req.conn);
                    let event = handle(&reg, req).await;
                    let _ = resp_tx.send(Response { id, conn, event });
                    waker();
                });
            }
        });
        CoreHandle { tx, rx, registry, rt, next_id: AtomicU64::new(1) }
    }

    pub fn send(&self, conn: Option<ConnId>, cmd: Command) -> RequestId {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
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

async fn handle(reg: &Registry, req: Request) -> Event {
    let token = reg.running.lock().expect("running").get(&req.id).cloned();
    let result = match token {
        Some(token) => tokio::select! {
            r = run(reg, req.conn, req.cmd) => r,
            () = token.cancelled() => Err(CoreError::Cancelled),
        },
        None => run(reg, req.conn, req.cmd).await,
    };
    reg.running.lock().expect("running").remove(&req.id);
    result.unwrap_or_else(Event::Failed)
}

async fn run(reg: &Registry, conn_id: Option<ConnId>, cmd: Command) -> CoreResult<Event> {
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
        Command::DeleteMany { ns, filter } => {
            let deleted = reg.conn(conn_id)?.delete(&ns, filter, true).await?;
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
            reg.conn(conn_id)?.duplicate_collection(&ns, &to).await?;
            Ok(Event::Done)
        }
        Command::CreateIndex { ns, keys, options } => {
            let name = reg.conn(conn_id)?.create_index(&ns, keys, options).await?;
            Ok(Event::Document(doc! { "createdIndex": name }))
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
            let count = crate::transfer::export(&reg.conn(conn_id)?, &spec).await?;
            Ok(Event::Exported { count, path: spec.path.clone() })
        }
        Command::Import { ns, path, format } => {
            let inserted = crate::transfer::import(&reg.conn(conn_id)?, &ns, &path, format).await?;
            Ok(Event::Written(WriteOutcome { inserted, ..Default::default() }))
        }
    }
}

fn check_collection_name(name: &str) -> CoreResult<()> {
    if name.is_empty() || name.contains('$') || name.contains('\0') || name.starts_with("system.") {
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

    fn wait(core: &CoreHandle, id: RequestId) -> Response {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(r) = core.try_recv() {
                if r.id == id {
                    return r;
                }
                continue;
            }
            assert!(Instant::now() < deadline, "no response for {id}");
            std::thread::sleep(Duration::from_millis(5));
        }
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
}
