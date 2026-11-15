//! An in-memory stand-in for a MongoDB server, used by `rongo --mock` and by
//! tests that should not need a real server.

pub mod query;

use std::collections::{BTreeMap, HashMap};

use bson::oid::ObjectId;
use bson::{Bson, Document, doc};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use crate::api::{CollInfo, CollKind, DbInfo, Namespace, WriteOutcome};
use crate::error::{CoreError, CoreResult};
use crate::paths;

#[derive(Default)]
pub struct MockColl {
    pub docs: Vec<Document>,
    pub indexes: Vec<Document>,
    /// What `create` was given and `listCollections` reports: validator,
    /// capped, `viewOn`/`pipeline` for a view, `timeseries`, … Mostly stored
    /// only; inserts do check the validator (see `accepts`).
    pub options: Document,
    /// Which collection this is, as `listCollections` reports it in
    /// `info.uuid`: new on create, kept by a rename. None stands for a
    /// server that reports none (an older one, or a time series whose
    /// buckets entry the user can't see).
    pub uuid: Option<bson::Uuid>,
    /// Positions in `docs` by `query::eq_hash` of the `_id`, so an insert
    /// finds a duplicate without a scan. Kept by `insert`, rebuilt when
    /// documents go or an `_id` changes (`reindex`).
    ids: HashMap<u64, Vec<usize>>,
}

impl MockColl {
    fn new(docs: Vec<Document>) -> Self {
        let mut c = Self { docs, indexes: vec![doc! { "v": 2, "key": { "_id": 1 }, "name": "_id_" }], options: Document::new(), uuid: Some(bson::Uuid::new()), ids: HashMap::new() };
        c.reindex();
        c
    }

    fn reindex(&mut self) {
        self.ids.clear();
        for (i, d) in self.docs.iter().enumerate() {
            if let Some(id) = d.get("_id") {
                self.ids.entry(query::eq_hash(id)).or_default().push(i);
            }
        }
    }

    /// Whether a document with this `_id` (by `bson_eq`) is here.
    fn has_id(&self, id: &Bson) -> bool {
        self.ids.get(&query::eq_hash(id)).is_some_and(|at| at.iter().any(|&i| self.docs[i].get("_id").is_some_and(|x| query::bson_eq(x, id))))
    }

    /// Whether the validator lets `doc` in. The demo server checks
    /// `$jsonSchema.required` and the query conditions it can evaluate; what
    /// it can't evaluate lets everything through. `validationAction: "warn"`
    /// and `validationLevel: "off"` let everything through, as on a server.
    fn accepts(&self, doc: &Document) -> bool {
        let o = &self.options;
        let Ok(validator) = o.get_document("validator") else { return true };
        if o.get_str("validationLevel").ok() == Some("off") || o.get_str("validationAction").ok() == Some("warn") {
            return true;
        }
        let mut conditions = validator.clone();
        if let Some(Bson::Document(schema)) = conditions.remove("$jsonSchema") {
            let required = schema.get_array("required").map(|r| r.iter().filter_map(Bson::as_str).collect::<Vec<_>>()).unwrap_or_default();
            if !required.iter().all(|field| doc.contains_key(field)) {
                return false;
            }
        }
        query::matches(doc, &conditions).unwrap_or(true)
    }

    fn kind(&self) -> CollKind {
        if self.options.contains_key("viewOn") {
            CollKind::View
        } else if self.options.contains_key("timeseries") {
            CollKind::Timeseries
        } else {
            CollKind::Collection
        }
    }
}

#[derive(Default)]
pub struct MockStore {
    dbs: BTreeMap<String, BTreeMap<String, MockColl>>,
}

impl MockStore {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn list_databases(&self) -> Vec<DbInfo> {
        self.dbs
            .iter()
            .map(|(name, colls)| DbInfo {
                name: name.clone(),
                size_on_disk: colls.values().map(|c| c.docs.len() as u64 * 180).sum(),
            })
            .collect()
    }

    pub fn list_collections(&self, db: &str) -> Vec<CollInfo> {
        self.dbs
            .get(db)
            .map(|colls| colls.iter().map(|(name, c)| CollInfo { name: name.clone(), kind: c.kind() }).collect())
            .unwrap_or_default()
    }

    /// `{ name, type, options, info }` per collection, as `listCollections` returns them.
    pub fn collection_specs(&self, db: &str) -> Vec<Document> {
        let Some(colls) = self.dbs.get(db) else { return Vec::new() };
        colls
            .iter()
            .map(|(name, c)| {
                let kind = match c.kind() {
                    CollKind::View => "view",
                    CollKind::Timeseries => "timeseries",
                    CollKind::Collection => "collection",
                };
                // Like a server, a view has no uuid. (A real time series has
                // none either, only its buckets do; here it has its own.)
                let mut info = doc! { "readOnly": c.kind() == CollKind::View };
                if let Some(uuid) = c.uuid.filter(|_| c.kind() != CollKind::View) {
                    info.insert("uuid", uuid);
                }
                doc! { "name": name, "type": kind, "options": c.options.clone(), "info": info }
            })
            .collect()
    }

    fn coll(&self, ns: &Namespace) -> Option<&MockColl> {
        self.dbs.get(&ns.db)?.get(&ns.coll)
    }

    /// The collection, created if missing: only for the writes that create
    /// one on a server (inserts, upserts, `createIndexes`).
    fn coll_mut(&mut self, ns: &Namespace) -> &mut MockColl {
        // Like the server, a collection created by a write has an _id index.
        self.dbs.entry(ns.db.clone()).or_default().entry(ns.coll.clone()).or_insert_with(|| MockColl::new(Vec::new()))
    }

    /// The collection if it exists, for the writes that create none.
    fn existing_mut(&mut self, ns: &Namespace) -> Option<&mut MockColl> {
        self.dbs.get_mut(&ns.db)?.get_mut(&ns.coll)
    }

    pub fn list_indexes(&self, ns: &Namespace) -> Vec<Document> {
        self.coll(ns).map(|c| c.indexes.clone()).unwrap_or_default()
    }

