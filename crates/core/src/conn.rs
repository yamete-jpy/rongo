//! An open connection: either a real MongoDB client or the in-memory demo
//! server. Both expose the same async operations, used by the engine (for UI
//! commands) and by the shell (through `Session`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bson::{Bson, Document, RawDocumentBuf, doc};
use futures_util::TryStreamExt;
use mongodb::Client;
use mongodb::options::{
    ClientOptions, Credential, ServerAddress, Tls, TlsOptions,
};
use mongodb::results::CollectionType;
use rand::Rng;
use tokio_util::sync::CancellationToken;

use crate::api::{CollInfo, CollKind, ConnectRequest, DbInfo, FindSpec, Namespace, ReplicaMember, ServerInfo, WriteOutcome};
use crate::engine::Progress;
use crate::error::{CoreError, CoreResult};
use crate::mock::MockStore;
use crate::profile::{Auth, ConnectionProfile, Target};
use crate::tunnel::Tunnel;

#[derive(Clone)]
pub enum Conn {
    Real(Arc<RealConn>),
    Mock(Arc<MockConn>),
}

pub struct RealConn {
    client: Client,
    /// Kept alive for as long as the connection is open.
    _tunnel: Option<Tunnel>,
    read_only: AtomicBool,
}

pub struct MockConn {
    store: Mutex<MockStore>,
    latency: bool,
    read_only: AtomicBool,
    /// Tests: a fixed delay (µs) before every call, a slow server's round trip.
    #[cfg(test)]
    delay_us: std::sync::atomic::AtomicU64,
    /// Tests: the next write is applied, then answers this error.
    #[cfg(test)]
    lose_reply: Mutex<Option<CoreError>>,
    /// Tests: the next unordered insert is applied, then reports a write-concern error.
    #[cfg(test)]
    concern_fails: AtomicBool,
    /// Tests: calls under way now, and the most there have been at once.
    #[cfg(test)]
    calls: (std::sync::atomic::AtomicUsize, std::sync::atomic::AtomicUsize),
}

#[derive(Clone, Debug, Default)]
pub struct FindOpts {
    pub sort: Option<Document>,
    pub projection: Option<Document>,
    pub skip: Option<u64>,
    pub limit: Option<i64>,
    pub batch_size: Option<u32>,
}

/// What an unordered insert managed: how many documents went in, the
/// positions of those refused because their `_id` already exists, and the
/// first other write error (a validator's, say), else a write-concern error.
/// With an error, `inserted` still counts exactly the documents that went in
/// around it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Inserted {
    pub inserted: u64,
    pub duplicates: Vec<usize>,
    pub error: Option<CoreError>,
    /// The server reported a write-concern error: the documents counted in
    /// `inserted` went in on the primary but were not confirmed as asked
    /// (they may yet be rolled back). Never a refusal.
    pub unconfirmed: bool,
}

impl Inserted {
    /// The server's write-concern error for the insert: unconfirmed, and the
    /// error unless a document was refused too.
    fn concern_failed(&mut self, code: i32, msg: &str) {
        self.error.get_or_insert(CoreError::WriteConcern { code, msg: msg.to_string() });
        self.unconfirmed = true;
    }
}

/// What an unordered insert of `n` documents managed, from the driver's
/// failure; `None` when it says nothing about them (the command failed).
fn unordered_outcome(n: usize, failure: &mongodb::error::InsertManyError) -> Option<Inserted> {
    let errors = failure.write_errors.as_deref().unwrap_or_default();
    if errors.is_empty() && failure.write_concern_error.is_none() {
        return None;
    }
    let mut duplicates: Vec<usize> = errors.iter().filter(|w| is_duplicate_id(w)).map(|w| w.index).collect();
    duplicates.sort_unstable();
    // Unordered: every document without a write error went in.
    let error = errors.iter().find(|w| !is_duplicate_id(w)).map(|w| CoreError::Server { code: w.code, msg: w.message.clone() });
    let mut out = Inserted { inserted: (n - errors.len()) as u64, duplicates, error, unconfirmed: false };
    if let Some(w) = &failure.write_concern_error {
        out.concern_failed(w.code, &w.message);
    }
    Some(out)
}

/// Update spec: an operator document or an aggregation pipeline.
#[derive(Clone, Debug)]
pub enum UpdateSpec {
    Doc(Document),
    Pipeline(Vec<Document>),
}

pub enum DocCursor {
    Real(mongodb::Cursor<Document>),
    Mock(std::vec::IntoIter<Document>),
}

impl DocCursor {
    /// Up to `n` more documents; an empty batch means the cursor is exhausted.
    pub async fn next_batch(&mut self, n: usize) -> CoreResult<Vec<Document>> {
        match self {
            DocCursor::Real(c) => drain(c, n).await,
            DocCursor::Mock(it) => Ok(it.take(n).collect()),
        }
    }
}

/// Up to `n` more of a driver cursor's documents.
async fn drain<T>(cursor: &mut mongodb::Cursor<T>, n: usize) -> CoreResult<Vec<T>>
where
    T: serde::de::DeserializeOwned + Unpin + Send + Sync,
{
    let mut out = Vec::with_capacity(n.min(1024));
    while out.len() < n {
        match cursor.try_next().await? {
            Some(d) => out.push(d),
            None => break,
        }
    }
    Ok(out)
}

/// A find with `o` applied, documents read as `T` (`Document`, or raw).
async fn find_with<T: Send + Sync>(coll: mongodb::Collection<T>, filter: Document, o: FindOpts) -> CoreResult<mongodb::Cursor<T>> {
    Ok(coll
        .find(filter)
        .optional(o.sort, |a, s| a.sort(s))
        .optional(o.projection, |a, p| a.projection(p))
        .optional(o.skip, |a, s| a.skip(s))
        .optional(o.limit, |a, l| a.limit(l))
        .optional(o.batch_size, |a, b| a.batch_size(b))
        .await?)
}

/// A find whose documents arrive as raw BSON, as the server sent them.
pub enum RawCursor {
    Real(mongodb::Cursor<RawDocumentBuf>),
    Mock(std::vec::IntoIter<RawDocumentBuf>),
}

impl RawCursor {
    /// Up to `n` more documents; an empty batch means the cursor is exhausted.
    pub async fn next_batch(&mut self, n: usize) -> CoreResult<Vec<RawDocumentBuf>> {
        match self {
            RawCursor::Real(c) => drain(c, n).await,
            RawCursor::Mock(it) => Ok(it.take(n).collect()),
        }
    }
}

impl Conn {
    pub async fn open(req: &ConnectRequest) -> CoreResult<Conn> {
        if req.profile.is_mock() {
            let conn = Conn::mock(MockStore::demo(), true);
            conn.set_read_only(req.profile.read_only);
            return Ok(conn);
        }
        let (opts, tunnel) = client_options(req).await?;
        let client = Client::with_options(opts)?;
        Ok(Conn::Real(Arc::new(RealConn { client, _tunnel: tunnel, read_only: AtomicBool::new(req.profile.read_only) })))
    }

    pub fn mock(store: MockStore, latency: bool) -> Conn {
        Conn::Mock(Arc::new(MockConn {
            store: Mutex::new(store),
            latency,
            read_only: AtomicBool::new(false),
            #[cfg(test)]
            delay_us: Default::default(),
            #[cfg(test)]
            lose_reply: Mutex::new(None),
            #[cfg(test)]
            concern_fails: AtomicBool::new(false),
            #[cfg(test)]
            calls: Default::default(),
        }))
    }

    fn read_only_flag(&self) -> &AtomicBool {
        match self {
            Conn::Real(c) => &c.read_only,
            Conn::Mock(m) => &m.read_only,
        }
    }

    /// Lock or unlock writes. Takes effect for every clone of this connection,
    /// including the ones held by open shell tabs.
    pub fn set_read_only(&self, on: bool) {
        self.read_only_flag().store(on, Ordering::SeqCst);
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only_flag().load(Ordering::SeqCst)
    }

    /// Direct access to the demo server's store, for tests.
    #[cfg(test)]
    pub(crate) fn with_store<T>(&self, f: impl FnOnce(&mut MockStore) -> T) -> T {
        let Conn::Mock(m) = self else { panic!("not a mock connection") };
        f(&mut m.store.lock().expect("mock store"))
    }

    /// Tests: every call to the demo server waits `d` first, as a round trip
    /// to a slow server would.
    #[cfg(test)]
    pub(crate) fn set_mock_delay(&self, d: Duration) {
        let Conn::Mock(m) = self else { panic!("not a mock connection") };
        m.delay_us.store(d.as_micros() as u64, Ordering::SeqCst);
    }

