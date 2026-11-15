//! The UI ↔ core protocol. The UI sends [`Request`]s and receives [`Response`]s;
//! it remembers which [`RequestId`] belongs to which tab and drops stale ones.

use std::fmt;
use std::path::PathBuf;

use bson::{Bson, Document, RawDocumentBuf};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::CoreError;
use crate::history::{EntryId, HistoryKey, Kind, Store};
use crate::profile::ConnectionProfile;

pub type RequestId = u64;

/// One open connection. It is the id of the profile it was opened from, so a
/// profile can be open at most once.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ConnId(pub Uuid);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Namespace {
    pub db: String,
    pub coll: String,
}

impl Namespace {
    pub fn new(db: impl Into<String>, coll: impl Into<String>) -> Self {
        Self { db: db.into(), coll: coll.into() }
    }
}

impl fmt::Display for Namespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.db, self.coll)
    }
}

/// A paged find, as issued by a collection tab or a shell result tab.
#[derive(Clone, Debug, PartialEq)]
pub struct FindSpec {
    pub ns: Namespace,
    pub filter: Document,
    pub sort: Option<Document>,
    pub projection: Option<Document>,
    pub skip: u64,
    /// Page size. The core fetches one extra document to learn `has_more`.
    pub limit: i64,
}

impl FindSpec {
    pub fn all(ns: Namespace, limit: i64) -> Self {
        Self { ns, filter: Document::new(), sort: None, projection: None, skip: 0, limit }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExportFormat {
    /// One JSON array of relaxed Extended JSON documents.
    JsonArray,
    /// One relaxed Extended JSON document per line (mongoexport's default).
    JsonLines,
    Csv,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImportFormat {
    /// Decide from the file extension and the first character.
    Auto,
    JsonArray,
    JsonLines,
    Csv,
}

#[derive(Clone, Debug)]
pub struct ExportSpec {
    pub ns: Namespace,
    pub filter: Document,
    pub sort: Option<Document>,
    pub projection: Option<Document>,
    pub format: ExportFormat,
    pub path: PathBuf,
    /// CSV columns; `None` takes the fields of the first documents.
    pub fields: Option<Vec<String>>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum UpdateBody {
    /// `{ $set: … }` and other update operators.
    Doc(Document),
    /// An aggregation pipeline (`[{ $set: … }, …]`).
    Pipeline(Vec<Document>),
}

/// Everything needed to open a connection: the saved profile plus the secrets
/// the UI read from the vault (or asked the user for). Its Debug leaves the
/// secrets out, and those a URI carries (`profile::redact_uri_secrets`).
#[derive(Clone)]
pub struct ConnectRequest {
    pub profile: ConnectionProfile,
    pub password: Option<String>,
    pub ssh_secret: Option<String>,
}

impl fmt::Debug for ConnectRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut profile = self.profile.clone();
        if let crate::profile::Target::Uri { uri } = &mut profile.target {
            *uri = crate::profile::redact_uri_secrets(uri);
        }
        let hidden = |s: &Option<String>| s.as_ref().map(|_| "****");
        f.debug_struct("ConnectRequest").field("profile", &profile).field("password", &hidden(&self.password)).field("ssh_secret", &hidden(&self.ssh_secret)).finish()
    }
}

/// One collection (or view) to copy, and where to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyItem {
    pub src: Namespace,
    pub dst: Namespace,
    pub if_exists: IfExists,
}

/// What to do when the target collection already exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IfExists {
    /// Add the documents to it.
    Append(OnDuplicateId),
    /// Drop it first, then copy.
    Replace,
}

/// Appending a document whose `_id` the target already has.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OnDuplicateId {
    /// Keep the target's document.
    #[default]
    Skip,
    /// Replace the target's document with the source's.
    Overwrite,
    /// End that collection's copy with an error.
    Stop,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyOptions {
    /// Create the source's indexes (after the documents).
    pub indexes: bool,
    /// Create a missing target with the source's options and validator.
    pub collection_options: bool,
    /// Collection History for what Append + Overwrite replaces. The record
    /// describes the target (its profile and name); entries go under the
    /// target namespace. None keeps no history.
    pub history: Option<Record>,
}

impl Default for CopyOptions {
    fn default() -> Self {
        Self { indexes: true, collection_options: true, history: None }
    }
}

