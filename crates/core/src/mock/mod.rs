//! An in-memory stand-in for a MongoDB server, used by `rongo --mock` and by
//! tests that should not need a real server.

pub mod query;

use std::collections::BTreeMap;

use bson::oid::ObjectId;
use bson::{Bson, Document, doc};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use crate::api::{CollInfo, CollKind, DbInfo, Namespace, WriteOutcome};
use crate::error::{CoreError, CoreResult};

#[derive(Default)]
pub struct MockColl {
    pub docs: Vec<Document>,
    pub indexes: Vec<Document>,
    /// Stored for Add/Edit Validator; the demo server does not enforce it.
    pub validator: Option<(Document, String, String)>,
}

impl MockColl {
    fn new(docs: Vec<Document>) -> Self {
        Self { docs, indexes: vec![doc! { "v": 2, "key": { "_id": 1 }, "name": "_id_" }], validator: None }
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
            .map(|colls| colls.keys().map(|name| CollInfo { name: name.clone(), kind: CollKind::Collection }).collect())
            .unwrap_or_default()
    }

    fn coll(&self, ns: &Namespace) -> Option<&MockColl> {
        self.dbs.get(&ns.db)?.get(&ns.coll)
    }

    fn coll_mut(&mut self, ns: &Namespace) -> &mut MockColl {
        // Like the server, a collection created by a write has an _id index.
        self.dbs.entry(ns.db.clone()).or_default().entry(ns.coll.clone()).or_insert_with(|| MockColl::new(Vec::new()))
    }

    pub fn list_indexes(&self, ns: &Namespace) -> Vec<Document> {
        self.coll(ns).map(|c| c.indexes.clone()).unwrap_or_default()
    }

    pub fn create_index(&mut self, ns: &Namespace, keys: Document, name: Option<String>) -> String {
        let name = name.unwrap_or_else(|| {
            keys.iter().map(|(k, v)| format!("{k}_{}", crate::format::cell_text(v))).collect::<Vec<_>>().join("_")
        });
        let c = self.coll_mut(ns);
        if !c.indexes.iter().any(|i| i.get_str("name").ok() == Some(name.as_str())) {
            c.indexes.push(doc! { "v": 2, "key": keys, "name": name.clone() });
        }
        name
    }