    /// `createIndexes`: each spec has `key` and the index options, `name`
    /// included (MongoDB's usual name when missing). Existing names are kept.
    /// A missing collection is created, as on a server.
    pub fn create_indexes(&mut self, ns: &Namespace, specs: Vec<Document>) {
        let c = self.coll_mut(ns);
        for spec in specs {
            let name = match spec.get_str("name") {
                Ok(n) => n.to_string(),
                Err(_) => crate::conn::default_index_name(&spec.get_document("key").cloned().unwrap_or_default()),
            };
            if c.indexes.iter().any(|i| i.get_str("name").ok() == Some(name.as_str())) {
                continue;
            }
            let mut index = doc! { "v": 2 };
            index.extend(spec);
            index.insert("name", name);
            c.indexes.push(index);
        }
    }

    pub fn drop_index(&mut self, ns: &Namespace, name: &str) -> CoreResult<()> {
        let c = self.existing_mut(ns).ok_or_else(|| ns_not_found(ns))?;
        let before = c.indexes.len();
        c.indexes.retain(|i| i.get_str("name").ok() != Some(name) || name == "_id_");
        if c.indexes.len() == before {
            return Err(CoreError::Server { code: 27, msg: format!("index not found with name [{name}]") });
        }
        Ok(())
    }

    /// Matching documents after filter, sort, skip, limit and projection.
    pub fn find(
        &self,
        ns: &Namespace,
        filter: &Document,
        sort: Option<&Document>,
        projection: Option<&Document>,
        skip: u64,
        limit: Option<u64>,
    ) -> CoreResult<Vec<Document>> {
        let Some(c) = self.coll(ns) else { return Ok(Vec::new()) };
        let sort = sort.filter(|s| !s.is_empty());
        let limit = limit.filter(|n| *n > 0).map(|n| n as usize);
        // Without a sort, the page is the first matches: stop at its end.
        let enough = match (sort, limit) {
            (None, Some(n)) => (skip as usize).saturating_add(n),
            _ => usize::MAX,
        };
        // Matches by reference; only the page is copied.
        let mut hits: Vec<&Document> = Vec::new();
        for d in &c.docs {
            if hits.len() == enough {
                break;
            }
            if query::matches(d, filter)? {
                hits.push(d);
            }
        }
        if let Some(s) = sort {
            hits = query::sort_docs(hits, s);
        }
        let page = hits.into_iter().skip(skip as usize).take(limit.unwrap_or(usize::MAX));
        match projection.filter(|p| !p.is_empty()) {
            Some(p) => page.map(|d| query::project(d, p)).collect(),
            None => Ok(page.cloned().collect()),
        }
    }

    pub fn count(&self, ns: &Namespace, filter: &Document) -> CoreResult<u64> {
        let Some(c) = self.coll(ns) else { return Ok(0) };
        let mut n = 0;
        for d in &c.docs {
            if query::matches(d, filter)? {
                n += 1;
            }
        }
        Ok(n)
    }

    pub fn estimated_count(&self, ns: &Namespace) -> u64 {
        self.coll(ns).map_or(0, |c| c.docs.len() as u64)
    }

    pub fn insert(&mut self, ns: &Namespace, mut doc: Document) -> CoreResult<Bson> {
        let id = match doc.get("_id") {
            Some(id) => id.clone(),
            None => {
                let id = Bson::ObjectId(ObjectId::new());
                let mut with_id = doc! { "_id": id.clone() };
                with_id.extend(doc);
                doc = with_id;
                id
            }
        };
        let c = self.coll_mut(ns);
        if c.kind() == CollKind::View {
            return Err(view_refuses(ns));
        }
        if !c.accepts(&doc) {
            return Err(CoreError::Server { code: 121, msg: "Document failed validation".into() });
        }
        if c.has_id(&id) {
            return Err(CoreError::Server {
                code: 11000,
                msg: format!("E11000 duplicate key error collection: {ns} index: _id_ dup key: {{ _id: {} }}", crate::format::to_shell(&id, crate::format::Style::Display)),
            });
        }
        c.ids.entry(query::eq_hash(&id)).or_default().push(c.docs.len());
        c.docs.push(doc);
        Ok(id)
    }

    /// An unordered insert: every document is tried. Those refused for an
    /// existing `_id` are reported by position; the first other refusal
    /// (a validator's) is kept in `error` and the rest are still tried.
    pub fn insert_unordered(&mut self, ns: &Namespace, docs: Vec<Document>) -> CoreResult<crate::conn::Inserted> {
        if self.coll(ns).is_some_and(|c| c.kind() == CollKind::View) {
            return Err(view_refuses(ns));
        }
        let mut out = crate::conn::Inserted::default();
        for (i, d) in docs.into_iter().enumerate() {
            match self.insert(ns, d) {
                Ok(_) => out.inserted += 1,
                Err(CoreError::Server { code: 11000, .. }) => out.duplicates.push(i),
                Err(e) => {
                    out.error.get_or_insert(e);
                }
            }
        }
        Ok(out)
    }

    pub fn update(&mut self, ns: &Namespace, filter: &Document, update: &Document, many: bool, upsert: bool) -> CoreResult<WriteOutcome> {
        let mut out = WriteOutcome::default();
        // A missing collection matches nothing, and only an upsert creates it.
        if let Some(c) = self.existing_mut(ns) {
            let mut moved_id = false;
            let result: CoreResult<()> = (|| {
                for d in c.docs.iter_mut() {
                    if query::matches(d, filter)? {
                        out.matched += 1;
                        let before = d.clone();
                        query::apply_update(d, update)?;
                        if *d != before {
                            out.modified += 1;
                            moved_id |= d.get("_id") != before.get("_id");
                        }
                        if !many {
                            break;
                        }
                    }
                }
                Ok(())
            })();
            // ($set on a field of _id, say; a failed update may have
            // changed part of a document before it stopped.)
            if moved_id || result.is_err() {
                c.reindex();
            }
            result?;
        }
        if out.matched == 0 && upsert {
            let mut fresh = Document::new();
            for (k, v) in filter.iter().filter(|(k, _)| !k.starts_with('$')) {
                if !matches!(v, Bson::Document(d) if d.keys().any(|k| k.starts_with('$'))) {
                    paths::set_path(&mut fresh, k, v.clone())?;
                }
            }
            query::apply_update(&mut fresh, update)?;
            if let Ok(set) = update.get_document("$setOnInsert") {
                for (k, v) in set {
                    paths::set_path(&mut fresh, k, v.clone())?;
                }
            }
            out.inserted_id = Some(self.insert(ns, fresh)?);
        }
        Ok(out)
    }

