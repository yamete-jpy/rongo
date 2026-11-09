//! The UI ↔ core protocol. The UI sends [`Request`]s and receives [`Response`]s;
//! it remembers which [`RequestId`] belongs to which tab and drops stale ones.

use std::fmt;
use std::path::PathBuf;

use bson::{Bson, Document, RawDocumentBuf};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::CoreError;
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
/// the UI read from the keychain (or asked the user for).
#[derive(Clone, Debug)]
pub struct ConnectRequest {
    pub profile: ConnectionProfile,
    pub password: Option<String>,
    pub ssh_secret: Option<String>,
}

#[derive(Clone, Debug)]
pub enum Command {
    Connect(Box<ConnectRequest>),
    Disconnect,
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
    DeleteMany { ns: Namespace, filter: Document },
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
    /// Copy documents and indexes into a new collection in the same database.
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
    /// Stop a running request. Its response becomes `Failed(Cancelled)`.
    Cancel(RequestId),
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