    /// Tests: the next update, replace or delete is applied, then answers a
    /// network error, as when the connection drops before the reply.
    #[cfg(test)]
    pub(crate) fn lose_next_reply(&self) {
        self.lose_next_reply_with(CoreError::Network("the connection closed before the reply".into()));
    }

    /// Tests: the next update, replace or delete is applied, then answers `e`.
    #[cfg(test)]
    pub(crate) fn lose_next_reply_with(&self, e: CoreError) {
        let Conn::Mock(m) = self else { panic!("not a mock connection") };
        *m.lose_reply.lock().expect("lose_reply") = Some(e);
    }

    /// Tests: the next unordered insert is applied, then reports a
    /// write-concern error, as a server whose replication timed out does.
    #[cfg(test)]
    pub(crate) fn concern_fails_next(&self) {
        let Conn::Mock(m) = self else { panic!("not a mock connection") };
        m.concern_fails.store(true, Ordering::SeqCst);
    }

    /// Tests: the most calls the demo server has had under way at once since
    /// the last time this was asked.
    #[cfg(test)]
    pub(crate) fn most_calls_at_once(&self) -> usize {
        let Conn::Mock(m) = self else { panic!("not a mock connection") };
        m.calls.1.swap(0, Ordering::SeqCst)
    }

    /// The guard every writing method starts with.
    pub(crate) fn writable(&self, what: &str) -> CoreResult<()> {
        if self.is_read_only() { Err(CoreError::ReadOnly(what.to_string())) } else { Ok(()) }
    }