    pub fn replace(&mut self, ns: &Namespace, filter: &Document, replacement: Document, upsert: bool) -> CoreResult<WriteOutcome> {
        if replacement.keys().any(|k| k.starts_with('$')) {
            return Err(CoreError::InvalidInput("replacement document must not contain update operators".into()));
        }
        let mut out = WriteOutcome::default();
        let docs = self.existing_mut(ns).map_or(&mut [][..], |c| &mut c.docs[..]);
        for d in docs.iter_mut() {
            if query::matches(d, filter)? {
                out.matched = 1;
                let id = d.get("_id").cloned();
                if let (Some(id), Some(new_id)) = (&id, replacement.get("_id"))
                    && !query::bson_eq(id, new_id)
                {
                    return Err(CoreError::Server { code: 66, msg: "the _id field cannot be changed".into() });
                }
                let mut next = Document::new();
                if let Some(id) = id {
                    next.insert("_id", id);
                }
                next.extend(replacement.into_iter().filter(|(k, _)| k != "_id"));
                if *d != next {
                    out.modified = 1;
                }
                *d = next;
                return Ok(out);
            }
        }
        if upsert {
            out.inserted_id = Some(self.insert(ns, replacement)?);
        }
        Ok(out)
    }

    pub fn delete(&mut self, ns: &Namespace, filter: &Document, many: bool) -> CoreResult<u64> {
        let Some(c) = self.existing_mut(ns) else { return Ok(0) };
        // Every match is found before anything goes, so a filter that fails
        // deletes nothing.
        let mut deleted = 0;
        let mut hit = Vec::with_capacity(c.docs.len());
        for d in &c.docs {
            let h = (many || deleted == 0) && query::matches(d, filter)?;
            deleted += u64::from(h);
            hit.push(h);
        }
        if deleted > 0 {
            let mut i = 0;
            c.docs.retain(|_| {
                i += 1;
                !hit[i - 1]
            });
            c.reindex();
        }
        Ok(deleted)
    }

    pub fn aggregate(&self, ns: &Namespace, pipeline: &[Document]) -> CoreResult<Vec<Document>> {
        // Leading $match stages (and a $limit after them) run on the
        // collection in place: only what they let through is copied.
        let stage = |s: &Document, name: &str| s.len() == 1 && s.contains_key(name);
        let matches: Vec<&Document> = pipeline.iter().take_while(|s| stage(s, "$match")).map(|s| s.get_document("$match")).collect::<Result<_, _>>().unwrap_or_default();
        let mut rest = &pipeline[matches.len()..];
        let limit = match rest.first() {
            Some(s) if stage(s, "$limit") => s.get("$limit").and_then(query::as_f64).filter(|n| *n >= 0.0).map(|n| {
                rest = &rest[1..];
                n as usize
            }),
            _ => None,
        };
        let mut docs = Vec::new();
        for d in self.coll(ns).map_or(&[][..], |c| &c.docs[..]) {
            if docs.len() == limit.unwrap_or(usize::MAX) {
                break;
            }
            let mut keep = true;
            for f in &matches {
                if !query::matches(d, f)? {
                    keep = false;
                    break;
                }
            }
            if keep {
                docs.push(d.clone());
            }
        }
        query::aggregate(docs, rest)
    }

    pub fn distinct(&self, ns: &Namespace, field: &str, filter: &Document) -> CoreResult<Vec<Bson>> {
        let mut out: Vec<Bson> = Vec::new();
        for d in self.find(ns, filter, None, None, 0, None)? {
            for v in paths::lookup(&d, field) {
                let items: Vec<&Bson> = match v {
                    Bson::Array(a) => a.iter().collect(),
                    other => vec![other],
                };
                for i in items {
                    if !out.iter().any(|x| query::bson_eq(x, i)) {
                        out.push(i.clone());
                    }
                }
            }
        }
        out.sort_by(query::total_cmp);
        Ok(out)
    }

    pub fn create_collection(&mut self, ns: &Namespace) -> CoreResult<()> {
        self.create_collection_with(ns, Document::new())
    }

    /// The `create` command with these options; `viewOn` makes a view.
    pub fn create_collection_with(&mut self, ns: &Namespace, options: Document) -> CoreResult<()> {
        if self.coll(ns).is_some() {
            return Err(CoreError::Server { code: 48, msg: format!("Collection {ns} already exists.") });
        }
        let mut coll = MockColl::new(Vec::new());
        coll.options = options;
        if coll.kind() == CollKind::View {
            coll.indexes.clear();
        }
        self.dbs.entry(ns.db.clone()).or_default().insert(ns.coll.clone(), coll);
        Ok(())
    }

    /// Report no uuid for this collection, as some servers do.
    #[cfg(test)]
    pub(crate) fn forget_uuid(&mut self, ns: &Namespace) {
        self.coll_mut(ns).uuid = None;
    }

    pub fn drop_collection(&mut self, ns: &Namespace) -> bool {
        self.dbs.get_mut(&ns.db).is_some_and(|colls| colls.remove(&ns.coll).is_some())
    }

    pub fn rename_collection(&mut self, ns: &Namespace, to: &str) -> CoreResult<()> {
        let target = Namespace::new(&ns.db, to);
        if self.coll(&target).is_some() {
            return Err(CoreError::Server { code: 48, msg: format!("target namespace exists: {target}") });
        }
        let coll = self
            .dbs
            .get_mut(&ns.db)
            .and_then(|c| c.remove(&ns.coll))
            .ok_or_else(|| CoreError::Server { code: 26, msg: format!("source namespace does not exist: {ns}") })?;
        self.dbs.entry(ns.db.clone()).or_default().insert(to.to_string(), coll);
        Ok(())
    }