/// How a copy went, item by item. A cancelled copy lists the items it
/// started, with exact counts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CopyReport {
    pub items: Vec<CopiedItem>,
    pub cancelled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopiedItem {
    pub src: Namespace,
    pub dst: Namespace,
    pub inserted: u64,
    pub overwritten: u64,
    pub skipped: u64,
    /// The Collection History entry holding the target documents this item
    /// replaced (`CopyOptions::history`); none when it replaced nothing.
    pub recorded: Option<EntryId>,
    /// Something to know about an item that did not stop for it, such as a
    /// Collection History entry that could not be finished.
    pub warning: Option<String>,
    /// Why this item stopped early; the other items still ran.
    pub error: Option<String>,
    /// The write it stopped on got no confirmation (no answer from the
    /// server): it may have been applied. The counts are what was confirmed.
    /// After a write-concern error, the inserted count includes the batch
    /// the primary applied without that confirmation.
    pub unconfirmed: bool,
}

/// Where and how to keep Collection History for one recorded write
/// (`Command::Recorded`). The app builds it for each request from its settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    /// The history directory; the store keeps `<profile>/<db>/<collection>/` under it.
    pub root: PathBuf,
    pub key: HistoryKey,
    /// The saved connection profile the history belongs to.
    pub profile: Uuid,
    /// Kept in the entry, for the History tab.
    pub conn_name: String,
    /// Retention, applied after the write: older entries go first.
    pub keep_days: u32,
    pub max_bytes: u64,
}

impl Record {
    /// The store this history is kept in.
    pub(crate) fn store(&self) -> Store {
        Store::new(self.root.clone(), self.key.clone())
    }
}

/// What a Collection History restore does with a saved document that
/// conflicts with the collection as it is now (see `Command::HistoryConflicts`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OnConflict {
    /// Leave the collection's document as it is.
    #[default]
    Skip,
    /// Put the saved version in its place (an update's missing document is
    /// inserted). What it replaces is recorded first.
    Overwrite,
    /// Deletes only: insert the saved document with a new ObjectId `_id`.
    InsertNew,
}

#[derive(Clone, Debug)]
pub enum Command {
    Connect(Box<ConnectRequest>),
    Disconnect,
    /// Lock or unlock writes on the connection (answered `Done`).
    SetReadOnly(bool),
    ListDatabases,
    ListCollections { db: String },
    ListIndexes { ns: Namespace },
    Find(FindSpec),
    Count { ns: Namespace, filter: Document },
    Insert { ns: Namespace, doc: Document },
    Replace { ns: Namespace, id: Bson, doc: Document },
    /// Delete the documents with these `_id`s.
    Delete { ns: Namespace, ids: Vec<Bson> },
    InsertMany { ns: Namespace, docs: Vec<Document> },
    /// Apply an update document to every match.
    UpdateMany { ns: Namespace, filter: Document, update: Document },
    /// The Update dialog: an update document or a pipeline, applied to the
    /// first match or every match, optionally inserting when nothing matches.
    Update { ns: Namespace, filter: Document, update: UpdateBody, many: bool, upsert: bool },
    /// The Delete dialog: the first match or every match.
    DeleteWhere { ns: Namespace, filter: Document, many: bool },
    CreateCollection { ns: Namespace },
    DropCollection { ns: Namespace },
    /// Delete every document but keep the collection and its indexes.
    ClearCollection { ns: Namespace },
    RenameCollection { ns: Namespace, to: String },
    /// Copy a collection into a new one in the same database, created with
    /// its options (validator, collation, capped, time series), then its
    /// documents (in batches, with progress) and indexes. A view is refused.
    DuplicateCollection { ns: Namespace, to: String },
    DropDatabase { db: String },
    CreateIndex { ns: Namespace, keys: Document, options: Option<Document> },
    DropIndex { ns: Namespace, name: String },
    /// Replies with `{ validator, validationLevel, validationAction }`.
    GetValidator { ns: Namespace },
    SetValidator { ns: Namespace, validator: Document, level: String, action: String },
    CollStats { ns: Namespace },
    /// Index specs with their sizes and usage, for the index manager.
    IndexDetails { ns: Namespace },
    /// Change what MongoDB allows on an existing index: TTL and hidden.
    ModifyIndex { ns: Namespace, name: String, hidden: Option<bool>, expire_after_seconds: Option<i64> },
    RunCommand { db: String, cmd: Document },
    Export(Box<ExportSpec>),
    Import { ns: Namespace, path: PathBuf, format: ImportFormat },
    /// Copy collections from the request's connection into connection `to`
    /// (which may be the same one). Answered `Copied`, even when cancelled.
    CopyCollections { to: ConnId, items: Vec<CopyItem>, options: CopyOptions },
    /// Run `cmd` (`Delete`, `DeleteWhere`, `Replace`, `UpdateMany` or
    /// `Update`) saving the documents it deletes or changes in Collection
    /// History first, batch by batch, then writing exactly those by `_id`.
    /// Answered `Written`, with `recorded` set when anything was saved; once
    /// something has been written, a later stop or failure is a `warning` in
    /// that answer rather than `Failed`.
    Recorded { record: Record, cmd: Box<Command> },
    /// Compare a Collection History entry with its collection as it is now
    /// (`entry.ns` on the request's connection). A deleted document conflicts
    /// when its `_id` exists again; an updated one when it is gone or was
    /// changed after the operation. Answered `HistoryConflicts`.
    HistoryConflicts { record: Record, entry: EntryId },
    /// Put an entry's documents back: deleted ones are inserted, updated ones
    /// replaced by `_id` with the saved version; conflicts follow
    /// `on_conflict`. What it replaces is recorded first in a new update
    /// entry. Answered `Restored`, also when cancelled (it stops between
    /// batches) or when it stops part way after writing.
    HistoryRestore { record: Record, entry: EntryId, on_conflict: OnConflict },
    /// Stop a running request, if it can be stopped (`Command::cancellable`).
    /// A read's response becomes `Failed(Cancelled)`. A copy stops between
    /// batches and answers `Copied`; a recorded write and an import stop
    /// between batches and answer `Written` with what they wrote; a restore
    /// answers `Restored` the same way; an export or a duplicate stops between batches,
    /// deletes what it had written and answers `Failed(Cancelled)` (a duplicate whose partial copy can't be dropped says it remains). Any
    /// other write runs to completion and answers what it did.
    Cancel(RequestId),
}