    pub fn drop_index(&mut self, ns: &Namespace, name: &str) -> CoreResult<()> {
        let c = self.coll_mut(ns);
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
        let mut hits = Vec::new();
        for d in &c.docs {
            if query::matches(d, filter)? {
                hits.push(d.clone());
            }
        }
        if let Some(s) = sort.filter(|s| !s.is_empty()) {
            query::sort_docs(&mut hits, s);
        }
        let iter = hits.into_iter().skip(skip as usize);
        let page: Vec<Document> = match limit {
            Some(n) if n > 0 => iter.take(n as usize).collect(),
            _ => iter.collect(),
        };
        match projection.filter(|p| !p.is_empty()) {
            Some(p) => page.iter().map(|d| query::project(d, p)).collect(),
            None => Ok(page),
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
        if c.docs.iter().any(|d| d.get("_id").is_some_and(|x| query::bson_eq(x, &id))) {
            return Err(CoreError::Server {
                code: 11000,
                msg: format!("E11000 duplicate key error collection: {ns} index: _id_ dup key: {{ _id: {} }}", crate::format::to_shell(&id, crate::format::Style::Display)),
            });
        }
        c.docs.push(doc);
        Ok(id)
    }

    pub fn update(&mut self, ns: &Namespace, filter: &Document, update: &Document, many: bool, upsert: bool) -> CoreResult<WriteOutcome> {
        let mut out = WriteOutcome::default();
        let c = self.coll_mut(ns);
        for d in c.docs.iter_mut() {
            if query::matches(d, filter)? {
                out.matched += 1;
                let before = d.clone();
                query::apply_update(d, update)?;
                if *d != before {
                    out.modified += 1;
                }
                if !many {
                    break;
                }
            }
        }
        if out.matched == 0 && upsert {
            let mut fresh = Document::new();
            for (k, v) in filter.iter().filter(|(k, _)| !k.starts_with('$')) {
                if !matches!(v, Bson::Document(d) if d.keys().any(|k| k.starts_with('$'))) {
                    query::set_path(&mut fresh, k, v.clone())?;
                }
            }
            query::apply_update(&mut fresh, update)?;
            if let Ok(set) = update.get_document("$setOnInsert") {
                for (k, v) in set {
                    query::set_path(&mut fresh, k, v.clone())?;
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
        let c = self.coll_mut(ns);
        for d in c.docs.iter_mut() {
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
        let c = self.coll_mut(ns);
        let mut deleted = 0;
        let mut kept = Vec::with_capacity(c.docs.len());
        for d in std::mem::take(&mut c.docs) {
            if (many || deleted == 0) && query::matches(&d, filter)? {
                deleted += 1;
            } else {
                kept.push(d);
            }
        }
        c.docs = kept;
        Ok(deleted)
    }

    pub fn aggregate(&self, ns: &Namespace, pipeline: &[Document]) -> CoreResult<Vec<Document>> {
        let docs = self.coll(ns).map(|c| c.docs.clone()).unwrap_or_default();
        query::aggregate(docs, pipeline)
    }

    pub fn distinct(&self, ns: &Namespace, field: &str, filter: &Document) -> CoreResult<Vec<Bson>> {
        let mut out: Vec<Bson> = Vec::new();
        for d in self.find(ns, filter, None, None, 0, None)? {
            for v in query::lookup(&d, field) {
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
        if self.coll(ns).is_some() {
            return Err(CoreError::Server { code: 48, msg: format!("Collection {ns} already exists.") });
        }
        self.dbs.entry(ns.db.clone()).or_default().insert(ns.coll.clone(), MockColl::new(Vec::new()));
        Ok(())
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

    pub fn duplicate_collection(&mut self, ns: &Namespace, to: &str) -> CoreResult<()> {
        let target = Namespace::new(&ns.db, to);
        if self.coll(&target).is_some() {
            return Err(CoreError::Server { code: 48, msg: format!("Collection {target} already exists.") });
        }
        let src = self.coll(ns).ok_or_else(|| CoreError::Server { code: 26, msg: format!("ns not found: {ns}") })?;
        let copy = MockColl { docs: src.docs.clone(), indexes: src.indexes.clone(), validator: src.validator.clone() };
        self.dbs.entry(ns.db.clone()).or_default().insert(to.to_string(), copy);
        Ok(())
    }

    pub fn modify_index(&mut self, ns: &Namespace, name: &str, hidden: Option<bool>, ttl: Option<i64>) -> CoreResult<()> {
        let c = self.coll_mut(ns);
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
            "capped": false,
        })
    }

    pub fn validator(&self, ns: &Namespace) -> CoreResult<Document> {
        let c = self.coll(ns).ok_or_else(|| CoreError::Server { code: 26, msg: format!("ns not found: {ns}") })?;
        Ok(match &c.validator {
            Some((v, level, action)) => doc! { "validator": v.clone(), "validationLevel": level.clone(), "validationAction": action.clone() },
            None => doc! { "validator": {}, "validationLevel": "strict", "validationAction": "error" },
        })
    }

    pub fn set_validator(&mut self, ns: &Namespace, validator: Document, level: &str, action: &str) -> CoreResult<()> {
        if self.coll(ns).is_none() {
            return Err(CoreError::Server { code: 26, msg: format!("ns not found: {ns}") });
        }
        self.coll_mut(ns).validator = Some((validator, level.to_string(), action.to_string()));
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
    fn rename_duplicate_and_validators() {
        let mut s = MockStore::empty();
        let ns = Namespace::new("t", "a");
        s.insert(&ns, doc! { "x": 1 }).unwrap();
        s.create_index(&ns, doc! { "x": 1 }, None);
        s.duplicate_collection(&ns, "b").unwrap();
        assert!(s.duplicate_collection(&ns, "b").is_err());
        assert_eq!(s.list_indexes(&Namespace::new("t", "b")).len(), 2);
        s.rename_collection(&ns, "c").unwrap();
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
        s.create_index(&ns, doc! { "at": 1 }, Some("ttl".into()));
        s.coll_mut(&ns).indexes[1].insert("expireAfterSeconds", 60);
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
}