    pub fn modify_index(&mut self, ns: &Namespace, name: &str, hidden: Option<bool>, ttl: Option<i64>) -> CoreResult<()> {
        let c = self.existing_mut(ns).ok_or_else(|| ns_not_found(ns))?;
        let idx = c
            .indexes
            .iter_mut()
            .find(|i| i.get_str("name").ok() == Some(name))
            .ok_or_else(|| CoreError::Server { code: 27, msg: format!("cannot find index {name}") })?;
        if let Some(h) = hidden {
            if name == "_id_" {
                return Err(CoreError::Server { code: 72, msg: "cannot hide the _id index".into() });
            }
            idx.insert("hidden", h);
        }
        if let Some(t) = ttl {
            if !idx.contains_key("expireAfterSeconds") {
                return Err(CoreError::Server { code: 72, msg: "no expireAfterSeconds field to update".into() });
            }
            idx.insert("expireAfterSeconds", t);
        }
        Ok(())
    }

    pub fn coll_stats(&self, ns: &Namespace) -> CoreResult<Document> {
        let c = self.coll(ns).ok_or_else(|| CoreError::Server { code: 26, msg: format!("ns not found: {ns}") })?;
        let size: i64 = c.docs.iter().map(|d| d.to_vec().map_or(0, |b| b.len() as i64)).sum();
        let count = c.docs.len() as i64;
        Ok(doc! {
            "ns": ns.to_string(),
            "count": count,
            "size": size,
            "avgObjSize": if count > 0 { size / count } else { 0 },
            "storageSize": size / 2,
            "nindexes": c.indexes.len() as i32,
            "totalIndexSize": c.indexes.len() as i64 * 4096,
            "indexSizes": c.indexes.iter().filter_map(|i| i.get_str("name").ok()).map(|n| (n.to_string(), Bson::Int64(4096))).collect::<Document>(),
            "indexNames": c.indexes.iter().filter_map(|i| i.get_str("name").ok().map(str::to_string)).collect::<Vec<_>>(),
            "capped": c.options.get_bool("capped").unwrap_or(false),
        })
    }

    pub fn validator(&self, ns: &Namespace) -> CoreResult<Document> {
        let c = self.coll(ns).ok_or_else(|| CoreError::Server { code: 26, msg: format!("ns not found: {ns}") })?;
        let o = &c.options;
        Ok(doc! {
            "validator": o.get_document("validator").cloned().unwrap_or_default(),
            "validationLevel": o.get_str("validationLevel").unwrap_or("strict"),
            "validationAction": o.get_str("validationAction").unwrap_or("error"),
        })
    }

    pub fn set_validator(&mut self, ns: &Namespace, validator: Document, level: &str, action: &str) -> CoreResult<()> {
        let o = &mut self.existing_mut(ns).ok_or_else(|| ns_not_found(ns))?.options;
        o.insert("validator", validator);
        o.insert("validationLevel", level);
        o.insert("validationAction", action);
        Ok(())
    }

    pub fn drop_database(&mut self, db: &str) -> bool {
        self.dbs.remove(db).is_some()
    }

    /// A few commands that the shell and the UI rely on.
    pub fn run_command(&mut self, db: &str, cmd: &Document) -> CoreResult<Document> {
        let name = cmd.keys().next().map(String::as_str).unwrap_or("");
        Ok(match name {
            "ping" => doc! { "ok": 1.0 },
            "buildInfo" | "buildinfo" => doc! { "version": "7.0.0-mock", "ok": 1.0 },
            "hello" | "isMaster" | "ismaster" => doc! { "isWritablePrimary": true, "me": "demo.local:27017", "maxWireVersion": 21, "ok": 1.0 },
            "listDatabases" => {
                let dbs: Vec<Bson> = self
                    .list_databases()
                    .into_iter()
                    .map(|d| Bson::Document(doc! { "name": d.name, "sizeOnDisk": d.size_on_disk as i64, "empty": false }))
                    .collect();
                doc! { "databases": dbs, "ok": 1.0 }
            }
            "dbStats" => {
                let colls = self.dbs.get(db).map_or(0, |c| c.len()) as i32;
                let objects: i64 = self.dbs.get(db).map_or(0, |c| c.values().map(|x| x.docs.len() as i64).sum());
                doc! { "db": db, "collections": colls, "objects": objects, "ok": 1.0 }
            }
            "connectionStatus" => doc! { "authInfo": { "authenticatedUsers": [] }, "ok": 1.0 },
            "serverStatus" => doc! {
                "host": "demo.local:27017",
                "version": "7.0.0-mock",
                "process": "mongod",
                "uptime": 86_400.0,
                "connections": { "current": 3, "available": 838_857 },
                "mem": { "resident": 128, "virtual": 2048 },
                "ok": 1.0,
            },
            "hostInfo" => doc! {
                "system": { "hostname": "demo.local", "numCores": 8, "memSizeMB": 16_384 },
                "os": { "type": "Linux", "name": "Demo OS" },
                "ok": 1.0,
            },
            "currentOp" => doc! { "inprog": [], "ok": 1.0 },
            "replSetGetStatus" => doc! {
                "set": "rs-demo",
                "members": [
                    { "_id": 0, "name": "demo.local:27017", "stateStr": "PRIMARY", "health": 1.0 },
                    { "_id": 1, "name": "demo-2.local:27017", "stateStr": "SECONDARY", "health": 1.0 },
                ],
                "ok": 1.0,
            },
            "collStats" => {
                let coll = cmd.get_str("collStats").unwrap_or_default();
                self.coll_stats(&Namespace::new(db, coll))?
            }
            other => return Err(CoreError::InvalidInput(format!("the demo server does not support the {other} command"))),
        })
    }

