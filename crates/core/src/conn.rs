//! An open connection: either a real MongoDB client or the in-memory demo
//! server. Both expose the same async operations, used by the engine (for UI
//! commands) and by the shell (through `Session`).

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

use crate::api::{CollInfo, CollKind, ConnectRequest, DbInfo, FindSpec, Namespace, ReplicaMember, ServerInfo, WriteOutcome};
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
}

pub struct MockConn {
    store: Mutex<MockStore>,
    latency: bool,
}

#[derive(Clone, Debug, Default)]
pub struct FindOpts {
    pub sort: Option<Document>,
    pub projection: Option<Document>,
    pub skip: Option<u64>,
    pub limit: Option<i64>,
    pub batch_size: Option<u32>,
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
        let mut out = Vec::with_capacity(n.min(1024));
        match self {
            DocCursor::Real(c) => {
                while out.len() < n {
                    match c.try_next().await? {
                        Some(d) => out.push(d),
                        None => break,
                    }
                }
            }
            DocCursor::Mock(it) => out.extend(it.take(n)),
        }
        Ok(out)
    }
}

impl Conn {
    pub async fn open(req: &ConnectRequest) -> CoreResult<Conn> {
        if req.profile.is_mock() {
            return Ok(Conn::mock(MockStore::demo(), true));
        }
        let (opts, tunnel) = client_options(req).await?;
        let client = Client::with_options(opts)?;
        Ok(Conn::Real(Arc::new(RealConn { client, _tunnel: tunnel })))
    }

    pub fn mock(store: MockStore, latency: bool) -> Conn {
        Conn::Mock(Arc::new(MockConn { store: Mutex::new(store), latency }))
    }

    pub fn is_mock(&self) -> bool {
        matches!(self, Conn::Mock(_))
    }