    /// Run `f` against the mock store after a short fake network delay.
    async fn with_mock<T>(m: &MockConn, f: impl FnOnce(&mut MockStore) -> CoreResult<T>) -> CoreResult<T> {
        if m.latency {
            let ms = rand::rng().random_range(20..120);
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
        #[cfg(test)]
        {
            let now = m.calls.0.fetch_add(1, Ordering::SeqCst) + 1;
            m.calls.1.fetch_max(now, Ordering::SeqCst);
            let us = m.delay_us.load(Ordering::SeqCst);
            if us > 0 {
                tokio::time::sleep(Duration::from_micros(us)).await;
            }
            m.calls.0.fetch_sub(1, Ordering::SeqCst);
        }
        let mut store = m.store.lock().map_err(|_| CoreError::Other("mock store poisoned".into()))?;
        f(&mut store)
    }

    /// `with_mock` for an update, replace or delete; in tests its reply can be
    /// lost after it was applied (`lose_next_reply`).
    async fn with_mock_write<T>(m: &MockConn, f: impl FnOnce(&mut MockStore) -> CoreResult<T>) -> CoreResult<T> {
        let out = Self::with_mock(m, f).await;
        #[cfg(test)]
        if out.is_ok()
            && let Some(e) = m.lose_reply.lock().expect("lose_reply").take()
        {
            return Err(e);
        }
        out
    }

    fn coll(c: &RealConn, ns: &Namespace) -> mongodb::Collection<Document> {
        c.client.database(&ns.db).collection(&ns.coll)
    }

    pub async fn server_info(&self) -> CoreResult<ServerInfo> {
        match self {
            Conn::Mock(_) => Ok(ServerInfo {
                version: "7.0.0-mock".into(),
                host: "demo.local:27017".into(),
                role: "Primary".into(),
                replica_set: Some("rs-demo".into()),
                members: vec![
                    ReplicaMember { host: "demo.local:27017".into(), state: "PRIMARY".into() },
                    ReplicaMember { host: "demo-2.local:27017".into(), state: "SECONDARY".into() },
                ],
                user: Some("demo".into()),
            }),
            Conn::Real(c) => {
                let admin = c.client.database("admin");
                // Together, not one after another: over an SSH tunnel each
                // round trip is slow, and this runs on every connect.
                let (hello, build, status) = tokio::join!(
                    admin.run_command(doc! { "hello": 1 }),
                    admin.run_command(doc! { "buildInfo": 1 }),
                    admin.run_command(doc! { "connectionStatus": 1 }),
                );
                let hello = hello?;
                let version = build.ok().and_then(|b| b.get_str("version").ok().map(str::to_string)).unwrap_or_else(|| "?".into());
                let user = status.ok().and_then(|s| {
                    let users = s.get_document("authInfo").ok()?.get_array("authenticatedUsers").ok()?;
                    users.first()?.as_document()?.get_str("user").ok().map(str::to_string)
                });
                Ok(describe_server(&hello, version, user))
            }
        }
    }

    pub async fn list_databases(&self) -> CoreResult<Vec<DbInfo>> {
        match self {
            Conn::Mock(m) => Self::with_mock(m, |s| Ok(s.list_databases())).await,
            Conn::Real(c) => {
                // authorizedDatabases lets users without listDatabases
                // privilege still see the databases they can use.
                let res = c
                    .client
                    .database("admin")
                    .run_command(doc! { "listDatabases": 1, "authorizedDatabases": true })
                    .await?;
                let mut out: Vec<DbInfo> = res
                    .get_array("databases")
                    .map_err(|e| CoreError::Other(e.to_string()))?
                    .iter()
                    .filter_map(|d| {
                        let d = d.as_document()?;
                        Some(DbInfo {
                            name: d.get_str("name").ok()?.to_string(),
                            size_on_disk: match d.get("sizeOnDisk") {
                                Some(Bson::Int64(n)) => *n as u64,
                                Some(Bson::Int32(n)) => *n as u64,
                                Some(Bson::Double(n)) => *n as u64,
                                _ => 0,
                            },
                        })
                    })
                    .collect();
                out.sort_by(|a, b| a.name.cmp(&b.name));
                Ok(out)
            }
        }
    }

    pub async fn list_collections(&self, db: &str) -> CoreResult<Vec<CollInfo>> {
        match self {
            Conn::Mock(m) => Self::with_mock(m, |s| Ok(s.list_collections(db))).await,
            Conn::Real(c) => {
                let specs: Vec<_> = c.client.database(db).list_collections().await?.try_collect().await?;
                let mut out: Vec<CollInfo> = specs
                    .into_iter()
                    .map(|s| CollInfo {
                        name: s.name,
                        kind: match s.collection_type {
                            CollectionType::View => CollKind::View,
                            CollectionType::Timeseries => CollKind::Timeseries,
                            _ => CollKind::Collection,
                        },
                    })
                    .collect();
                out.sort_by(|a, b| a.name.cmp(&b.name));
                Ok(out)
            }
        }
    }

    /// The database's collections as `listCollections` returns them:
    /// `{ name, type: "collection"|"view"|"timeseries", options, … }`.
    pub async fn collection_specs(&self, db: &str) -> CoreResult<Vec<Document>> {
        match self {
            Conn::Mock(m) => Self::with_mock(m, |s| Ok(s.collection_specs(db))).await,
            Conn::Real(c) => {
                let cursor = c.client.database(db).run_cursor_command(doc! { "listCollections": 1 }).await?;
                Ok(cursor.try_collect().await?)
            }
        }
    }

    pub async fn list_indexes(&self, ns: &Namespace) -> CoreResult<Vec<Document>> {
        match self {
            Conn::Mock(m) => Self::with_mock(m, |s| Ok(s.list_indexes(ns))).await,
            Conn::Real(c) => {
                let cursor = c.client.database(&ns.db).run_cursor_command(doc! { "listIndexes": &ns.coll }).await?;
                Ok(cursor.try_collect().await?)
            }
        }
    }

    /// One page of a find, as raw BSON straight from the server (no
    /// parsing), plus whether more documents follow it.
    pub async fn find_page(&self, spec: &FindSpec) -> CoreResult<(Vec<RawDocumentBuf>, bool)> {
        // The page and one more, to learn whether more follow.
        let limit = spec.limit.clamp(1, i64::MAX - 1);
        let want = limit as usize + 1;
        let opts = FindOpts {
            sort: spec.sort.clone(),
            projection: spec.projection.clone(),
            skip: Some(spec.skip),
            limit: Some(limit + 1),
            batch_size: Some((limit + 1).min(10_000) as u32),
        };
        let mut docs = self.open_find_raw(&spec.ns, spec.filter.clone(), opts).await?.next_batch(want).await?;
        let has_more = docs.len() > limit as usize;
        docs.truncate(limit as usize);
        Ok((docs, has_more))
    }

    pub async fn open_find(&self, ns: &Namespace, filter: Document, o: FindOpts) -> CoreResult<DocCursor> {
        match self {
            Conn::Mock(m) => {
                let limit = o.limit.filter(|l| *l != 0).map(|l| l.unsigned_abs());
                let docs = Self::with_mock(m, |s| {
                    s.find(ns, &filter, o.sort.as_ref(), o.projection.as_ref(), o.skip.unwrap_or(0), limit)
                })
                .await?;
                Ok(DocCursor::Mock(docs.into_iter()))
            }
            Conn::Real(c) => Ok(DocCursor::Real(find_with(Self::coll(c, ns), filter, o).await?)),
        }
    }

    /// `open_find` with the documents as raw BSON, unparsed: Collection
    /// History keeps and hashes documents byte for byte.
    pub async fn open_find_raw(&self, ns: &Namespace, filter: Document, o: FindOpts) -> CoreResult<RawCursor> {
        match self {
            Conn::Mock(_) => {
                let docs = self.open_find(ns, filter, o).await?.next_batch(usize::MAX).await?;
                let raw = docs.iter().map(RawDocumentBuf::try_from).collect::<Result<Vec<_>, _>>().map_err(|e| CoreError::Other(e.to_string()))?;
                Ok(RawCursor::Mock(raw.into_iter()))
            }
            Conn::Real(c) => Ok(RawCursor::Real(find_with(Self::coll(c, ns).clone_with_type::<RawDocumentBuf>(), filter, o).await?)),
        }
    }

    pub async fn open_aggregate(&self, ns: &Namespace, pipeline: Vec<Document>) -> CoreResult<DocCursor> {
        if writes_output(&pipeline) {
            self.writable("aggregate with $out or $merge")?;
        }
        match self {
            Conn::Mock(m) => {
                let docs = Self::with_mock(m, |s| s.aggregate(ns, &pipeline)).await?;
                Ok(DocCursor::Mock(docs.into_iter()))
            }
            Conn::Real(c) => Ok(DocCursor::Real(Self::coll(c, ns).aggregate(pipeline).await?)),
        }
    }

    pub async fn count(&self, ns: &Namespace, filter: Document) -> CoreResult<u64> {
        match self {
            Conn::Mock(m) => Self::with_mock(m, |s| s.count(ns, &filter)).await,
            Conn::Real(c) => Ok(Self::coll(c, ns).count_documents(filter).await?),
        }
    }

    pub async fn estimated_count(&self, ns: &Namespace) -> CoreResult<u64> {
        match self {
            Conn::Mock(m) => Self::with_mock(m, |s| Ok(s.estimated_count(ns))).await,
            Conn::Real(c) => Ok(Self::coll(c, ns).estimated_document_count().await?),
        }
    }

    pub async fn insert_one(&self, ns: &Namespace, doc: Document) -> CoreResult<Bson> {
        self.writable("insert")?;
        match self {
            Conn::Mock(m) => Self::with_mock(m, |s| s.insert(ns, doc)).await,
            Conn::Real(c) => Ok(Self::coll(c, ns).insert_one(doc).await?.inserted_id),
        }
    }

    pub async fn insert_many(&self, ns: &Namespace, docs: Vec<Document>) -> CoreResult<Vec<Bson>> {
        self.writable("insert")?;
        match self {
            Conn::Mock(m) => Self::with_mock(m, |s| docs.into_iter().map(|d| s.insert(ns, d)).collect()).await,
            Conn::Real(c) => {
                let res = Self::coll(c, ns).insert_many(docs).await?;
                let mut ids: Vec<(usize, Bson)> = res.inserted_ids.into_iter().collect();
                ids.sort_by_key(|(i, _)| *i);
                Ok(ids.into_iter().map(|(_, id)| id).collect())
            }
        }
    }

    /// Insert every document it can (unordered). Documents refused because
    /// their `_id` already exists (error 11000 on `_id_`) are reported by
    /// position; the first other write error, or a write-concern error, is
    /// `Inserted::error` next to the exact count (a write-concern error also
    /// sets `Inserted::unconfirmed`). An error that fails the
    /// whole command is an `Err`. The documents are borrowed, so a caller
    /// that must name a refused one afterwards keeps it without a copy.
    pub async fn insert_many_unordered(&self, ns: &Namespace, docs: &[Document]) -> CoreResult<Inserted> {
        self.writable("insert")?;
        if docs.is_empty() {
            return Ok(Inserted::default());
        }
        match self {
            // The demo server keeps documents of its own.
            Conn::Mock(m) => {
                let out = Self::with_mock(m, |s| s.insert_unordered(ns, docs.to_vec())).await;
                #[cfg(test)]
                let out = out.map(|mut out| {
                    if m.concern_fails.swap(false, Ordering::SeqCst) {
                        out.concern_failed(64, "waiting for replication timed out");
                    }
                    out
                });
                out
            }
            Conn::Real(c) => {
                let e = match Self::coll(c, ns).insert_many(docs).ordered(false).await {
                    Ok(r) => return Ok(Inserted { inserted: r.inserted_ids.len() as u64, ..Default::default() }),
                    Err(e) => e,
                };
                let mongodb::error::ErrorKind::InsertMany(failure) = e.kind.as_ref() else { return Err(e.into()) };
                unordered_outcome(docs.len(), failure).ok_or_else(|| e.into())
            }
        }
    }

    pub async fn update(&self, ns: &Namespace, filter: Document, update: UpdateSpec, many: bool, upsert: bool) -> CoreResult<WriteOutcome> {
        self.writable("update")?;
        match self {
            Conn::Mock(m) => {
                let UpdateSpec::Doc(u) = update else {
                    return Err(CoreError::InvalidInput("the demo server does not support pipeline updates".into()));
                };
                Self::with_mock_write(m, |s| s.update(ns, &filter, &u, many, upsert)).await
            }
            Conn::Real(c) => {
                let coll = Self::coll(c, ns);
                let modifications: mongodb::options::UpdateModifications = match update {
                    UpdateSpec::Doc(d) => d.into(),
                    UpdateSpec::Pipeline(p) => p.into(),
                };
                let res = if many {
                    coll.update_many(filter, modifications).upsert(upsert).await?
                } else {
                    coll.update_one(filter, modifications).upsert(upsert).await?
                };
                Ok(WriteOutcome {
                    inserted_id: res.upserted_id,
                    matched: res.matched_count,
                    modified: res.modified_count,
                    ..Default::default()
                })
            }
        }
    }

    pub async fn replace_one(&self, ns: &Namespace, filter: Document, doc: Document, upsert: bool) -> CoreResult<WriteOutcome> {
        self.writable("replace")?;
        match self {
            Conn::Mock(m) => Self::with_mock_write(m, |s| s.replace(ns, &filter, doc, upsert)).await,
            Conn::Real(c) => {
                let res = Self::coll(c, ns).replace_one(filter, doc).upsert(upsert).await?;
                Ok(WriteOutcome {
                    inserted_id: res.upserted_id,
                    matched: res.matched_count,
                    modified: res.modified_count,
                    ..Default::default()
                })
            }
        }
    }

    pub async fn delete(&self, ns: &Namespace, filter: Document, many: bool) -> CoreResult<u64> {
        self.writable("delete")?;
        match self {
            Conn::Mock(m) => Self::with_mock_write(m, |s| s.delete(ns, &filter, many)).await,
            Conn::Real(c) => {
                let coll = Self::coll(c, ns);
                let res = if many { coll.delete_many(filter).await? } else { coll.delete_one(filter).await? };
                Ok(res.deleted_count)
            }
        }
    }

    pub async fn distinct(&self, ns: &Namespace, field: &str, filter: Document) -> CoreResult<Vec<Bson>> {
        match self {
            Conn::Mock(m) => Self::with_mock(m, |s| s.distinct(ns, field, &filter)).await,
            Conn::Real(c) => Ok(Self::coll(c, ns).distinct(field, filter).await?),
        }
    }

    pub async fn run_command(&self, db: &str, cmd: Document) -> CoreResult<Document> {
        if self.is_read_only() {
            match command_reads(&cmd) {
                Reads::Yes => {}
                Reads::Writes => return Err(CoreError::ReadOnly("aggregate with $out or $merge (or a pipeline that can't be checked)".into())),
                Reads::Unlisted(name) => return Err(CoreError::ReadOnly(format!("command {name} is not on the read-only allowlist"))),
            }
        }
        match self {
            Conn::Mock(m) => Self::with_mock(m, |s| s.run_command(db, &cmd)).await,
            Conn::Real(c) => Ok(c.client.database(db).run_command(cmd).await?),
        }
    }

    /// Create an index from its keys and options exactly as given. The
    /// definition goes to `createIndexes` unchanged, so options the driver's
    /// typed `IndexOptions` does not know (text weights, 2d bounds, …) still
    /// reach the server. Without a name, MongoDB's usual one is used.
    pub async fn create_index(&self, ns: &Namespace, keys: Document, options: Option<Document>) -> CoreResult<String> {
        self.writable("create index")?;
        let mut spec = options.unwrap_or_default();
        let name = match spec.get_str("name") {
            Ok(n) if !n.is_empty() => n.to_string(),
            _ => default_index_name(&keys),
        };
        spec.insert("name", name.clone());
        let mut index = doc! { "key": keys };
        index.extend(spec);
        match self {
            Conn::Mock(m) => Self::with_mock(m, |s| {
                s.create_indexes(ns, vec![index]);
                Ok(name)
            })
            .await,
            Conn::Real(c) => {
                c.client.database(&ns.db).run_command(doc! { "createIndexes": &ns.coll, "indexes": [index] }).await?;
                Ok(name)
            }
        }
    }

    pub async fn drop_index(&self, ns: &Namespace, name: &str) -> CoreResult<()> {
        self.writable("drop index")?;
        match self {
            Conn::Mock(m) => Self::with_mock(m, |s| s.drop_index(ns, name)).await,
            Conn::Real(c) => Ok(Self::coll(c, ns).drop_index(name).await?),
        }
    }

    /// Create several indexes in one `createIndexes`; each spec is `key` plus
    /// the index options, as `list_indexes` gives them (see `copyable_indexes`).
    pub async fn create_indexes(&self, ns: &Namespace, specs: Vec<Document>) -> CoreResult<()> {
        self.writable("create index")?;
        if specs.is_empty() {
            return Ok(());
        }
        match self {
            Conn::Mock(m) => Self::with_mock(m, |s| {
                s.create_indexes(ns, specs);
                Ok(())
            })
            .await,
            Conn::Real(c) => {
                c.client.database(&ns.db).run_command(doc! { "createIndexes": &ns.coll, "indexes": specs }).await?;
                Ok(())
            }
        }
    }

    pub async fn create_collection(&self, ns: &Namespace) -> CoreResult<()> {
        self.writable("create collection")?;
        match self {
            Conn::Mock(m) => Self::with_mock(m, |s| s.create_collection(ns)).await,
            Conn::Real(c) => Ok(c.client.database(&ns.db).create_collection(&ns.coll).await?),
        }
    }

    /// The `create` command with these options (as `listCollections` reports
    /// them: validator, capped, collation, time series, `viewOn`/`pipeline`, …).
    pub async fn create_collection_with(&self, ns: &Namespace, options: Document) -> CoreResult<()> {
        self.writable("create collection")?;
        match self {
            Conn::Mock(m) => Self::with_mock(m, |s| s.create_collection_with(ns, options)).await,
            Conn::Real(c) => {
                let mut cmd = doc! { "create": &ns.coll };
                cmd.extend(options);
                c.client.database(&ns.db).run_command(cmd).await?;
                Ok(())
            }
        }
    }

    pub async fn drop_collection(&self, ns: &Namespace) -> CoreResult<()> {
        self.writable("drop collection")?;
        match self {
            Conn::Mock(m) => Self::with_mock(m, |s| {
                s.drop_collection(ns);
                Ok(())
            })
            .await,
            Conn::Real(c) => Ok(Self::coll(c, ns).drop().await?),
        }
    }

    pub async fn rename_collection(&self, ns: &Namespace, to: &str) -> CoreResult<()> {
        self.writable("rename collection")?;
        match self {
            Conn::Mock(m) => Self::with_mock(m, |s| s.rename_collection(ns, to)).await,
            Conn::Real(c) => {
                let cmd = doc! { "renameCollection": ns.to_string(), "to": format!("{}.{to}", ns.db), "dropTarget": false };
                c.client.database("admin").run_command(cmd).await?;
                Ok(())
            }
        }
    }

    /// Copy a collection into a new one, `to`, in the same database, as Copy
    /// Collection does: the copy is created with the collection's options
    /// (capped, collation, time series, …), the documents follow in
    /// batches (each reported to `progress`), then the validator (so
    /// documents from before it came are copied too) and every index but
    /// `_id_`. A view is refused. The token is checked between batches. A
    /// copy that fails or is cancelled part way is dropped again; if that
    /// drop fails too, the error says the copy remains.
    pub(crate) async fn duplicate_collection(&self, ns: &Namespace, to: &str, progress: &Progress, cancel: &CancellationToken) -> CoreResult<()> {
        self.writable("duplicate collection")?;
        let target = Namespace::new(&ns.db, to);
        let specs = self.collection_specs(&ns.db).await?;
        let named = |name: &str| specs.iter().find(|s| s.get_str("name").ok() == Some(name));
        let source = named(&ns.coll).ok_or_else(|| CoreError::Server { code: 26, msg: format!("ns not found: {ns}") })?;
        if source.get_str("type").ok() == Some("view") {
            return Err(CoreError::InvalidInput(format!("{ns} is a view: only collections can be duplicated")));
        }
        if named(to).is_some() {
            return Err(CoreError::Server { code: 48, msg: format!("Collection {target} already exists.") });
        }
        let mut options = source.get_document("options").cloned().unwrap_or_default();
        let validation = match options.remove("validator") {
            Some(Bson::Document(v)) => Some((v, options.remove("validationLevel"), options.remove("validationAction"))),
            _ => None,
        };
        if cancel.is_cancelled() {
            return Err(CoreError::Cancelled);
        }
        self.create_collection_with(&target, options).await?;
        let total = self.estimated_count(ns).await.ok();
        let mut copied = 0u64;
        let filled = async {
            let mut cursor = self.open_find(ns, Document::new(), FindOpts { batch_size: Some(crate::transfer::BATCH as u32), ..Default::default() }).await?;
            loop {
                if cancel.is_cancelled() {
                    return Err(CoreError::Cancelled);
                }
                let batch = cursor.next_batch(crate::transfer::BATCH).await?;
                if batch.is_empty() {
                    break;
                }
                let n = batch.len() as u64;
                self.insert_many(&target, batch).await?;
                copied += n;
                progress.report(copied, total, "");
            }
            if let Some((validator, level, action)) = validation {
                let text = |v: &Option<Bson>, default| v.as_ref().and_then(Bson::as_str).unwrap_or(default).to_string();
                self.set_validator(&target, validator, &text(&level, "strict"), &text(&action, "error")).await?;
            }
            self.create_indexes(&target, copyable_indexes(self.list_indexes(ns).await?)).await
        };
        let Err(e) = filled.await else { return Ok(()) };
        // Nothing of a failed or stopped duplicate stays behind, or the error says what does.
        match self.drop_collection(&target).await {
            Ok(()) => Err(e),
            Err(drop) => {
                let what = if e == CoreError::Cancelled { format!("stopped after copying {copied} documents") } else { e.to_string() };
                // An insert that failed part way may have stored some of its
                // batch: say what the copy holds, as the server counts it.
                let holds = match self.estimated_count(&target).await {
                    Ok(n) => format!("with {} documents", n.max(copied)),
                    Err(_) => format!("with at least {copied} documents"),
                };
                Err(CoreError::Other(format!("{what}; the partial copy {target} remains, {holds} (dropping it failed: {drop})")))
            }
        }
    }

    pub async fn coll_stats(&self, ns: &Namespace) -> CoreResult<Document> {
        match self {
            Conn::Mock(m) => Self::with_mock(m, |s| s.coll_stats(ns)).await,
            Conn::Real(c) => {
                let pipeline = vec![doc! { "$collStats": { "storageStats": {}, "count": {} } }];
                let mut cursor = Self::coll(c, ns).aggregate(pipeline).await?;
                Ok(cursor.try_next().await?.unwrap_or_default())
            }
        }
    }

    pub async fn validator(&self, ns: &Namespace) -> CoreResult<Document> {
        match self {
            Conn::Mock(m) => Self::with_mock(m, |s| s.validator(ns)).await,
            Conn::Real(c) => {
                let reply = c
                    .client
                    .database(&ns.db)
                    .run_command(doc! { "listCollections": 1, "filter": { "name": &ns.coll } })
                    .await?;
                let options = reply
                    .get_document("cursor")
                    .ok()
                    .and_then(|cur| cur.get_array("firstBatch").ok())
                    .and_then(|b| b.first())
                    .and_then(Bson::as_document)
                    .and_then(|spec| spec.get_document("options").ok())
                    .cloned()
                    .unwrap_or_default();
                Ok(doc! {
                    "validator": options.get_document("validator").cloned().unwrap_or_default(),
                    "validationLevel": options.get_str("validationLevel").unwrap_or("strict"),
                    "validationAction": options.get_str("validationAction").unwrap_or("error"),
                })
            }
        }
    }

    pub async fn set_validator(&self, ns: &Namespace, validator: Document, level: &str, action: &str) -> CoreResult<()> {
        self.writable("set validator")?;
        match self {
            Conn::Mock(m) => Self::with_mock(m, |s| s.set_validator(ns, validator, level, action)).await,
            Conn::Real(c) => {
                let cmd = doc! { "collMod": &ns.coll, "validator": validator, "validationLevel": level, "validationAction": action };
                c.client.database(&ns.db).run_command(cmd).await?;
                Ok(())
            }
        }
    }

    /// Index specs joined with sizes (from collection stats) and usage (from
    /// `$indexStats`, which needs extra privileges; without them usage stays
    /// unknown rather than failing).
    pub async fn index_details(&self, ns: &Namespace) -> CoreResult<Vec<crate::api::IndexInfo>> {
        let usage = async {
            let mut usage: std::collections::HashMap<String, (i64, Option<bson::DateTime>)> = Default::default();
            if let Conn::Real(c) = self
                && let Ok(cursor) = Self::coll(c, ns).aggregate(vec![doc! { "$indexStats": {} }]).await
                && let Ok(rows) = cursor.try_collect::<Vec<Document>>().await
            {
                for r in rows {
                    let Ok(name) = r.get_str("name") else { continue };
                    let acc = r.get_document("accesses").ok();
                    let ops = acc.and_then(|a| a.get("ops")).and_then(|o| match o {
                        Bson::Int64(n) => Some(*n),
                        Bson::Int32(n) => Some(i64::from(*n)),
                        _ => None,
                    });
                    let since = acc.and_then(|a| a.get_datetime("since").ok()).copied();
                    if let Some(ops) = ops {
                        usage.insert(name.to_string(), (ops, since));
                    }
                }
            }
            usage
        };
        // The three reads go out together rather than one after another.
        let (specs, stats, usage) = tokio::join!(self.list_indexes(ns), self.coll_stats(ns), usage);
        let specs = specs?;
        let sizes = stats.map(|d| crate::api::CollSummary::from_stats(&d).index_sizes).unwrap_or_default();
        Ok(specs
            .into_iter()
            .map(|spec| {
                let name = spec.get_str("name").unwrap_or_default().to_string();
                let size = sizes.iter().find(|(n, _)| *n == name).map(|(_, s)| *s);
                let (ops, since) = usage.get(&name).map_or((None, None), |(o, s)| (Some(*o), *s));
                crate::api::IndexInfo { spec, size, ops, since }
            })
            .collect())
    }

    pub async fn modify_index(&self, ns: &Namespace, name: &str, hidden: Option<bool>, ttl: Option<i64>) -> CoreResult<()> {
        self.writable("modify index")?;
        let mut change = doc! { "name": name };
        if let Some(h) = hidden {
            change.insert("hidden", h);
        }
        if let Some(t) = ttl {
            change.insert("expireAfterSeconds", t);
        }
        match self {
            Conn::Mock(m) => Self::with_mock(m, |s| s.modify_index(ns, name, hidden, ttl)).await,
            Conn::Real(c) => {
                c.client.database(&ns.db).run_command(doc! { "collMod": &ns.coll, "index": change }).await?;
                Ok(())
            }
        }
    }

    pub async fn drop_database(&self, db: &str) -> CoreResult<()> {
        self.writable("drop database")?;
        match self {
            Conn::Mock(m) => Self::with_mock(m, |s| {
                s.drop_database(db);
                Ok(())
            })
            .await,
            Conn::Real(c) => Ok(c.client.database(db).drop().await?),
        }
    }
}

/// Commands a read-only connection may still run through `run_command`
/// (`aggregate` is allowed too, unless its pipeline writes, and `explain`
/// of any of these; see `command_reads`).
const READ_COMMANDS: &[&str] = &["ping", "hello", "isMaster", "ismaster", "buildInfo", "buildinfo", "hostInfo", "serverStatus",
    "connectionStatus", "currentOp", "replSetGetStatus", "listDatabases", "listCollections", "listIndexes", "dbStats", "collStats",
    "count", "distinct", "find", "getMore", "killCursors", "getParameter", "getCmdLineOpts", "dataSize", "usersInfo", "rolesInfo",
    "replSetGetConfig", "getLog", "top", "listShards", "connPoolStats", "lockInfo"];

/// The indexes worth recreating elsewhere, as `createIndexes` specs: every
/// one but `_id_` (and a clustered index, which only `create` can make),
/// without the `v`/`ns` fields `listIndexes` adds.
pub(crate) fn copyable_indexes(indexes: Vec<Document>) -> Vec<Document> {
    indexes
        .into_iter()
        .filter(|i| i.get_str("name").ok() != Some("_id_") && i.get_bool("clustered").ok() != Some(true))
        .map(|mut i| {
            i.remove("v");
            i.remove("ns");
            i
        })
        .collect()
}

/// Whether a write error is a duplicate key on `_id` (not on another unique index).
fn is_duplicate_id(w: &mongodb::error::IndexedWriteError) -> bool {
    w.code == 11000 && w.message.contains(" index: _id_ ")
}

/// Whether a pipeline's stages write to a collection (`$out`, `$merge`):
/// the read-only guard's test, and the shell's, which never runs such a
/// pipeline twice.
pub fn writes_output(pipeline: &[Document]) -> bool {
    pipeline.iter().any(|stage| stage.contains_key("$out") || stage.contains_key("$merge"))
}

/// What a read-only connection makes of a command.
#[derive(Debug, PartialEq)]
enum Reads {
    Yes,
    /// An aggregate whose pipeline writes or can't be checked.
    Writes,
    /// Not on the allowlist (it may well only read, but nobody checked).
    Unlisted(String),
}

/// Whether a read-only connection lets `cmd` through `run_command`: it only reads.
pub(crate) fn command_only_reads(cmd: &Document) -> bool {
    command_reads(cmd) == Reads::Yes
}

/// Whether `cmd` only reads: a command on the allowlist, an `aggregate`
/// whose pipeline doesn't write, or an `explain` of one of those. The
/// command an `explain` wraps is checked like any other, so an explained
/// `$out` (or anything that isn't a plain read) is refused: fail closed.
fn command_reads(cmd: &Document) -> Reads {
    let name = cmd.keys().next().map(String::as_str).unwrap_or("");
    match name {
        "aggregate" if command_pipeline_writes(cmd) => Reads::Writes,
        "aggregate" => Reads::Yes,
        "explain" => match cmd.get_document("explain") {
            Ok(inner) if inner.keys().next().map(String::as_str) != Some("explain") => command_reads(inner),
            _ => Reads::Unlisted("explain (of something other than a read)".into()),
        },
        _ if READ_COMMANDS.contains(&name) => Reads::Yes,
        _ => Reads::Unlisted(name.to_string()),
    }
}

/// Whether an `aggregate` command document's pipeline writes. A pipeline
/// that is missing or not an array of documents is treated as writing, so
/// a malformed command never slips past the guard.
fn command_pipeline_writes(cmd: &Document) -> bool {
    match cmd.get_array("pipeline") {
        Ok(stages) => stages.iter().any(|s| s.as_document().is_none_or(|d| d.contains_key("$out") || d.contains_key("$merge"))),
        Err(_) => true,
    }
}

/// MongoDB's default index name: `field_direction` pairs joined by `_`,
/// e.g. `chatID_1_sentAt_-1` or `body_text`.
pub fn default_index_name(keys: &Document) -> String {
    keys.iter()
        .map(|(k, v)| {
            let dir = match v {
                Bson::Int32(n) => n.to_string(),
                Bson::Int64(n) => n.to_string(),
                Bson::Double(f) => (*f as i64).to_string(),
                Bson::String(s) => s.clone(),
                other => crate::format::cell_text(other),
            };
            format!("{k}_{dir}")
        })
        .collect::<Vec<_>>()
        .join("_")
}

/// Builder helper: apply `f` only when the option is set.
trait Optional: Sized {
    fn optional<T>(self, value: Option<T>, f: impl FnOnce(Self, T) -> Self) -> Self {
        match value {
            Some(v) => f(self, v),
            None => self,
        }
    }
}

impl<T> Optional for T {}

fn describe_server(hello: &Document, version: String, user: Option<String>) -> ServerInfo {
    let set = hello.get_str("setName").ok().map(str::to_string);
    let me = hello.get_str("me").ok().map(str::to_string);
    let primary = hello.get_str("primary").ok();
    let role = if hello.get_str("msg").ok() == Some("isdbgrid") {
        "Mongos"
    } else if set.is_some() {
        if hello.get_bool("isWritablePrimary").unwrap_or(false) {
            "Primary"
        } else if hello.get_bool("secondary").unwrap_or(false) {
            "Secondary"
        } else {
            "Member"
        }
    } else {
        "Standalone"
    };
    let list = |key: &str| -> Vec<String> {
        hello
            .get_array(key)
            .map(|a| a.iter().filter_map(|h| h.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    };
    let mut members: Vec<ReplicaMember> = list("hosts")
        .into_iter()
        .chain(list("passives"))
        .map(|host| {
            let state = if Some(host.as_str()) == primary { "PRIMARY" } else { "SECONDARY" };
            ReplicaMember { host, state: state.into() }
        })
        .collect();
    members.extend(list("arbiters").into_iter().map(|host| ReplicaMember { host, state: "ARBITER".into() }));
    ServerInfo {
        version,
        host: me.or_else(|| members.first().map(|m| m.host.clone())).unwrap_or_else(|| "?".into()),
        role: role.into(),
        replica_set: set,
        members,
        user,
    }
}

const TIMEOUT: Duration = Duration::from_secs(10);

/// Driver options for a profile, starting the SSH tunnel first when needed.
async fn client_options(req: &ConnectRequest) -> CoreResult<(ClientOptions, Option<Tunnel>)> {
    let p: &ConnectionProfile = &req.profile;
    let mut opts = match &p.target {
        Target::Uri { uri } => ClientOptions::parse(uri).await?,
        Target::Manual { hosts, replica_set, direct } => {
            let mut o = ClientOptions::default();
            o.hosts = hosts
                .iter()
                .filter(|h| !h.trim().is_empty())
                .map(|h| ServerAddress::parse(h.trim()))
                .collect::<Result<_, _>>()?;
            if o.hosts.is_empty() {
                return Err(CoreError::InvalidInput("add at least one host".into()));
            }
            o.repl_set_name = replica_set.clone().filter(|s| !s.is_empty());
            o.direct_connection = Some(*direct);
            o
        }
    };

    if let Auth::Password { user, auth_db, mechanism } = &p.auth {
        let mut cred = opts.credential.take().unwrap_or_default();
        cred.username = Some(user.clone());
        cred.source = Some(if auth_db.is_empty() { "admin".into() } else { auth_db.clone() });
        cred.mechanism = match mechanism.as_deref() {
            None | Some("") => None,
            Some(m) => Some(m.parse().map_err(|_| CoreError::InvalidInput(format!("unknown auth mechanism {m}")))?),
        };
        cred.password = req.password.clone();
        opts.credential = Some(cred);
    } else if let Some(cred) = opts.credential.as_mut()
        && cred.password.is_none()
    {
        // A URI with a username whose password lives in the vault.
        cred.password = req.password.clone();
    }
    if opts.credential.as_ref().is_some_and(|c: &Credential| c.username.is_some() && c.password.is_none()) {
        return Err(CoreError::Auth("no password saved for this connection".into()));
    }

    if p.tls.enabled {
        let tls = TlsOptions::builder()
            .allow_invalid_certificates(p.tls.allow_invalid_certs.then_some(true))
            .ca_file_path(p.tls.ca_file.clone())
            .cert_key_file_path(p.tls.cert_key_file.clone())
            .build();
        opts.tls = Some(Tls::Enabled(tls));
    }

    let mut tunnel = None;
    if let Some(ssh) = &p.ssh {
        let target = opts
            .hosts
            .first()
            .cloned()
            .ok_or_else(|| CoreError::InvalidInput("no host to tunnel to".into()))?;
        let (host, port) = match &target {
            ServerAddress::Tcp { host, port } => (host.clone(), port.unwrap_or(27017)),
            other => return Err(CoreError::InvalidInput(format!("cannot tunnel to {other}"))),
        };
        let t = Tunnel::start(ssh, req.ssh_secret.clone(), host, port).await?;
        opts.hosts = vec![ServerAddress::Tcp { host: "127.0.0.1".into(), port: Some(t.local_port()) }];
        // Replica set discovery would hand out member names that are only
        // reachable from the far side of the tunnel.
        opts.direct_connection = Some(true);
        opts.repl_set_name = None;
        tunnel = Some(t);
    }

    opts.app_name = Some("rongo".into());
    opts.server_selection_timeout.get_or_insert(TIMEOUT);
    opts.connect_timeout.get_or_insert(TIMEOUT);
    Ok((opts, tunnel))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A write-concern error means the documents went in on the primary but
    /// were not confirmed as asked: never a refusal, whether or not a
    /// document was also refused, and whichever way the driver reports it.
    #[test]
    fn a_write_concern_error_is_unconfirmed_not_a_refusal() {
        let failure = |d: Document| bson::deserialize_from_document::<mongodb::error::InsertManyError>(d).unwrap();
        let concern = doc! { "code": 64, "codeName": "WriteConcernFailed", "errmsg": "waiting for replication timed out" };
        let wc = CoreError::WriteConcern { code: 64, msg: "waiting for replication timed out".into() };
        let out = unordered_outcome(3, &failure(doc! { "writeConcernError": concern.clone() })).unwrap();
        assert_eq!(out, Inserted { inserted: 3, duplicates: vec![], error: Some(wc.clone()), unconfirmed: true });
        let refused = doc! { "index": 3, "code": 121, "errmsg": "Document failed validation" };
        let duplicate = doc! { "index": 1, "code": 11000, "errmsg": "E11000 duplicate key error collection: t.c index: _id_ dup key: { _id: 1 }" };
        let out = unordered_outcome(5, &failure(doc! { "writeErrors": [duplicate.clone(), refused.clone()], "writeConcernError": concern.clone() })).unwrap();
        let validation = CoreError::Server { code: 121, msg: "Document failed validation".into() };
        assert_eq!(out, Inserted { inserted: 3, duplicates: vec![1], error: Some(validation.clone()), unconfirmed: true });
        let out = unordered_outcome(5, &failure(doc! { "writeErrors": [duplicate, refused] })).unwrap();
        assert_eq!(out, Inserted { inserted: 3, duplicates: vec![1], error: Some(validation), unconfirmed: false });
        // One document's update or replace, through the driver's error.
        let w = bson::deserialize_from_document::<mongodb::error::WriteConcernError>(concern).unwrap();
        let e = CoreError::from(mongodb::error::Error::from(mongodb::error::ErrorKind::Write(mongodb::error::WriteFailure::WriteConcernError(w))));
        assert_eq!(e, wc);
        assert!(!crate::recorded::changed_nothing(&e, true));
    }

    /// An ordered insert of many documents (Paste Documents, the shell's
    /// insertMany, Duplicate) whose only failure is a write-concern error
    /// went in on the primary: it is a write-concern error, never `Other`,
    /// which reads as a refusal that invites a retry.
    #[test]
    fn an_ordered_inserts_write_concern_error_is_unconfirmed() {
        let failure = |d: Document| bson::deserialize_from_document::<mongodb::error::InsertManyError>(d).unwrap();
        let concern = doc! { "code": 64, "codeName": "WriteConcernFailed", "errmsg": "waiting for replication timed out" };
        let e = CoreError::from(mongodb::error::Error::from(mongodb::error::ErrorKind::InsertMany(failure(doc! { "writeConcernError": concern }))));
        assert_eq!(e, CoreError::WriteConcern { code: 64, msg: "waiting for replication timed out".into() });
        assert!(!crate::recorded::changed_nothing(&e, false));
    }

    #[test]
    fn describes_replica_set_primary() {
        let hello = doc! {
            "setName": "rs0",
            "isWritablePrimary": true,
            "me": "a:27017",
            "primary": "a:27017",
            "hosts": ["a:27017", "b:27017"],
            "arbiters": ["c:27017"],
        };
        let info = describe_server(&hello, "7.0.2".into(), Some("u".into()));
        assert_eq!(info.role, "Primary");
        assert_eq!(info.host, "a:27017");
        assert_eq!(info.replica_set.as_deref(), Some("rs0"));
        let states: Vec<_> = info.members.iter().map(|m| m.state.as_str()).collect();
        assert_eq!(states, ["PRIMARY", "SECONDARY", "ARBITER"]);
    }

    #[test]
    fn default_index_names_follow_mongodb() {
        assert_eq!(default_index_name(&doc! { "chatID": 1, "sentAt": -1 }), "chatID_1_sentAt_-1");
        assert_eq!(default_index_name(&doc! { "body": "text" }), "body_text");
        assert_eq!(default_index_name(&doc! { "loc": "2dsphere" }), "loc_2dsphere");
    }

    #[test]
    fn describes_standalone_and_mongos() {
        assert_eq!(describe_server(&doc! { "isWritablePrimary": true }, "x".into(), None).role, "Standalone");
        assert_eq!(describe_server(&doc! { "msg": "isdbgrid" }, "x".into(), None).role, "Mongos");
    }

    #[tokio::test]
    async fn opening_a_locked_mock_profile_yields_a_locked_conn() {
        let mut profile = crate::profile::ConnectionProfile::mock();
        let req = ConnectRequest { profile: profile.clone(), password: None, ssh_secret: None };
        assert!(!Conn::open(&req).await.unwrap().is_read_only());
        profile.read_only = true;
        let req = ConnectRequest { profile, password: None, ssh_secret: None };
        let conn = Conn::open(&req).await.unwrap();
        assert!(conn.is_read_only());
        let err = conn.insert_one(&Namespace::new("demo-games", "advertisers"), doc! { "a": 1 }).await.unwrap_err();
        assert!(matches!(err, CoreError::ReadOnly(_)), "{err:?}");
    }

    #[tokio::test]
    async fn mock_find_page_reports_more() {
        let conn = Conn::mock(MockStore::demo(), false);
        let spec = FindSpec::all(Namespace::new("demo-games", "advertisers"), 3);
        let (docs, more) = conn.find_page(&spec).await.unwrap();
        assert_eq!((docs.len(), more), (3, true));
        let (docs, more) = conn.find_page(&FindSpec { skip: 3, ..spec }).await.unwrap();
        assert_eq!((docs.len(), more), (1, false));
    }

    /// The demo server keeps an index's options (unique, TTL, …) as a
    /// server does, so the index dialog shows them and TTL can be changed.
    #[tokio::test]
    async fn mock_create_index_keeps_its_options() {
        let c = Conn::mock(MockStore::empty(), false);
        let ns = Namespace::new("t", "c");
        c.insert_one(&ns, doc! { "at": 1 }).await.unwrap();
        let name = c.create_index(&ns, doc! { "at": 1 }, Some(doc! { "expireAfterSeconds": 60, "unique": true })).await.unwrap();
        assert_eq!(name, "at_1");
        let indexes = c.list_indexes(&ns).await.unwrap();
        let index = indexes.iter().find(|i| i.get_str("name").ok() == Some("at_1")).expect("created");
        assert_eq!((index.get_document("key").unwrap(), index.get_i32("expireAfterSeconds").ok(), index.get_bool("unique").ok()), (&doc! { "at": 1 }, Some(60), Some(true)), "{index}");
        c.modify_index(&ns, "at_1", None, Some(120)).await.unwrap();
        let named = c.create_index(&ns, doc! { "b": -1 }, Some(doc! { "name": "by_b", "sparse": true })).await.unwrap();
        assert_eq!(named, "by_b");
        let indexes = c.list_indexes(&ns).await.unwrap();
        assert!(indexes.iter().any(|i| i.get_str("name").ok() == Some("by_b") && i.get_bool("sparse").ok() == Some(true)), "{indexes:?}");
    }

    /// Duplicate Collection makes the copy with the collection's options
    /// (capped, collation, validator and its level and action) and indexes,
    /// copies documents written before the validator existed, and refuses a
    /// view, an existing target and a missing source.
    #[tokio::test]
    async fn duplicate_keeps_options_and_refuses_views() {
        let c = Conn::mock(MockStore::empty(), false);
        let ns = Namespace::new("t", "orders");
        c.create_collection_with(&ns, doc! { "capped": true, "size": 1_048_576, "collation": { "locale": "fr", "strength": 2 } }).await.unwrap();
        c.insert_many(&ns, (0..2500).map(|i| doc! { "_id": i, "b": i }).collect()).await.unwrap();
        c.set_validator(&ns, doc! { "a": { "$exists": true } }, "moderate", "error").await.unwrap();
        c.create_index(&ns, doc! { "b": 1 }, Some(doc! { "unique": true })).await.unwrap();

        c.duplicate_collection(&ns, "copy", &Progress::channel().0, &CancellationToken::new()).await.unwrap();
        let copy = Namespace::new("t", "copy");
        let specs = c.collection_specs("t").await.unwrap();
        let options = |name: &str| specs.iter().find(|s| s.get_str("name").ok() == Some(name)).and_then(|s| s.get_document("options").ok()).cloned().unwrap();
        let (from, to) = (options("orders"), options("copy"));
        assert_eq!(from.len(), to.len(), "{from} / {to}");
        for (k, v) in &from {
            assert_eq!(to.get(k), Some(v), "{k}: {from} / {to}");
        }
        let uuid = |name: &str| specs.iter().find(|s| s.get_str("name").ok() == Some(name)).and_then(|s| s.get_document("info").ok()?.get("uuid").cloned());
        assert_ne!(uuid("orders"), uuid("copy"), "another collection");
        assert_eq!(c.count(&copy, doc! {}).await.unwrap(), 2500, "documents from before the validator came too");
        let indexes = c.list_indexes(&copy).await.unwrap();
        assert!(indexes.iter().any(|i| i.get_str("name").ok() == Some("b_1") && i.get_bool("unique").ok() == Some(true)), "{indexes:?}");

        c.create_collection_with(&Namespace::new("t", "v"), doc! { "viewOn": "orders", "pipeline": [] }).await.unwrap();
        let e = c.duplicate_collection(&Namespace::new("t", "v"), "v2", &Progress::channel().0, &CancellationToken::new()).await.unwrap_err();
        assert!(matches!(e, CoreError::InvalidInput(ref m) if m.contains("view")), "{e:?}");
        assert!(c.collection_specs("t").await.unwrap().iter().all(|s| s.get_str("name").ok() != Some("v2")), "nothing made");
        assert!(matches!(c.duplicate_collection(&ns, "copy", &Progress::channel().0, &CancellationToken::new()).await, Err(CoreError::Server { code: 48, .. })));
        assert!(matches!(c.duplicate_collection(&Namespace::new("t", "gone"), "x", &Progress::channel().0, &CancellationToken::new()).await, Err(CoreError::Server { code: 26, .. })));
    }

    /// When Duplicate can't drop its partial copy, the error says how many
    /// documents the copy holds as the server counts them: an insert that
    /// failed part way may have stored some of its batch.
    #[tokio::test]
    async fn a_partial_duplicate_left_behind_is_counted() {
        let c = Conn::mock(MockStore::empty(), false);
        let ns = Namespace::new("t", "orders");
        c.insert_many(&ns, (0..2500).map(|i| doc! { "_id": i }).collect()).await.unwrap();
        let lock = c.clone();
        let progress = Progress::with_hook(move |done| {
            if done == 1000 {
                // Part of the next batch reached the copy; then the connection is locked.
                lock.with_store(|s| s.insert(&Namespace::new("t", "copy"), doc! { "_id": 1000 })).unwrap();
                lock.set_read_only(true);
            }
        });
        let e = c.duplicate_collection(&ns, "copy", &progress, &CancellationToken::new()).await.unwrap_err().to_string();
        c.set_read_only(false);
        assert!(e.contains("the partial copy t.copy remains, with 1001 documents"), "{e}");
    }

    /// A duplicate `_id` is told from a clash on another unique index by
    /// the index named in the message. The messages here are what MongoDB
    /// 4.4 and 7 return (checked against both in Docker); the driver keeps
    /// no `keyPattern` to go by.
    #[test]
    fn duplicate_ids_are_told_from_other_unique_keys() {
        let err = |code: i32, msg: &str| -> mongodb::error::IndexedWriteError { bson::deserialize_from_document(doc! { "index": 0, "code": code, "errmsg": msg }).unwrap() };
        assert!(is_duplicate_id(&err(11000, "E11000 duplicate key error collection: shop.orders index: _id_ dup key: { _id: 1.0 }")), "4.4");
        assert!(is_duplicate_id(&err(11000, "E11000 duplicate key error collection: shop.orders index: _id_ dup key: { _id: 1 }")), "7");
        assert!(!is_duplicate_id(&err(11000, "E11000 duplicate key error collection: shop.orders index: code_1 dup key: { code: \"a\" }")));
        assert!(!is_duplicate_id(&err(11000, "E11000 duplicate key error collection: shop.orders index: _id_1_code_1 dup key: { _id: 1, code: 2 }")));
        assert!(!is_duplicate_id(&err(121, "Document failed validation index: _id_ ")));
    }

    /// On a locked connection `run_command` lets reads through and looks
    /// inside what could write: an aggregate's pipeline, and what an
    /// `explain` explains. Anything it can't check is refused.
    #[tokio::test]
    async fn read_only_commands_are_checked_inside() {
        let c = Conn::mock(MockStore::demo(), false);
        c.set_read_only(true);
        let db = "demo-games";
        let locked = |r: &CoreResult<Document>| matches!(r, Err(CoreError::ReadOnly(_)));
        let allowed = [
            doc! { "aggregate": "advertisers", "pipeline": [{ "$match": {} }], "cursor": {} },
            doc! { "explain": { "find": "advertisers", "filter": {} }, "verbosity": "queryPlanner" },
            doc! { "explain": { "aggregate": "advertisers", "pipeline": [{ "$match": {} }], "cursor": {} } },
            doc! { "usersInfo": 1 },
            doc! { "rolesInfo": 1 },
            doc! { "replSetGetConfig": 1 },
            doc! { "getLog": "global" },
            doc! { "top": 1 },
            doc! { "listShards": 1 },
            doc! { "connPoolStats": 1 },
            doc! { "lockInfo": 1 },
        ];
        for cmd in allowed {
            let r = c.run_command(db, cmd.clone()).await;
            assert!(!locked(&r), "{cmd}: {r:?}");
        }
        let refused = [
            doc! { "aggregate": "advertisers", "cursor": {} },
            doc! { "aggregate": "advertisers", "pipeline": "not stages", "cursor": {} },
            doc! { "aggregate": "advertisers", "pipeline": [1], "cursor": {} },
            doc! { "explain": { "aggregate": "advertisers", "pipeline": [{ "$out": "x" }], "cursor": {} } },
            doc! { "explain": { "aggregate": "advertisers", "pipeline": [{ "$merge": { "into": "x" } }], "cursor": {} }, "verbosity": "executionStats" },
            doc! { "explain": { "aggregate": "advertisers", "cursor": {} } },
            doc! { "explain": "aggregate" },
            doc! { "explain": { "dropDatabase": 1 } },
            doc! { "explain": { "explain": { "aggregate": "advertisers", "pipeline": [{ "$out": "x" }] } } },
        ];
        for cmd in refused {
            let r = c.run_command(db, cmd.clone()).await;
            assert!(locked(&r), "{cmd}: {r:?}");
        }
        // An unlisted command may well only read: say why it was refused.
        let e = c.run_command(db, doc! { "someNewCommand": 1 }).await.unwrap_err().to_string();
        assert!(e.contains("someNewCommand is not on the read-only allowlist"), "{e}");
    }

    /// Times index_details on the demo server with its fake network delay
    /// (20-120 ms a call), which stands in for a round trip.
    /// `cargo test -p rongo-core --release --lib -- --ignored --nocapture report_index_details_time`
    #[tokio::test]
    #[ignore = "timing; run explicitly in release mode"]
    async fn report_index_details_time() {
        let c = Conn::mock(MockStore::demo(), true);
        let ns = Namespace::new("demo-games", "bet_logs");
        let t = std::time::Instant::now();
        for _ in 0..30 {
            assert_eq!(c.index_details(&ns).await.unwrap().len(), 1);
        }
        println!("index_details: {:?} a call (30 calls)", t.elapsed() / 30);
    }

    #[tokio::test]
    async fn read_only_connections_refuse_every_write() {
        let c = Conn::mock(MockStore::demo(), false);
        c.set_read_only(true);
        assert!(c.is_read_only());
        let ns = Namespace::new("demo-games", "advertisers");
        let other = Namespace::new("demo-games", "fresh");
        let before_count = c.count(&ns, doc! {}).await.unwrap();
        let before_dbs: Vec<String> = c.list_databases().await.unwrap().into_iter().map(|d| d.name).collect();
        let before_colls = c.list_collections("demo-games").await.unwrap().len();
        let before_indexes = c.list_indexes(&ns).await.unwrap().len();

        fn refused<T: std::fmt::Debug>(what: &str, r: CoreResult<T>) {
            assert!(matches!(r, Err(CoreError::ReadOnly(_))), "{what}: {r:?}");
        }
        refused("insert_one", c.insert_one(&ns, doc! { "x": 1 }).await);
        refused("insert_many", c.insert_many(&ns, vec![doc! { "x": 1 }]).await);
        refused("insert_many_unordered", c.insert_many_unordered(&ns, &[doc! { "x": 1 }]).await);
        refused("insert_many_unordered (nothing)", c.insert_many_unordered(&ns, &[]).await);
        refused("update",c.update(&ns, doc! {}, UpdateSpec::Doc(doc! { "$set": { "x": 1 } }), true, false).await);
        refused("replace_one", c.replace_one(&ns, doc! {}, doc! { "x": 1 }, false).await);
        refused("delete", c.delete(&ns, doc! {}, true).await);
        refused("create_index", c.create_index(&ns, doc! { "x": 1 }, None).await);
        refused("drop_index", c.drop_index(&ns, "x_1").await);
        refused("create_collection", c.create_collection(&other).await);
        refused("create_collection_with", c.create_collection_with(&other, doc! { "capped": true, "size": 4096 }).await);
        refused("create_indexes", c.create_indexes(&ns, vec![doc! { "key": { "x": 1 }, "name": "x_1" }]).await);
        refused("create_indexes (nothing)", c.create_indexes(&ns, Vec::new()).await);
        refused("drop_collection", c.drop_collection(&ns).await);
        refused("rename_collection", c.rename_collection(&ns, "renamed").await);
        refused("duplicate_collection", c.duplicate_collection(&ns, "copy", &Progress::channel().0, &CancellationToken::new()).await);
        refused("set_validator", c.set_validator(&ns, doc! {}, "strict", "error").await);
        refused("modify_index", c.modify_index(&ns, "x_1", Some(true), None).await);
        refused("drop_database", c.drop_database("demo-games").await);
        refused("$out", c.open_aggregate(&ns, vec![doc! { "$match": {} }, doc! { "$out": "elsewhere" }]).await.map(|_| ()));
        refused("$merge", c.open_aggregate(&ns, vec![doc! { "$merge": { "into": "elsewhere" } }]).await.map(|_| ()));
        refused("dropDatabase", c.run_command("demo-games", doc! { "dropDatabase": 1 }).await);
        refused("unknown command", c.run_command("demo-games", doc! { "someNewCommand": 1 }).await);
        refused("aggregate command with $out", c.run_command("demo-games", doc! { "aggregate": "advertisers", "pipeline": [{ "$out": "x" }], "cursor": {} }).await);

        // Nothing changed.
        assert_eq!(c.count(&ns, doc! {}).await.unwrap(), before_count);
        let after_dbs: Vec<String> = c.list_databases().await.unwrap().into_iter().map(|d| d.name).collect();
        assert_eq!(after_dbs, before_dbs);
        assert_eq!(c.list_collections("demo-games").await.unwrap().len(), before_colls);
        assert_eq!(c.list_indexes(&ns).await.unwrap().len(), before_indexes);

        // Reads still work.
        let spec = FindSpec::all(ns.clone(), 10);
        assert!(!c.find_page(&spec).await.unwrap().0.is_empty());
        assert_eq!(c.count(&ns, doc! {}).await.unwrap(), before_count);
        let mut cur = c.open_aggregate(&ns, vec![doc! { "$match": {} }]).await.unwrap();
        assert!(!cur.next_batch(10).await.unwrap().is_empty());
        assert!(c.run_command("demo-games", doc! { "dbStats": 1 }).await.is_ok());

        c.set_read_only(false);
        assert!(!c.is_read_only());
        c.insert_one(&ns, doc! { "x": 1 }).await.unwrap();
        assert_eq!(c.count(&ns, doc! {}).await.unwrap(), before_count + 1);
    }
}