    /// Made-up demo data for --mock.
    pub fn demo() -> Self {
        let mut rng = StdRng::seed_from_u64(7);
        let mut s = Self::empty();
        let mut put = |db: &str, coll: &str, docs: Vec<Document>| {
            s.dbs.entry(db.into()).or_default().insert(coll.into(), MockColl::new(docs));
        };

        put("admin", "system.version", vec![doc! { "_id": "featureCompatibilityVersion", "version": "7.0" }]);

        put("demo-games", "advertisers", (1..=4).map(|i| {
            doc! {
                "_id": oid(&mut rng),
                "name": format!("Position {i}"),
                "img": [],
                "position": i,
                "bannerType": "advertisers",
            }
        }).collect());

        let providers = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel"];
        put("demo-games", "providers", providers.iter().enumerate().map(|(i, p)| {
            doc! {
                "_id": oid(&mut rng),
                "code": p.to_uppercase(),
                "name": format!("{} Gaming", capitalise(p)),
                "enabled": i % 3 != 0,
                "categories": ["slot", if i % 2 == 0 { "live" } else { "fish" }],
                "config": { "currency": "THB", "timeoutMs": 5000 + i as i32 * 500, "retry": { "max": 3, "backoffMs": 250 } },
                "rate": 0.95 + i as f64 / 100.0,
                "updatedAt": date(&mut rng),
            }
        }).collect());

        let bet_logs = (0..25_000).map(|i| {
            let amount = (rng.random_range(10..50_000) as f64) / 10.0;
            doc! {
                "_id": oid(&mut rng),
                "ref": format!("BET{:08}", i),
                "username": format!("player{:04}", rng.random_range(0..800)),
                "provider": providers[rng.random_range(0..providers.len())].to_uppercase(),
                "amount": amount,
                "payout": if rng.random_bool(0.4) { amount * rng.random_range(1..5) as f64 } else { 0.0 },
                "status": pick(&mut rng, &["settled", "pending", "cancelled"]),
                "round": i64::from(rng.random_range(1_000_000..9_999_999)),
                "createdAt": date(&mut rng),
            }
        }).collect();
        put("demo-games", "bet_logs", bet_logs);

        let users = (0..800).map(|i| {
            doc! {
                "_id": oid(&mut rng),
                "username": format!("player{i:04}"),
                "email": format!("player{i:04}@example.test"),
                "balance": bson::Decimal128::from_bytes(dec_bytes(&mut rng)),
                "vip": rng.random_bool(0.1),
                "tags": (0..rng.random_range(0..4)).map(|_| pick(&mut rng, &["new", "promo", "risk", "kyc"])).collect::<Vec<_>>(),
                "profile": { "country": pick(&mut rng, &["TH", "VN", "ID", "MY"]), "lang": "th" },
                "createdAt": date(&mut rng),
            }
        }).collect();
        put("demo-games", "users", users);

        // Pad the database out to ~300 collections to exercise long lists.
        let prefixes = ["game", "provider", "player", "bet", "agent", "banner", "broadcast", "promo", "report", "wallet", "session", "audit", "ai", "bo", "callback", "config"];
        let suffixes = ["logs", "history", "settings", "limits", "pending", "tokens", "detail", "summary", "queue", "cache", "events", "jobs", "usage", "paths", "messages", "snapshots", "backup", "archive", "stats", "rules"];
        'outer: for p in prefixes {
            for x in suffixes {
                let name = format!("{p}_{x}");
                if s.dbs.get("demo-games").is_some_and(|c| c.len() >= 294) {
                    break 'outer;
                }
                let n = rng.random_range(0..40);
                let docs = (0..n).map(|i| doc! { "_id": oid(&mut rng), "seq": i, "note": format!("{name} #{i}"), "at": date(&mut rng) }).collect();
                s.dbs.entry("demo-games".into()).or_default().entry(name).or_insert_with(|| MockColl::new(docs));
            }
        }

        let events = (0..5000).map(|i| {
            doc! {
                "_id": oid(&mut rng),
                "type": pick(&mut rng, &["login", "deposit", "withdraw", "spin"]),
                "seq": i,
                "meta": { "ip": format!("10.0.{}.{}", rng.random_range(0..255), rng.random_range(1..255)), "ua": "Mozilla/5.0" },
                "at": date(&mut rng),
            }
        }).collect();
        s.dbs.entry("analytics".into()).or_default().insert("events".into(), MockColl::new(events));
        s
    }
}

/// A server's answer to a write into a view.
fn view_refuses(ns: &Namespace) -> CoreError {
    CoreError::Server { code: 166, msg: format!("Namespace {ns} is a view, not a collection") }
}

/// A server's answer (NamespaceNotFound) to changing what a missing collection has.
fn ns_not_found(ns: &Namespace) -> CoreError {
    CoreError::Server { code: 26, msg: format!("ns not found: {ns}") }
}

fn pick<'a>(rng: &mut StdRng, items: &[&'a str]) -> &'a str {
    items[rng.random_range(0..items.len())]
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

fn oid(rng: &mut StdRng) -> ObjectId {
    let mut bytes = [0u8; 12];
    let ts: u32 = rng.random_range(1_690_000_000..1_760_000_000);
    bytes[..4].copy_from_slice(&ts.to_be_bytes());
    rng.fill(&mut bytes[4..]);
    ObjectId::from_bytes(bytes)
}

fn date(rng: &mut StdRng) -> bson::DateTime {
    bson::DateTime::from_millis(rng.random_range(1_690_000_000_000_i64..1_760_000_000_000))
}