impl Command {
    /// Only reads (or opens a connection), so a cancel drops it mid-flight
    /// and nothing is left half done on the server.
    pub fn only_reads(&self) -> bool {
        match self {
            Command::Connect(_)
            | Command::ListDatabases
            | Command::ListCollections { .. }
            | Command::ListIndexes { .. }
            | Command::Find(_)
            | Command::Count { .. }
            | Command::GetValidator { .. }
            | Command::CollStats { .. }
            | Command::IndexDetails { .. }
            | Command::HistoryConflicts { .. } => true,
            // The read-only connection's allowlist: aggregates that don't write included.
            Command::RunCommand { cmd, .. } => crate::conn::command_only_reads(cmd),
            _ => false,
        }
    }

    /// Watches its cancel token between batches, stops there and says
    /// exactly what it did.
    pub fn stops_between_batches(&self) -> bool {
        matches!(self, Command::CopyCollections { .. } | Command::DuplicateCollection { .. } | Command::Recorded { .. } | Command::HistoryRestore { .. } | Command::Import { .. } | Command::Export(_))
    }

    /// Whether `Cancel` can stop it. A write the server finishes once begun
    /// (clear, drop, rename, create index, …) can't be: cancelling it
    /// would only hide what the server goes on doing.
    pub fn cancellable(&self) -> bool {
        self.only_reads() || self.stops_between_batches()
    }