    /// Run `f` against the mock store after a short fake network delay.
    async fn with_mock<T>(m: &MockConn, f: impl FnOnce(&mut MockStore) -> CoreResult<T>) -> CoreResult<T> {
        if m.latency {
            let ms = rand::rng().random_range(20..120);
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
        let mut store = m.store.lock().map_err(|_| CoreError::Other("mock store poisoned".into()))?;
        f(&mut store)
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
                let hello = admin.run_command(doc! { "hello": 1 }).await?;
                let version = admin
                    .run_command(doc! { "buildInfo": 1 })
                    .await
                    .ok()
                    .and_then(|b| b.get_str("version").ok().map(str::to_string))
                    .unwrap_or_else(|| "?".into());
                let user = admin.run_command(doc! { "connectionStatus": 1 }).await.ok().and_then(|s| {
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
                // nameOnly + authorizedDatabases lets users without listDatabases
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

    pub async fn list_indexes(&self, ns: &Namespace) -> CoreResult<Vec<Document>> {
        match self {
            Conn::Mock(m) => Self::with_mock(m, |s| Ok(s.list_indexes(ns))).await,
            Conn::Real(c) => {
                let cursor = c.client.database(&ns.db).run_cursor_command(doc! { "listIndexes": &ns.coll }).await?;
                Ok(cursor.try_collect().await?)
            }
        }
    }

    /// One page of a find, plus whether more documents follow it.
    /// One page of a find, as raw BSON straight from the server (no parsing).
    pub async fn find_page(&self, spec: &FindSpec) -> CoreResult<(Vec<RawDocumentBuf>, bool)> {
        let limit = spec.limit.max(1);
        let want = limit as usize + 1;
        let mut docs = match self {
            Conn::Mock(_) => {
                let opts = FindOpts {
                    sort: spec.sort.clone(),
                    projection: spec.projection.clone(),
                    skip: Some(spec.skip),
                    limit: Some(limit + 1),
                    batch_size: None,
                };
                let docs = self.open_find(&spec.ns, spec.filter.clone(), opts).await?.next_batch(want).await?;
                docs.iter().map(RawDocumentBuf::try_from).collect::<Result<Vec<_>, _>>().map_err(|e| CoreError::Other(e.to_string()))?
            }
            Conn::Real(c) => {
                let mut cursor = Self::coll(c, &spec.ns)
                    .clone_with_type::<RawDocumentBuf>()
                    .find(spec.filter.clone())
                    .optional(spec.sort.clone(), |a, s| a.sort(s))
                    .optional(spec.projection.clone(), |a, p| a.projection(p))
                    .skip(spec.skip)
                    .limit(limit + 1)
                    .batch_size((limit + 1).min(10_000) as u32)
                    .await?;
                let mut out = Vec::with_capacity(want.min(1024));
                while out.len() < want {
                    match cursor.try_next().await? {
                        Some(d) => out.push(d),
                        None => break,
                    }
                }
                out
            }
        };
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
            Conn::Real(c) => {
                let cursor = Self::coll(c, ns)
                    .find(filter)
                    .optional(o.sort, |a, s| a.sort(s))
                    .optional(o.projection, |a, p| a.projection(p))
                    .optional(o.skip, |a, s| a.skip(s))
                    .optional(o.limit, |a, l| a.limit(l))
                    .optional(o.batch_size, |a, b| a.batch_size(b))
                    .await?;
                Ok(DocCursor::Real(cursor))
            }
        }
    }

    pub async fn open_aggregate(&self, ns: &Namespace, pipeline: Vec<Document>) -> CoreResult<DocCursor> {
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
        match self {
            Conn::Mock(m) => Self::with_mock(m, |s| s.insert(ns, doc)).await,
            Conn::Real(c) => Ok(Self::coll(c, ns).insert_one(doc).await?.inserted_id),
        }
    }

    pub async fn insert_many(&self, ns: &Namespace, docs: Vec<Document>) -> CoreResult<Vec<Bson>> {
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

    pub async fn update(&self, ns: &Namespace, filter: Document, update: UpdateSpec, many: bool, upsert: bool) -> CoreResult<WriteOutcome> {
        match self {
            Conn::Mock(m) => {
                let UpdateSpec::Doc(u) = update else {
                    return Err(CoreError::InvalidInput("the demo server does not support pipeline updates".into()));
                };
                Self::with_mock(m, |s| s.update(ns, &filter, &u, many, upsert)).await
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
        match self {
            Conn::Mock(m) => Self::with_mock(m, |s| s.replace(ns, &filter, doc, upsert)).await,
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
        match self {
            Conn::Mock(m) => Self::with_mock(m, |s| s.delete(ns, &filter, many)).await,
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
        let mut spec = options.unwrap_or_default();
        let name = match spec.get_str("name") {
            Ok(n) if !n.is_empty() => n.to_string(),
            _ => default_index_name(&keys),
        };
        spec.insert("name", name.clone());
        match self {
            Conn::Mock(m) => Self::with_mock(m, |s| Ok(s.create_index(ns, keys, Some(name)))).await,
            Conn::Real(c) => {
                let mut index = doc! { "key": keys };
                index.extend(spec);
                c.client.database(&ns.db).run_command(doc! { "createIndexes": &ns.coll, "indexes": [index] }).await?;
                Ok(name)
            }
        }
    }

    pub async fn drop_index(&self, ns: &Namespace, name: &str) -> CoreResult<()> {
        match self {
            Conn::Mock(m) => Self::with_mock(m, |s| s.drop_index(ns, name)).await,
            Conn::Real(c) => Ok(Self::coll(c, ns).drop_index(name).await?),
        }
    }

    pub async fn create_collection(&self, ns: &Namespace) -> CoreResult<()> {
        match self {
            Conn::Mock(m) => Self::with_mock(m, |s| s.create_collection(ns)).await,
            Conn::Real(c) => Ok(c.client.database(&ns.db).create_collection(&ns.coll).await?),
        }
    }

    pub async fn drop_collection(&self, ns: &Namespace) -> CoreResult<()> {
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
        match self {
            Conn::Mock(m) => Self::with_mock(m, |s| s.rename_collection(ns, to)).await,
            Conn::Real(c) => {
                let cmd = doc! { "renameCollection": ns.to_string(), "to": format!("{}.{to}", ns.db), "dropTarget": false };
                c.client.database("admin").run_command(cmd).await?;
                Ok(())
            }
        }
    }

    /// Copy every document, then every index except `_id_`, into `to`.
    pub async fn duplicate_collection(&self, ns: &Namespace, to: &str) -> CoreResult<()> {
        match self {
            Conn::Mock(m) => Self::with_mock(m, |s| s.duplicate_collection(ns, to)).await,
            Conn::Real(c) => {
                let db = c.client.database(&ns.db);
                if db.list_collection_names().filter(doc! { "name": to }).await?.iter().any(|n| n == to) {
                    return Err(CoreError::Server { code: 48, msg: format!("Collection {}.{to} already exists.", ns.db) });
                }
                // $out writes the copy server-side; the cursor it returns is empty.
                let mut cursor = Self::coll(c, ns).aggregate(vec![doc! { "$match": {} }, doc! { "$out": to }]).await?;
                while cursor.try_next().await?.is_some() {}
                let indexes: Vec<Document> = db.run_cursor_command(doc! { "listIndexes": &ns.coll }).await?.try_collect().await?;
                let specs: Vec<Bson> = indexes
                    .into_iter()
                    .filter(|i| i.get_str("name").ok() != Some("_id_"))
                    .map(|mut i| {
                        i.remove("v");
                        i.remove("ns");
                        Bson::Document(i)
                    })
                    .collect();
                if !specs.is_empty() {
                    db.run_command(doc! { "createIndexes": to, "indexes": specs }).await?;
                }
                Ok(())
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
        let specs = self.list_indexes(ns).await?;
        let sizes = self.coll_stats(ns).await.map(|d| crate::api::CollSummary::from_stats(&d).index_sizes).unwrap_or_default();
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
        // A URI with a username whose password lives in the keychain.
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
    async fn mock_find_page_reports_more() {
        let conn = Conn::mock(MockStore::demo(), false);
        let spec = FindSpec::all(Namespace::new("demo-games", "advertisers"), 3);
        let (docs, more) = conn.find_page(&spec).await.unwrap();
        assert_eq!((docs.len(), more), (3, true));
        let (docs, more) = conn.find_page(&FindSpec { skip: 3, ..spec }).await.unwrap();
        assert_eq!((docs.len(), more), (1, false));
    }
}