/// Decimal128 bytes for a value with two decimal places, built through the
/// string parser so the encoding is always valid.
fn dec_bytes(rng: &mut StdRng) -> [u8; 16] {
    let text = format!("{}.{:02}", rng.random_range(0..100_000), rng.random_range(0..100));
    text.parse::<bson::Decimal128>().map(|d| d.bytes()).unwrap_or([0; 16])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_has_expected_shape() {
        let s = MockStore::demo();
        let names: Vec<_> = s.list_databases().into_iter().map(|d| d.name).collect();
        assert_eq!(names, ["admin", "analytics", "demo-games"]);
        assert_eq!(s.list_collections("demo-games").len(), 294);
        let ns = Namespace::new("demo-games", "bet_logs");
        assert_eq!(s.estimated_count(&ns), 25_000);
        let page = s.find(&ns, &doc! { "status": "pending" }, Some(&doc! { "amount": -1 }), None, 0, Some(10)).unwrap();
        assert_eq!(page.len(), 10);
        assert!(page.windows(2).all(|w| w[0].get_f64("amount").unwrap() >= w[1].get_f64("amount").unwrap()));
    }

    #[test]
    fn rename_and_validators() {
        let mut s = MockStore::empty();
        let ns = Namespace::new("t", "a");
        s.insert(&ns, doc! { "x": 1 }).unwrap();
        s.create_indexes(&ns, vec![doc! { "key": { "x": 1 } }]);
        s.insert(&Namespace::new("t", "b"), doc! { "x": 1 }).unwrap();
        assert!(matches!(s.rename_collection(&ns, "b"), Err(CoreError::Server { code: 48, .. })));
        s.rename_collection(&ns, "c").unwrap();
        assert_eq!(s.list_indexes(&Namespace::new("t", "c")).len(), 2, "indexes go along");
        assert!(s.coll(&ns).is_none());
        let c = Namespace::new("t", "c");
        assert_eq!(s.coll_stats(&c).unwrap().get_i64("count").unwrap(), 1);
        s.set_validator(&c, doc! { "x": { "$exists": true } }, "moderate", "warn").unwrap();
        assert_eq!(s.validator(&c).unwrap().get_str("validationLevel").unwrap(), "moderate");
    }

    #[test]
    fn stats_summary_and_index_changes() {
        let mut s = MockStore::empty();
        let ns = Namespace::new("t", "a");
        s.insert(&ns, doc! { "at": 1 }).unwrap();
        s.create_indexes(&ns, vec![doc! { "key": { "at": 1 }, "name": "ttl", "expireAfterSeconds": 60 }]);
        let sum = crate::api::CollSummary::from_stats(&s.coll_stats(&ns).unwrap());
        assert_eq!((sum.count, sum.indexes, sum.total_index_size), (1, 2, 8192));
        s.modify_index(&ns, "ttl", Some(true), Some(120)).unwrap();
        assert_eq!(s.list_indexes(&ns)[1].get_i64("expireAfterSeconds").ok().or(s.list_indexes(&ns)[1].get_i32("expireAfterSeconds").ok().map(i64::from)), Some(120));
        assert!(s.modify_index(&ns, "_id_", Some(true), None).is_err());
    }

    #[test]
    fn writes() {
        let mut s = MockStore::empty();
        let ns = Namespace::new("t", "c");
        let id = s.insert(&ns, doc! { "a": 1 }).unwrap();
        assert!(matches!(id, Bson::ObjectId(_)));
        assert!(s.insert(&ns, doc! { "_id": id.clone() }).is_err());
        let r = s.update(&ns, &doc! { "a": 1 }, &doc! { "$set": { "b": 2 } }, false, false).unwrap();
        assert_eq!((r.matched, r.modified), (1, 1));
        let r = s.replace(&ns, &doc! { "_id": id.clone() }, doc! { "c": 3 }, false).unwrap();
        assert_eq!(r.modified, 1);
        assert_eq!(s.find(&ns, &doc! {}, None, None, 0, None).unwrap(), vec![doc! { "_id": id, "c": 3 }]);
        let r = s.update(&ns, &doc! { "k": "x" }, &doc! { "$inc": { "n": 1 } }, false, true).unwrap();
        assert!(r.inserted_id.is_some());
        assert_eq!(s.count(&ns, &doc! { "k": "x", "n": 1 }).unwrap(), 1);
        assert_eq!(s.delete(&ns, &doc! {}, true).unwrap(), 2);
    }

    #[test]
    fn collection_options_kinds_and_views() {
        let mut s = MockStore::empty();
        let orders = Namespace::new("t", "orders");
        let view = Namespace::new("t", "v");
        let definition = doc! { "viewOn": "orders", "pipeline": [{ "$match": { "a": 1 } }] };
        s.create_collection_with(&orders, doc! { "validator": { "a": { "$exists": true } }, "capped": true, "size": 4096 }).unwrap();
        s.create_collection_with(&view, definition.clone()).unwrap();
        s.create_collection_with(&Namespace::new("t", "ts"), doc! { "timeseries": { "timeField": "at" } }).unwrap();
        assert!(s.create_collection_with(&orders, doc! {}).is_err(), "already exists");

        let specs = s.collection_specs("t");
        let kinds: Vec<(&str, &str)> = specs.iter().map(|d| (d.get_str("name").unwrap(), d.get_str("type").unwrap())).collect();
        assert_eq!(kinds, [("orders", "collection"), ("ts", "timeseries"), ("v", "view")]);
        assert_eq!(specs[2].get_document("options").unwrap(), &definition);
        let kinds: Vec<CollKind> = s.list_collections("t").into_iter().map(|c| c.kind).collect();
        assert_eq!(kinds, [CollKind::Collection, CollKind::Timeseries, CollKind::View]);
        assert!(s.collection_specs("nothing").is_empty());

        // The validator is one of the options, both ways.
        assert_eq!(s.validator(&orders).unwrap().get_document("validator").unwrap(), &doc! { "a": { "$exists": true } });
        s.set_validator(&orders, doc! { "b": 1 }, "moderate", "warn").unwrap();
        let options = s.collection_specs("t")[0].get_document("options").unwrap().clone();
        assert_eq!(options, doc! { "validator": { "b": 1 }, "capped": true, "size": 4096, "validationLevel": "moderate", "validationAction": "warn" });
        assert!(s.coll_stats(&orders).unwrap().get_bool("capped").unwrap());

        // A view has no indexes and takes no documents.
        assert!(s.list_indexes(&view).is_empty());
        assert!(matches!(s.insert(&view, doc! { "a": 1 }), Err(CoreError::Server { code: 166, .. })));
        assert!(matches!(s.insert_unordered(&view, vec![doc! { "a": 1 }]), Err(CoreError::Server { code: 166, .. })));
    }

    #[test]
    fn unordered_inserts_report_duplicate_ids() {
        let mut s = MockStore::empty();
        let ns = Namespace::new("t", "c");
        s.insert(&ns, doc! { "_id": 2, "old": true }).unwrap();
        let r = s.insert_unordered(&ns, vec![doc! { "_id": 1 }, doc! { "_id": 2 }, doc! { "_id": 3 }, doc! { "_id": 1 }]).unwrap();
        assert_eq!(r, crate::conn::Inserted { inserted: 2, duplicates: vec![1, 3], error: None, unconfirmed: false });
        assert_eq!(s.count(&ns, &doc! {}).unwrap(), 3);
        assert_eq!(s.count(&ns, &doc! { "old": true }).unwrap(), 1, "the existing document is untouched");

        s.create_indexes(&ns, vec![doc! { "key": { "a": 1 }, "name": "a_1", "unique": true }, doc! { "key": { "b": -1 } }, doc! { "key": { "a": 1 }, "name": "a_1" }]);
        let indexes = s.list_indexes(&ns);
        let names: Vec<&str> = indexes.iter().map(|i| i.get_str("name").unwrap()).collect();
        assert_eq!(names, ["_id_", "a_1", "b_-1"]);
        assert_eq!(indexes[1], doc! { "v": 2, "key": { "a": 1 }, "name": "a_1", "unique": true });
    }

    #[test]
    fn unordered_inserts_go_on_past_validation_failures() {
        let mut s = MockStore::empty();
        let ns = Namespace::new("t", "c");
        s.create_collection_with(&ns, doc! { "validator": { "$jsonSchema": { "required": ["a"] } } }).unwrap();
        let r = s.insert_unordered(&ns, vec![doc! { "_id": 1, "a": 1 }, doc! { "_id": 2 }, doc! { "_id": 1, "a": 1 }, doc! { "_id": 3, "a": 3 }, doc! { "_id": 4 }]).unwrap();
        assert_eq!((r.inserted, r.duplicates.clone()), (2, vec![2]));
        assert!(matches!(r.error, Some(CoreError::Server { code: 121, .. })), "{r:?}");
        assert_eq!(s.count(&ns, &doc! {}).unwrap(), 2);

        // Query-style validators too; "warn" and "off" let documents through.
        let q = Namespace::new("t", "q");
        s.create_collection_with(&q, doc! { "validator": { "a": { "$gt": 0 } } }).unwrap();
        assert!(matches!(s.insert(&q, doc! { "a": 0 }), Err(CoreError::Server { code: 121, .. })));
        s.insert(&q, doc! { "a": 1 }).unwrap();
        s.set_validator(&q, doc! { "a": { "$gt": 0 } }, "strict", "warn").unwrap();
        s.insert(&q, doc! { "a": 0 }).unwrap();
        s.set_validator(&q, doc! { "a": { "$gt": 0 } }, "off", "error").unwrap();
        s.insert(&q, doc! { "a": -1 }).unwrap();
        assert_eq!(s.count(&q, &doc! {}).unwrap(), 3);
    }

    /// A filter the demo server can't evaluate fails the delete before
    /// anything is deleted, as a server's does (it once emptied the
    /// collection).
    #[test]
    fn a_delete_whose_filter_fails_deletes_nothing() {
        let mut s = MockStore::empty();
        let ns = Namespace::new("t", "c");
        for i in 0..3 {
            s.insert(&ns, doc! { "_id": i }).unwrap();
        }
        assert!(s.delete(&ns, &doc! { "$where": "this.a" }, true).is_err());
        assert_eq!(s.count(&ns, &doc! {}).unwrap(), 3);
        assert!(s.delete(&ns, &doc! { "a": { "$near": [0, 0] } }, false).is_err());
        assert_eq!(s.count(&ns, &doc! {}).unwrap(), 3);
    }

    /// Like a server, only inserts, upserts and index or collection
    /// creation make a missing collection; other writes find nothing, and
    /// dropping or changing an index of one is "ns not found" (code 26).
    #[test]
    fn writes_to_a_missing_collection_create_nothing() {
        let mut s = MockStore::empty();
        let ns = Namespace::new("t", "gone");
        assert_eq!(s.delete(&ns, &doc! {}, true).unwrap(), 0);
        let r = s.update(&ns, &doc! {}, &doc! { "$set": { "a": 1 } }, true, false).unwrap();
        assert_eq!((r.matched, r.modified, r.inserted_id), (0, 0, None));
        let r = s.replace(&ns, &doc! { "_id": 1 }, doc! { "a": 1 }, false).unwrap();
        assert_eq!((r.matched, r.inserted_id), (0, None));
        assert!(matches!(s.drop_index(&ns, "a_1"), Err(CoreError::Server { code: 26, .. })), "{:?}", s.drop_index(&ns, "a_1"));
        assert!(matches!(s.modify_index(&ns, "a_1", Some(true), None), Err(CoreError::Server { code: 26, .. })));
        assert!(s.list_databases().is_empty() && s.list_collections("t").is_empty(), "nothing was created");

        let upserted = Namespace::new("t", "upserted");
        s.update(&upserted, &doc! { "k": 1 }, &doc! { "$set": { "a": 1 } }, false, true).unwrap();
        s.replace(&Namespace::new("t", "replaced"), &doc! { "_id": 1 }, doc! { "a": 1 }, true).unwrap();
        s.create_indexes(&Namespace::new("t", "indexed"), vec![doc! { "key": { "a": 1 } }]);
        let names: Vec<String> = s.list_collections("t").into_iter().map(|c| c.name).collect();
        assert_eq!(names, ["indexed", "replaced", "upserted"]);
    }

    #[test]
    fn collections_have_their_own_uuid() {
        let mut s = MockStore::empty();
        let a = Namespace::new("t", "a");
        s.insert(&a, doc! { "x": 1 }).unwrap();
        s.insert(&Namespace::new("t", "b"), doc! { "x": 1 }).unwrap();
        s.create_collection_with(&Namespace::new("t", "v"), doc! { "viewOn": "a", "pipeline": [] }).unwrap();
        let uuid = |s: &MockStore, name: &str| {
            let spec = s.collection_specs("t").into_iter().find(|d| d.get_str("name").ok() == Some(name))?;
            spec.get_document("info").ok()?.get("uuid").cloned()
        };
        let first = uuid(&s, "a").expect("a collection has a uuid");
        assert!(matches!(first, Bson::Binary(ref b) if b.subtype == bson::spec::BinarySubtype::Uuid), "{first:?}");
        assert!(uuid(&s, "b").is_some_and(|b| b != first), "each collection has its own");
        assert_eq!(uuid(&s, "v"), None, "a view has none, as on a real server");
        let c = Namespace::new("t", "c");
        s.rename_collection(&a, "c").unwrap();
        assert_eq!(uuid(&s, "c").as_ref(), Some(&first), "a rename keeps it");
        s.drop_collection(&c);
        s.insert(&c, doc! { "x": 1 }).unwrap();
        assert!(uuid(&s, "c").is_some_and(|u| u != first), "dropped and created again: another collection");
    }

    /// The `_id` index finds a duplicate as `bson_eq` does (1, 1.0 and
    /// NumberLong(1) are one `_id`), and follows deletes and an `_id` an
    /// update changed.
    #[test]
    fn the_id_index_follows_writes() {
        let mut s = MockStore::empty();
        let ns = Namespace::new("t", "c");
        s.insert(&ns, doc! { "_id": 1 }).unwrap();
        assert!(s.insert(&ns, doc! { "_id": 1.0 }).is_err() && s.insert(&ns, doc! { "_id": 1_i64 }).is_err());
        s.insert(&ns, doc! { "_id": { "a": 1, "b": "x" } }).unwrap();
        assert!(s.insert(&ns, doc! { "_id": { "a": 1.0, "b": "x" } }).is_err());
        s.insert(&ns, doc! { "_id": { "b": "x", "a": 1 } }).unwrap();
        s.insert(&ns, doc! { "_id": "s" }).unwrap();
        s.insert(&ns, doc! { "_id": Bson::Double(f64::NAN) }).unwrap();
        s.insert(&ns, doc! { "_id": Bson::Double(f64::NAN) }).unwrap();
        assert_eq!(s.delete(&ns, &doc! { "_id": 1 }, false).unwrap(), 1);
        s.insert(&ns, doc! { "_id": 1 }).unwrap();
        assert!(s.insert(&ns, doc! { "_id": "s" }).is_err(), "found where the delete moved it");
        s.insert(&ns, doc! { "_id": { "a": 2 } }).unwrap();
        s.update(&ns, &doc! { "_id": { "a": 2 } }, &doc! { "$set": { "_id.b": 3 } }, false, false).unwrap();
        s.insert(&ns, doc! { "_id": { "a": 2 } }).unwrap();
        assert!(s.insert(&ns, doc! { "_id": { "a": 2, "b": 3 } }).is_err());
    }

    /// A page without a sort stops at its last match, and an aggregate's
    /// leading $match and $limit run in place: both give what the general
    /// way gives.
    #[test]
    fn short_cuts_give_what_the_general_way_gives() {
        let s = MockStore::demo();
        let ns = Namespace::new("demo-games", "bet_logs");
        let all = s.find(&ns, &doc! {}, None, None, 0, None).unwrap();
        let pending: Vec<Document> = all.iter().filter(|d| d.get_str("status").ok() == Some("pending")).cloned().collect();
        assert_eq!(s.find(&ns, &doc! { "status": "pending" }, None, None, 30, Some(20)).unwrap(), pending[30..50]);
        assert_eq!(s.find(&ns, &doc! { "status": "pending" }, None, Some(&doc! { "ref": 1 }), 5, Some(2)).unwrap(), s.find(&ns, &doc! { "status": "pending" }, None, None, 5, Some(2)).unwrap().iter().map(|d| query::project(d, &doc! { "ref": 1 }).unwrap()).collect::<Vec<_>>());
        for pipeline in [
            vec![doc! { "$match": { "status": "pending" } }],
            vec![doc! { "$match": { "status": "pending" } }, doc! { "$limit": 10 }],
            vec![doc! { "$match": { "status": "pending" } }, doc! { "$match": { "provider": "ALPHA" } }, doc! { "$limit": 5 }, doc! { "$sort": { "amount": -1 } }],
            vec![doc! { "$limit": 3 }],
            vec![doc! { "$limit": -3 }],
            vec![doc! { "$group": { "_id": { "u": "$username", "p": "$provider" }, "n": { "$sum": 1 } } }],
            vec![doc! { "$sortByCount": "$status" }],
        ] {
            assert_eq!(s.aggregate(&ns, &pipeline).map_err(|e| e.to_string()), query::aggregate(all.clone(), &pipeline).map_err(|e| e.to_string()), "{pipeline:?}");
        }
    }

    /// Times the demo server's hot spots on its 25,000 bet_logs: an
    /// unordered insert of all of them (as a copy does), finds with and
    /// without a sort, and a $group.
    /// `cargo test -p rongo-core --release --lib -- --ignored --nocapture report_mock_time`
    #[test]
    #[ignore = "timing; run explicitly in release mode"]
    fn report_mock_time() {
        let mut s = MockStore::demo();
        let ns = Namespace::new("demo-games", "bet_logs");
        let docs = s.find(&ns, &doc! {}, None, None, 0, None).unwrap();
        let time = |what: &str, f: &mut dyn FnMut() -> usize| {
            let t = std::time::Instant::now();
            let n = f();
            println!("{what}: {:?} ({n})", t.elapsed());
        };
        time("insert_unordered 25k into an empty collection", &mut || s.insert_unordered(&Namespace::new("copy", "bets"), docs.clone()).unwrap().inserted as usize);
        time("insert_unordered 25k again (all duplicates)", &mut || s.insert_unordered(&Namespace::new("copy", "bets"), docs.clone()).unwrap().duplicates.len());
        time("find page of 50, no sort", &mut || s.find(&ns, &doc! {}, None, None, 100, Some(50)).unwrap().len());
        time("find page of 50, sorted by status, amount", &mut || s.find(&ns, &doc! {}, Some(&doc! { "status": 1, "amount": -1 }), None, 100, Some(50)).unwrap().len());
        time("find page of 50 with a filter", &mut || s.find(&ns, &doc! { "status": "pending" }, None, None, 0, Some(50)).unwrap().len());
        time("$group by username", &mut || s.aggregate(&ns, &[doc! { "$group": { "_id": "$username", "n": { "$sum": 1 } } }]).unwrap().len());
        time("$match + $limit 10", &mut || s.aggregate(&ns, &[doc! { "$match": { "status": "pending" } }, doc! { "$limit": 10 }]).unwrap().len());
    }
}