    /// The collection a write deletes or changes documents in, and how: the
    /// commands `Recorded` keeps in Collection History. None for commands
    /// that don't.
    pub fn recorded_target(&self) -> Option<(&Namespace, Kind)> {
        match self {
            Command::Delete { ns, .. } | Command::DeleteWhere { ns, .. } => Some((ns, Kind::Delete)),
            Command::Replace { ns, .. } | Command::UpdateMany { ns, .. } | Command::Update { ns, .. } => Some((ns, Kind::Update)),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Request {
    pub id: RequestId,
    pub conn: Option<ConnId>,
    pub cmd: Command,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplicaMember {
    pub host: String,
    pub state: String,
}

#[derive(Clone, Debug)]
pub struct ServerInfo {
    pub version: String,
    /// Host shown in the breadcrumb, e.g. `db1.example.net:27017`.
    pub host: String,
    /// `Primary`, `Secondary`, `Standalone`, `Mongos`…
    pub role: String,
    pub replica_set: Option<String>,
    pub members: Vec<ReplicaMember>,
    pub user: Option<String>,
}

#[derive(Clone, Debug)]
pub struct DbInfo {
    pub name: String,
    pub size_on_disk: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CollKind {
    Collection,
    View,
    Timeseries,
}

#[derive(Clone, Debug)]
pub struct CollInfo {
    pub name: String,
    pub kind: CollKind,
}

#[derive(Clone, Debug, Default)]
pub struct WriteOutcome {
    pub inserted_id: Option<Bson>,
    pub inserted: u64,
    pub matched: u64,
    pub modified: u64,
    pub deleted: u64,
    /// Documents skipped because their `_id` was already in the
    /// collection (`Command::Import`).
    pub duplicates: u64,
    /// The Collection History entry that holds what this write deleted or
    /// changed (`Command::Recorded` only).
    pub recorded: Option<EntryId>,
    /// A recorded write stopped between batches: cancelled, or the
    /// connection was switched to read-only part way. The counts are exact.
    pub cancelled: bool,
    /// A recorded write stopped or finished with a problem after it had
    /// written something (so it is not an error: retrying could apply an
    /// update twice); says what happened.
    pub warning: Option<String>,
    /// A recorded write's last write got no confirmation (no answer from the
    /// server, or an error part way through a write of many documents): it
    /// may have been applied, wholly or in part. The counts are what was
    /// confirmed, `warning` says why, and the entry holds the documents as
    /// they were. Also an import whose batch met a write-concern error: its
    /// documents went in on the primary, counted, but were not confirmed.
    pub unconfirmed: bool,
    /// Of the documents counted in `inserted`, those that went in on the
    /// primary in a batch that met a write-concern error (an import's):
    /// not confirmed. The rest were.
    pub unconfirmed_docs: u64,
}

#[derive(Clone, Debug)]
pub enum Event {
    Connected(ServerInfo),
    Disconnected,
    Databases(Vec<DbInfo>),
    Collections { db: String, items: Vec<CollInfo> },
    Indexes { ns: Namespace, items: Vec<Document> },
    /// A page of a find, as raw BSON: parsed documents of nested data take
    /// up to twenty times the memory, so the UI parses only what it shows.
    Page { docs: Vec<RawDocumentBuf>, has_more: bool, elapsed_ms: u64 },
    Count(u64),
    Written(WriteOutcome),
    /// A command reply, statistics, or a validator.
    Document(Document),
    Exported { count: u64, path: PathBuf },
    IndexDetails { ns: Namespace, items: Vec<IndexInfo> },
    /// How far a long request has got. Sent any number of times before the
    /// request's one terminal event; never ends the request.
    Progress { done: u64, total: Option<u64>, note: String },
    Copied(CopyReport),
    /// `Command::HistoryConflicts`: how many documents the entry holds, how
    /// many conflict, and the `_id`s of the first 20 that do.
    HistoryConflicts { total: u64, conflicts: u64, sample: Vec<Bson> },
    /// `Command::HistoryRestore`, with exact counts. `restored`: put back as
    /// saved without replacing a conflicting document (inserted, or an update
    /// replaced over what rongo had written, or already as saved);
    /// `overwritten`: conflicting documents replaced; `inserted_new`: inserted
    /// with a new `_id`; `skipped`: conflicts left alone. `recorded` is the new
    /// update entry holding what was replaced (none when nothing was).
    /// `cancelled`: stopped between batches (cancel, or the connection was
    /// switched to read-only); `warning`: why it stopped or what went wrong
    /// after something had been written; `unconfirmed`: a write got no
    /// confirmation and may have been applied (the counts are what was
    /// confirmed, but for `unconfirmed_docs`: documents counted in
    /// `restored` or `inserted_new` that went in on the primary under a
    /// write-concern error, not confirmed). Only a restore that finished
    /// marks the entry restored.
    Restored { restored: u64, skipped: u64, overwritten: u64, inserted_new: u64, recorded: Option<EntryId>, cancelled: bool, warning: Option<String>, unconfirmed: bool, unconfirmed_docs: u64 },
    Done,
    Failed(CoreError),
}

/// One index with what the server reports about it.
#[derive(Clone, Debug, PartialEq)]
pub struct IndexInfo {
    /// As listed by `listIndexes`: name, key, unique, expireAfterSeconds, …
    pub spec: Document,
    pub size: Option<u64>,
    /// Operations that used the index since `since` (from `$indexStats`).
    pub ops: Option<i64>,
    pub since: Option<bson::DateTime>,
}

/// The numbers a collection's statistics boil down to.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CollSummary {
    pub count: u64,
    pub size: u64,
    pub storage_size: u64,
    pub indexes: u64,
    pub total_index_size: u64,
    pub index_sizes: Vec<(String, u64)>,
}

impl CollSummary {
    /// From a `$collStats` document (fields under `storageStats`) or a
    /// `collStats` command reply (fields at the top level).
    pub fn from_stats(d: &Document) -> CollSummary {
        let src = d.get_document("storageStats").unwrap_or(d);
        let num = |doc: &Document, k: &str| match doc.get(k) {
            Some(Bson::Int32(n)) => *n.max(&0) as u64,
            Some(Bson::Int64(n)) => *n.max(&0) as u64,
            Some(Bson::Double(f)) if *f > 0.0 => *f as u64,
            _ => 0,
        };
        let count = match d.get("count") {
            Some(Bson::Document(c)) => num(c, "count"),
            Some(_) => num(d, "count"),
            None => num(src, "count"),
        };
        let index_sizes: Vec<(String, u64)> = src
            .get_document("indexSizes")
            .map(|m| m.keys().map(|k| (k.clone(), num(m, k))).collect())
            .unwrap_or_default();
        CollSummary {
            count,
            size: num(src, "size"),
            storage_size: num(src, "storageSize"),
            indexes: match num(src, "nindexes") {
                0 => index_sizes.len() as u64,
                n => n,
            },
            total_index_size: match num(src, "totalIndexSize") {
                0 => index_sizes.iter().map(|(_, s)| s).sum(),
                n => n,
            },
            index_sizes,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Response {
    pub id: RequestId,
    pub conn: Option<ConnId>,
    pub event: Event,
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::doc;

    /// A connect request's Debug (and so a Command's and a Request's)
    /// never prints a password, the SSH secret, or the secrets a URI can
    /// carry: its password, a TLS key file's password (either spelling) and
    /// an AWS session token.
    #[test]
    fn connect_requests_never_print_secrets() {
        let mut profile = ConnectionProfile::new("p");
        let uri = "mongodb://alice:hunter2@db.example:27017/?authSource=admin&tlsCertificateKeyFilePassword=k3y-pw&sslPEMKeyPassword=pem-pw&authMechanismProperties=AWS_SESSION_TOKEN:t0ken%2Bx,SERVICE_NAME:svc";
        profile.target = crate::profile::Target::Uri { uri: uri.into() };
        let req = ConnectRequest { profile, password: Some("s3cret-pw".into()), ssh_secret: Some("ssh-pass".into()) };
        let cmd = Command::Connect(Box::new(req.clone()));
        for text in [format!("{req:?}"), format!("{cmd:?}"), format!("{:?}", Request { id: 1, conn: None, cmd: cmd.clone() })] {
            for secret in ["s3cret-pw", "ssh-pass", "hunter2", "k3y-pw", "pem-pw", "t0ken"] {
                assert!(!text.contains(secret), "{secret} in {text}");
            }
            assert!(text.contains("db.example") && text.contains("alice") && text.contains("authSource=admin") && text.contains("SERVICE_NAME"), "the rest still shows: {text}");
        }
    }

    #[test]
    fn summary_reads_collstats_aggregation_output() {
        let d = doc! {
            "ns": "db.c",
            "count": 7_i64,
            "storageStats": { "size": 100, "storageSize": 4096, "nindexes": 2, "totalIndexSize": 8192_i64, "indexSizes": { "_id_": 4096, "a_1": 4096 } },
        };
        let s = CollSummary::from_stats(&d);
        assert_eq!((s.count, s.size, s.storage_size, s.indexes, s.total_index_size), (7, 100, 4096, 2, 8192));
        assert_eq!(s.index_sizes, vec![("_id_".to_string(), 4096), ("a_1".to_string(), 4096)]);
    }
}
