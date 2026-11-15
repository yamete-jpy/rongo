//! Runs against a real server when `RONGO_TEST_HOST` (host:port) is set, e.g.
//!
//! ```sh
//! docker run -d --rm -p 127.0.0.1:27099:27017 \
//!   -e MONGO_INITDB_ROOT_USERNAME=root -e MONGO_INITDB_ROOT_PASSWORD=secret mongo:7
//! RONGO_TEST_HOST=127.0.0.1:27099 RONGO_TEST_USER=root RONGO_TEST_PASSWORD=secret cargo test -p rongo-core --test real_mongo
//! ```
//!
//! Without it, the tests return early.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rongo_core::bson::{Bson, Decimal128, Document, doc, oid::ObjectId};
use rongo_core::profile::{Auth, ConnectionProfile, Target};
use rongo_core::{Command, ConnectRequest, CoreError, CoreHandle, Event, FindSpec, Namespace, RequestId};

struct Server {
    host: String,
    user: Option<String>,
    password: Option<String>,
}

fn server() -> Option<Server> {
    Some(Server {
        host: std::env::var("RONGO_TEST_HOST").ok()?,
        user: std::env::var("RONGO_TEST_USER").ok(),
        password: std::env::var("RONGO_TEST_PASSWORD").ok(),
    })
}

fn profile(s: &Server) -> ConnectionProfile {
    let mut p = ConnectionProfile::new("test");
    p.target = Target::Manual { hosts: vec![s.host.clone()], replica_set: None, direct: true };
    if let Some(user) = &s.user {
        p.auth = Auth::Password { user: user.clone(), auth_db: "admin".into(), mechanism: None };
    }
    p
}

/// The request's terminal event; progress events are skipped.
fn wait(core: &CoreHandle, id: RequestId) -> Event {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(r) = core.try_recv() {
            if r.id == id && !matches!(r.event, Event::Progress { .. }) {
                return r.event;
            }
            continue;
        }
        assert!(Instant::now() < deadline, "no response for request {id}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn start() -> (tokio::runtime::Runtime, CoreHandle) {
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let core = CoreHandle::start(rt.handle().clone(), Arc::new(|| {}));
    (rt, core)
}

#[test]
fn full_round_trip_against_a_real_server() {
    let Some(s) = server() else { return };
    let (_rt, core) = start();
    let p = profile(&s);
    let conn = Some(p.conn_id());
    let req = ConnectRequest { profile: p, password: s.password.clone(), ssh_secret: None };
    let Event::Connected(info) = wait(&core, core.send(None, Command::Connect(Box::new(req)))) else { panic!("connect failed") };
    assert!(!info.version.is_empty() && info.version != "?", "{info:?}");
    assert_eq!(info.role, "Standalone");
    if s.user.is_some() {
        assert_eq!(info.user.as_deref(), s.user.as_deref());
    }

    let db = format!("rongo_test_{}", std::process::id());
    let ns = Namespace::new(&db, "things");
    assert!(matches!(wait(&core, core.send(conn, Command::CreateCollection { ns: ns.clone() })), Event::Done));

    let id = ObjectId::new();
    let original = doc! {
        "_id": id,
        "i": 1,
        "l": 9_007_199_254_740_993_i64,
        "f": 2.0,
        "d": "1.10".parse::<Decimal128>().unwrap(),
        "at": rongo_core::bson::DateTime::from_millis(1_760_000_000_123),
        "nested": { "tags": ["a", "b"] },
    };
    let ev = wait(&core, core.send(conn, Command::Insert { ns: ns.clone(), doc: original.clone() }));
    assert!(matches!(ev, Event::Written(ref w) if w.inserted_id == Some(Bson::ObjectId(id))), "{ev:?}");
    for i in 0..120 {
        wait(&core, core.send(conn, Command::Insert { ns: ns.clone(), doc: doc! { "n": i } }));
    }

    // Paging with has_more, and types preserved.
    let spec = FindSpec { sort: Some(doc! { "n": 1 }), ..FindSpec::all(ns.clone(), 50) };
    let Event::Page { docs, has_more, .. } = wait(&core, core.send(conn, Command::Find(spec.clone()))) else { panic!() };
    assert_eq!((docs.len(), has_more), (50, true));
    let Event::Page { docs, has_more, .. } = wait(&core, core.send(conn, Command::Find(FindSpec { skip: 100, ..spec }))) else { panic!() };
    assert_eq!((docs.len(), has_more), (21, false));
    let Event::Page { docs, .. } = wait(&core, core.send(conn, Command::Find(FindSpec { filter: doc! { "_id": id }, ..FindSpec::all(ns.clone(), 10) }))) else { panic!() };
    let docs: Vec<Document> = docs.into_iter().map(|d| Document::try_from(d).unwrap()).collect();
    assert_eq!(docs, vec![original.clone()]);

    let Event::Count(n) = wait(&core, core.send(conn, Command::Count { ns: ns.clone(), filter: doc! { "n": { "$gte": 100 } } })) else { panic!() };
    assert_eq!(n, 20);

    // Replace keeps _id; replacing a missing document is an error.
    let ev = wait(&core, core.send(conn, Command::Replace { ns: ns.clone(), id: Bson::ObjectId(id), doc: doc! { "_id": id, "i": 2 } }));
    assert!(matches!(ev, Event::Written(ref w) if w.modified == 1), "{ev:?}");
    let ev = wait(&core, core.send(conn, Command::Replace { ns: ns.clone(), id: Bson::ObjectId(ObjectId::new()), doc: doc! { "i": 3 } }));
    assert!(matches!(ev, Event::Failed(_)), "{ev:?}");

    // Duplicate key surfaces as a server error with code 11000.
    let ev = wait(&core, core.send(conn, Command::Insert { ns: ns.clone(), doc: doc! { "_id": id } }));
    assert!(matches!(ev, Event::Failed(CoreError::Server { code: 11000, .. })), "{ev:?}");

    // Browsing.
    let Event::Databases(dbs) = wait(&core, core.send(conn, Command::ListDatabases)) else { panic!() };
    assert!(dbs.iter().any(|d| d.name == db));
    let Event::Collections { items, .. } = wait(&core, core.send(conn, Command::ListCollections { db: db.clone() })) else { panic!() };
    assert_eq!(items.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["things"]);
    let Event::Indexes { items, .. } = wait(&core, core.send(conn, Command::ListIndexes { ns: ns.clone() })) else { panic!() };
    assert_eq!(items[0].get_str("name").unwrap(), "_id_");

    // Delete by ids.
    let ev = wait(&core, core.send(conn, Command::Delete { ns: ns.clone(), ids: vec![Bson::ObjectId(id), Bson::Null] }));
    assert!(matches!(ev, Event::Written(ref w) if w.deleted == 1), "{ev:?}");

    assert!(matches!(wait(&core, core.send(conn, Command::DropDatabase { db: db.clone() })), Event::Done));
    assert!(matches!(wait(&core, core.send(conn, Command::Disconnect)), Event::Disconnected));
}

#[test]
fn wrong_password_is_an_auth_error() {
    let Some(s) = server() else { return };
    if s.user.is_none() {
        return;
    }
    let (_rt, core) = start();
    let req = ConnectRequest { profile: profile(&s), password: Some("definitely-wrong".into()), ssh_secret: None };
    let ev = wait(&core, core.send(None, Command::Connect(Box::new(req))));
    assert!(matches!(ev, Event::Failed(CoreError::Auth(_))), "{ev:?}");
}

#[test]
fn unreachable_host_fails_quickly() {
    if server().is_none() {
        return;
    }
    let (_rt, core) = start();
    let mut p = ConnectionProfile::new("nowhere");
    p.target = Target::Uri { uri: "mongodb://127.0.0.1:1/?serverSelectionTimeoutMS=1500&directConnection=true".into() };
    let started = Instant::now();
    let ev = wait(&core, core.send(None, Command::Connect(Box::new(ConnectRequest { profile: p, password: None, ssh_secret: None }))));
    assert!(matches!(ev, Event::Failed(CoreError::Timeout(_) | CoreError::Network(_))), "{ev:?}");
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn collection_admin_and_transfer_against_a_real_server() {
    use rongo_core::{ExportFormat, ExportSpec, ImportFormat};
    let Some(s) = server() else { return };
    let (_rt, core) = start();
    let p = profile(&s);
    let conn = Some(p.conn_id());
    let req = ConnectRequest { profile: p, password: s.password.clone(), ssh_secret: None };
    assert!(matches!(wait(&core, core.send(None, Command::Connect(Box::new(req)))), Event::Connected(_)));
    let db = format!("rongo_admin_{}", std::process::id());
    let ns = Namespace::new(&db, "src");
    let send = |cmd| wait(&core, core.send(conn, cmd));

    let docs = (0..10).map(|i| doc! { "i": i, "even": i % 2 == 0, "tags": ["x"] }).collect();
    assert!(matches!(send(Command::InsertMany { ns: ns.clone(), docs }), Event::Written(ref w) if w.inserted == 10));
    let ev = send(Command::UpdateMany { ns: ns.clone(), filter: doc! { "even": true }, update: doc! { "$set": { "flag": 1 } } });
    assert!(matches!(ev, Event::Written(ref w) if w.matched == 5 && w.modified == 5), "{ev:?}");
    let ev = send(Command::CreateIndex { ns: ns.clone(), keys: doc! { "i": 1 }, options: Some(doc! { "name": "i_1", "unique": true }) });
    assert!(matches!(ev, Event::Document(ref d) if d.get_str("createdIndex").ok() == Some("i_1")), "{ev:?}");

    let ev = send(Command::SetValidator { ns: ns.clone(), validator: doc! { "i": { "$type": "int" } }, level: "moderate".into(), action: "warn".into() });
    assert!(matches!(ev, Event::Done), "{ev:?}");
    let Event::Document(v) = send(Command::GetValidator { ns: ns.clone() }) else { panic!() };
    assert_eq!((v.get_str("validationLevel").unwrap(), v.get_str("validationAction").unwrap()), ("moderate", "warn"));

    let Event::Document(stats) = send(Command::CollStats { ns: ns.clone() }) else { panic!() };
    assert!(stats.get_document("storageStats").is_ok(), "{stats:?}");
    let Event::Document(status) = send(Command::RunCommand { db: "admin".into(), cmd: doc! { "serverStatus": 1 } }) else { panic!() };
    assert!(status.get_str("version").is_ok());

    assert!(matches!(send(Command::DuplicateCollection { ns: ns.clone(), to: "dup".into() }), Event::Done));
    let dup = Namespace::new(&db, "dup");
    let Event::Indexes { items, .. } = send(Command::ListIndexes { ns: dup.clone() }) else { panic!() };
    assert!(items.iter().any(|i| i.get_str("name").ok() == Some("i_1") && i.get_bool("unique").ok() == Some(true)), "{items:?}");
    let Event::Count(n) = send(Command::Count { ns: dup.clone(), filter: doc! { "flag": 1 } }) else { panic!() };
    assert_eq!(n, 5);

    assert!(matches!(send(Command::RenameCollection { ns: dup.clone(), to: "renamed".into() }), Event::Done));
    assert!(matches!(send(Command::RenameCollection { ns: dup.clone(), to: "again".into() }), Event::Failed(_)));
    let renamed = Namespace::new(&db, "renamed");
    assert!(matches!(send(Command::DropIndex { ns: renamed.clone(), name: "i_1".into() }), Event::Done));
    let ev = send(Command::DeleteWhere { ns: renamed.clone(), filter: doc! { "i": { "$gte": 8 } }, many: true });
    assert!(matches!(ev, Event::Written(ref w) if w.deleted == 2), "{ev:?}");
    let ev = send(Command::ClearCollection { ns: renamed.clone() });
    assert!(matches!(ev, Event::Written(ref w) if w.deleted == 8), "{ev:?}");

    let path = std::env::temp_dir().join(format!("rongo-export-{}.jsonl", std::process::id()));
    let spec = ExportSpec { ns: ns.clone(), filter: doc! { "even": false }, sort: Some(doc! { "i": 1 }), projection: None, format: ExportFormat::JsonLines, path: path.clone(), fields: None };
    assert!(matches!(send(Command::Export(Box::new(spec))), Event::Exported { count: 5, .. }));
    let ev = send(Command::Import { ns: renamed.clone(), path: path.clone(), format: ImportFormat::Auto });
    assert!(matches!(ev, Event::Written(ref w) if w.inserted == 5), "{ev:?}");
    // Again: every _id is there already, so all are left out and counted.
    let ev = send(Command::Import { ns: renamed.clone(), path: path.clone(), format: ImportFormat::Auto });
    assert!(matches!(ev, Event::Written(ref w) if w.inserted == 0 && w.duplicates == 5 && w.warning.is_none()), "{ev:?}");
    std::fs::remove_file(path).ok();

    assert!(matches!(send(Command::DropDatabase { db }), Event::Done));
}

/// Duplicate Collection keeps the collection's options (capped, collation,
/// validator with its level and action), copies documents from before the
/// validator, keeps indexes, and refuses a view.
#[test]
fn duplicate_keeps_options_against_a_real_server() {
    let Some(s) = server() else { return };
    let (_rt, core) = start();
    let p = profile(&s);
    let conn = Some(p.conn_id());
    let req = ConnectRequest { profile: p, password: s.password.clone(), ssh_secret: None };
    assert!(matches!(wait(&core, core.send(None, Command::Connect(Box::new(req)))), Event::Connected(_)));
    let db = format!("rongo_dup_{}", std::process::id());
    let send = |cmd| wait(&core, core.send(conn, cmd));
    let create = |coll: &str, options: Document| {
        let mut cmd = doc! { "create": coll };
        cmd.extend(options);
        send(Command::RunCommand { db: db.clone(), cmd })
    };
    let ns = Namespace::new(&db, "orders");
    assert!(matches!(create("orders", doc! { "capped": true, "size": 1_048_576, "collation": { "locale": "fr", "strength": 2 } }), Event::Document(_)));
    let docs = (0..2500).map(|i| doc! { "_id": i, "b": i }).collect();
    assert!(matches!(send(Command::InsertMany { ns: ns.clone(), docs }), Event::Written(ref w) if w.inserted == 2500));
    let ev = send(Command::SetValidator { ns: ns.clone(), validator: doc! { "a": { "$exists": true } }, level: "moderate".into(), action: "error".into() });
    assert!(matches!(ev, Event::Done), "{ev:?}");
    let ev = send(Command::CreateIndex { ns: ns.clone(), keys: doc! { "b": 1 }, options: Some(doc! { "unique": true }) });
    assert!(matches!(ev, Event::Document(_)), "{ev:?}");

    assert!(matches!(send(Command::DuplicateCollection { ns: ns.clone(), to: "copy".into() }), Event::Done));
    let Event::Document(list) = send(Command::RunCommand { db: db.clone(), cmd: doc! { "listCollections": 1 } }) else { panic!() };
    let specs = list.get_document("cursor").unwrap().get_array("firstBatch").unwrap().clone();
    let options = |name: &str| specs.iter().filter_map(Bson::as_document).find(|s| s.get_str("name").ok() == Some(name)).and_then(|s| s.get_document("options").ok()).cloned().unwrap();
    let (from, to) = (options("orders"), options("copy"));
    for k in ["capped", "size", "collation", "validator", "validationLevel", "validationAction"] {
        assert_eq!(to.get(k), from.get(k), "{k}: {from} / {to}");
    }
    let copy = Namespace::new(&db, "copy");
    assert!(matches!(send(Command::Count { ns: copy.clone(), filter: doc! { "b": { "$gte": 0 } } }), Event::Count(2500)));
    let Event::Indexes { items, .. } = send(Command::ListIndexes { ns: copy.clone() }) else { panic!() };
    assert!(items.iter().any(|i| i.get_str("name").ok() == Some("b_1") && i.get_bool("unique").ok() == Some(true)), "{items:?}");

    assert!(matches!(create("v", doc! { "viewOn": "orders", "pipeline": [] }), Event::Document(_)));
    let ev = send(Command::DuplicateCollection { ns: Namespace::new(&db, "v"), to: "v2".into() });
    assert!(matches!(ev, Event::Failed(CoreError::InvalidInput(ref m)) if m.contains("view")), "{ev:?}");

    // A time series stays one.
    assert!(matches!(create("ts", doc! { "timeseries": { "timeField": "at", "metaField": "m", "granularity": "minutes" } }), Event::Document(_)));
    let docs = (0..10).map(|i| doc! { "at": rongo_core::bson::DateTime::from_millis(1_700_000_000_000 + i * 60_000), "m": "a", "v": i }).collect();
    assert!(matches!(send(Command::InsertMany { ns: Namespace::new(&db, "ts"), docs }), Event::Written(ref w) if w.inserted == 10));
    assert!(matches!(send(Command::DuplicateCollection { ns: Namespace::new(&db, "ts"), to: "ts2".into() }), Event::Done));
    let Event::Collections { items, .. } = send(Command::ListCollections { db: db.clone() }) else { panic!() };
    assert!(items.iter().any(|c| c.name == "ts2" && c.kind == rongo_core::CollKind::Timeseries), "{items:?}");
    assert!(matches!(send(Command::Count { ns: Namespace::new(&db, "ts2"), filter: doc! { "m": "a" } }), Event::Count(10)));
    assert!(matches!(send(Command::DropDatabase { db }), Event::Done));
}

#[test]
fn index_manager_against_a_real_server() {
    let Some(s) = server() else { return };
    let (_rt, core) = start();
    let p = profile(&s);
    let conn = Some(p.conn_id());
    let req = ConnectRequest { profile: p, password: s.password.clone(), ssh_secret: None };
    assert!(matches!(wait(&core, core.send(None, Command::Connect(Box::new(req)))), Event::Connected(_)));
    let db = format!("rongo_idx_{}", std::process::id());
    let ns = Namespace::new(&db, "events");
    let send = |cmd| wait(&core, core.send(conn, cmd));
    send(Command::InsertMany { ns: ns.clone(), docs: vec![doc! { "at": rongo_core::bson::DateTime::now(), "k": 1 }] });
    send(Command::CreateIndex { ns: ns.clone(), keys: doc! { "at": 1 }, options: Some(doc! { "name": "at_ttl", "expireAfterSeconds": 3600 }) });
    send(Command::CreateIndex { ns: ns.clone(), keys: doc! { "k": 1, "at": -1 }, options: Some(doc! { "name": "k_at" }) });

    let Event::IndexDetails { items, .. } = send(Command::IndexDetails { ns: ns.clone() }) else { panic!() };
    assert_eq!(items.iter().map(|i| i.spec.get_str("name").unwrap()).collect::<Vec<_>>(), ["_id_", "at_ttl", "k_at"]);
    assert!(items.iter().all(|i| i.size.is_some_and(|s| s > 0)), "{items:?}");
    assert!(items.iter().all(|i| i.ops.is_some()), "root can read $indexStats: {items:?}");

    let ev = send(Command::ModifyIndex { ns: ns.clone(), name: "at_ttl".into(), hidden: None, expire_after_seconds: Some(60) });
    assert!(matches!(ev, Event::Done), "{ev:?}");
    let ev = send(Command::ModifyIndex { ns: ns.clone(), name: "k_at".into(), hidden: Some(true), expire_after_seconds: None });
    assert!(matches!(ev, Event::Done), "{ev:?}");
    let Event::IndexDetails { items, .. } = send(Command::IndexDetails { ns: ns.clone() }) else { panic!() };
    let ttl = items.iter().find(|i| i.spec.get_str("name").ok() == Some("at_ttl")).unwrap();
    assert_eq!(ttl.spec.get("expireAfterSeconds").and_then(|v| v.as_i32().map(i64::from).or(v.as_i64())), Some(60));
    assert_eq!(items.iter().find(|i| i.spec.get_str("name").ok() == Some("k_at")).unwrap().spec.get_bool("hidden").ok(), Some(true));
    assert!(matches!(send(Command::ModifyIndex { ns: ns.clone(), name: "_id_".into(), hidden: Some(true), expire_after_seconds: None }), Event::Failed(_)));
    send(Command::DropDatabase { db });
}

#[test]
fn update_dialog_options_against_a_real_server() {
    use rongo_core::UpdateBody;
    let Some(s) = server() else { return };
    let (_rt, core) = start();
    let p = profile(&s);
    let conn = Some(p.conn_id());
    let req = ConnectRequest { profile: p, password: s.password.clone(), ssh_secret: None };
    assert!(matches!(wait(&core, core.send(None, Command::Connect(Box::new(req)))), Event::Connected(_)));
    let db = format!("rongo_upd_{}", std::process::id());
    let ns = Namespace::new(&db, "t");
    let send = |cmd| wait(&core, core.send(conn, cmd));
    send(Command::InsertMany { ns: ns.clone(), docs: (0..4).map(|i| doc! { "_id": i, "n": i, "chatId": 1_i64 }).collect() });
    let ids = rongo_core::bson::Bson::Array((0..3).map(rongo_core::bson::Bson::Int32).collect());
    let selected = doc! { "_id": { "$in": ids } };

    // Multi on: every selected document; NumberLong stays Int64.
    let ev = send(Command::Update { ns: ns.clone(), filter: selected.clone(), update: UpdateBody::Doc(doc! { "$set": { "chatId": -1_002_322_629_941_i64 } }), many: true, upsert: false });
    assert!(matches!(ev, Event::Written(ref w) if w.matched == 3 && w.modified == 3), "{ev:?}");
    let Event::Count(n) = send(Command::Count { ns: ns.clone(), filter: doc! { "chatId": -1_002_322_629_941_i64 } }) else { panic!() };
    assert_eq!(n, 3);
    // Multi off: only the first match.
    let ev = send(Command::Update { ns: ns.clone(), filter: selected.clone(), update: UpdateBody::Doc(doc! { "$inc": { "n": 10 } }), many: false, upsert: false });
    assert!(matches!(ev, Event::Written(ref w) if w.matched == 1), "{ev:?}");
    // Pipeline update.
    let ev = send(Command::Update { ns: ns.clone(), filter: doc! { "_id": 3 }, update: UpdateBody::Pipeline(vec![doc! { "$set": { "double": { "$multiply": ["$n", 2] } } }]), many: true, upsert: false });
    assert!(matches!(ev, Event::Written(ref w) if w.modified == 1), "{ev:?}");
    // Upsert.
    let ev = send(Command::Update { ns: ns.clone(), filter: doc! { "_id": 99 }, update: UpdateBody::Doc(doc! { "$set": { "n": 1 } }), many: false, upsert: true });
    assert!(matches!(ev, Event::Written(ref w) if w.inserted_id.is_some()), "{ev:?}");
    // Delete one versus many.
    let ev = send(Command::DeleteWhere { ns: ns.clone(), filter: selected.clone(), many: false });
    assert!(matches!(ev, Event::Written(ref w) if w.deleted == 1), "{ev:?}");
    let ev = send(Command::DeleteWhere { ns: ns.clone(), filter: selected, many: true });
    assert!(matches!(ev, Event::Written(ref w) if w.deleted == 2), "{ev:?}");
    send(Command::DropDatabase { db });
}

/// Every option the Add Index dialog can set reaches the server.
#[test]
fn index_options_reach_the_server() {
    let Some(s) = server() else { return };
    let (_rt, core) = start();
    let p = profile(&s);
    let conn = Some(p.conn_id());
    let req = ConnectRequest { profile: p, password: s.password.clone(), ssh_secret: None };
    assert!(matches!(wait(&core, core.send(None, Command::Connect(Box::new(req)))), Event::Connected(_)));
    let db = format!("rongo_idxopt_{}", std::process::id());
    let ns = Namespace::new(&db, "t");
    let send = |cmd| wait(&core, core.send(conn, cmd));
    send(Command::CreateCollection { ns: ns.clone() });
    let cases = [
        (doc! { "title": "text", "body": "text" }, doc! { "weights": { "title": 10 }, "default_language": "english", "language_override": "lang" }),
        (doc! { "loc": "2dsphere" }, doc! { "2dsphereIndexVersion": 3 }),
        (doc! { "grid": "2d" }, doc! { "bits": 20, "min": -500.0, "max": 500.0 }),
        (doc! { "u": "hashed" }, doc! {}),
        (doc! { "name": 1 }, doc! { "collation": { "locale": "th", "strength": 2, "numericOrdering": true }, "hidden": true }),
        (doc! { "at": 1 }, doc! { "expireAfterSeconds": 3600, "partialFilterExpression": { "kind": { "$eq": "temp" } } }),
        (doc! { "opt": 1 }, doc! { "sparse": true }),
        (doc! { "code": 1, "region": -1 }, doc! { "unique": true }),
    ];
    for (keys, options) in &cases {
        let ev = send(Command::CreateIndex { ns: ns.clone(), keys: keys.clone(), options: Some(options.clone()) });
        assert!(matches!(ev, Event::Document(_)), "{keys:?} {options:?}: {ev:?}");
    }
    let Event::Indexes { items, .. } = send(Command::ListIndexes { ns: ns.clone() }) else { panic!() };
    let find = |name: &str| items.iter().find(|i| i.get_str("name").ok() == Some(name)).unwrap_or_else(|| panic!("{name} missing: {items:?}")).clone();
    assert_eq!(find("title_text_body_text").get_document("weights").unwrap().get("title").and_then(|v| v.as_i32()), Some(10));
    assert_eq!(find("title_text_body_text").get_str("language_override").unwrap(), "lang");
    assert_eq!(find("grid_2d").get_i32("bits").unwrap(), 20);
    assert_eq!(find("name_1").get_document("collation").unwrap().get_str("locale").unwrap(), "th");
    assert!(find("name_1").get_bool("hidden").unwrap());
    assert!(find("at_1").get_document("partialFilterExpression").is_ok());
    assert!(find("code_1_region_-1").get_bool("unique").unwrap());
    send(Command::DropDatabase { db });
}

/// Copy/Paste Collections between two databases: options, kinds, indexes and
/// documents arrive; pasting again with "skip existing _id" adds nothing.
/// Runs on 4.4 (the oldest supported) too: time series, new in 5.0, are
/// left out there.
#[test]
fn copy_collections_against_a_real_server() {
    use rongo_core::{CollKind, CopyItem, CopyOptions, IfExists, OnDuplicateId};
    let Some(s) = server() else { return };
    let (_rt, core) = start();
    let p = profile(&s);
    let id = p.conn_id();
    let conn = Some(id);
    let req = ConnectRequest { profile: p, password: s.password.clone(), ssh_secret: None };
    let ev = wait(&core, core.send(None, Command::Connect(Box::new(req))));
    let Event::Connected(info) = ev else { panic!("{ev:?}") };
    let series = info.version.split('.').next().and_then(|m| m.parse::<u32>().ok()).is_some_and(|major| major >= 5);
    let (from, to) = (format!("rongo_copy_from_{}", std::process::id()), format!("rongo_copy_to_{}", std::process::id()));
    let send = |cmd| wait(&core, core.send(conn, cmd));
    let run = |db: &str, cmd: Document| {
        let ev = send(Command::RunCommand { db: db.into(), cmd });
        let Event::Document(d) = ev else { panic!("{ev:?}") };
        d
    };
    let insert = |coll: &str, docs: Vec<Document>| {
        let ev = send(Command::InsertMany { ns: Namespace::new(&from, coll), docs });
        assert!(matches!(ev, Event::Written(_)), "{ev:?}");
    };

    // The source: a validated, indexed collection; a capped one; a time
    // series; a view; and names with a dot and in Thai.
    let validator = doc! { "$jsonSchema": { "required": ["a"] } };
    run(&from, doc! { "create": "orders", "validator": validator.clone() });
    insert("orders", (0..2500).map(|i| doc! { "_id": i, "a": i }).collect());
    let ev = send(Command::CreateIndex { ns: Namespace::new(&from, "orders"), keys: doc! { "a": 1 }, options: Some(doc! { "name": "a_1", "unique": true }) });
    assert!(matches!(ev, Event::Document(_)), "{ev:?}");
    run(&from, doc! { "create": "capped", "capped": true, "size": 1_048_576, "max": 100 });
    insert("capped", (0..50).map(|i| doc! { "i": i }).collect());
    if series {
        run(&from, doc! { "create": "ts", "timeseries": { "timeField": "at", "metaField": "m", "granularity": "minutes" } });
        let start_ms = 1_760_000_000_000_i64;
        insert("ts", (0..30).map(|i| doc! { "at": rongo_core::bson::DateTime::from_millis(start_ms + i * 60_000), "m": i % 3, "v": i }).collect());
    }
    let pipeline = vec![doc! { "$match": { "a": { "$lt": 10 } } }];
    run(&from, doc! { "create": "v", "viewOn": "orders", "pipeline": pipeline.clone() });
    insert("fs.files", (0..3).map(|i| doc! { "n": i }).collect());
    insert("ลูกค้า", (0..3).map(|i| doc! { "n": i }).collect());

    let names: Vec<&str> = ["orders", "capped", "ts", "v", "fs.files", "ลูกค้า"].into_iter().filter(|n| series || *n != "ts").collect();
    let documents = |name: &str| match name {
        "orders" => 2500,
        "capped" => 50,
        "ts" => 30,
        "v" => 0,
        _ => 3,
    };
    let items = |if_exists: IfExists| names.iter().map(|n| CopyItem { src: Namespace::new(&from, *n), dst: Namespace::new(&to, *n), if_exists }).collect::<Vec<_>>();
    let skip = IfExists::Append(OnDuplicateId::Skip);
    let ev = send(Command::CopyCollections { to: id, items: items(skip), options: CopyOptions::default() });
    let Event::Copied(report) = ev else { panic!("{ev:?}") };
    assert!(!report.cancelled && report.items.iter().all(|i| i.error.is_none()), "{report:?}");
    assert_eq!(report.items.iter().map(|i| i.inserted).collect::<Vec<_>>(), names.iter().map(|n| documents(n)).collect::<Vec<u64>>());

    // Kinds and options.
    let Event::Collections { items: colls, .. } = send(Command::ListCollections { db: to.clone() }) else { panic!() };
    let kind = |name: &str| colls.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("{name} missing: {colls:?}")).kind.clone();
    for name in &names {
        let want = match *name {
            "ts" => CollKind::Timeseries,
            "v" => CollKind::View,
            _ => CollKind::Collection,
        };
        assert_eq!(kind(name), want, "{name}");
    }
    let listed = run(&to, doc! { "listCollections": 1 });
    let specs = listed.get_document("cursor").unwrap().get_array("firstBatch").unwrap().clone();
    let options = |name: &str| {
        specs.iter().filter_map(|s| s.as_document()).find(|s| s.get_str("name").ok() == Some(name)).unwrap().get_document("options").unwrap().clone()
    };
    assert_eq!(options("orders").get_document("validator").unwrap(), &validator);
    assert!(options("capped").get_bool("capped").unwrap());
    assert_eq!(options("capped").get("max").and_then(|m| m.as_i64().or(m.as_i32().map(i64::from))), Some(100));
    if series {
        let ts = options("ts").get_document("timeseries").unwrap().clone();
        assert_eq!((ts.get_str("timeField").unwrap(), ts.get_str("metaField").unwrap(), ts.get_str("granularity").unwrap()), ("at", "m", "minutes"));
    }
    assert_eq!(options("v").get_str("viewOn").unwrap(), "orders");
    assert_eq!(options("v").get_array("pipeline").unwrap(), &pipeline.iter().cloned().map(Bson::Document).collect::<Vec<_>>());

    // Indexes and counts.
    let Event::Indexes { items: indexes, .. } = send(Command::ListIndexes { ns: Namespace::new(&to, "orders") }) else { panic!() };
    assert!(indexes.iter().any(|i| i.get_str("name").ok() == Some("a_1") && i.get_bool("unique").ok() == Some(true)), "{indexes:?}");
    let count = |db: &str, coll: &str| {
        let Event::Count(n) = send(Command::Count { ns: Namespace::new(db, coll), filter: doc! { "_id": { "$exists": true } } }) else { panic!() };
        n
    };
    for name in &names {
        assert_eq!(count(&to, name), count(&from, name), "{name}");
    }

    // Pasting again: every document is already there. A time series has no
    // unique `_id` (the server would take its documents again), so appending
    // to one is refused; Replace still works. A view takes no documents, so
    // appending one is refused too.
    let ev = send(Command::CopyCollections { to: id, items: items(skip), options: CopyOptions::default() });
    let Event::Copied(again) = ev else { panic!("{ev:?}") };
    for (name, r) in names.iter().zip(&again.items) {
        let refused = match *name {
            "ts" => Some("time series"),
            "v" => Some("view"),
            _ => None,
        };
        match refused {
            Some(why) => assert!(r.error.as_deref().is_some_and(|e| e.contains(why)) && (r.inserted, r.skipped) == (0, 0), "{name}: {r:?}"),
            None => assert_eq!((r.inserted, r.skipped, r.error.as_deref()), (0, documents(name), None), "{name}"),
        }
    }
    for name in &names {
        assert_eq!(count(&to, name), count(&from, name), "{name} after pasting again");
    }
    if series {
        let ts = vec![CopyItem { src: Namespace::new(&from, "ts"), dst: Namespace::new(&to, "ts"), if_exists: IfExists::Replace }];
        let Event::Copied(r) = send(Command::CopyCollections { to: id, items: ts, options: CopyOptions::default() }) else { panic!() };
        assert_eq!((r.items[0].inserted, r.items[0].error.as_deref()), (30, None));
        assert_eq!(count(&to, "ts"), 30);
    }

    // A second profile for the same server: another connection, the same
    // collections. Pasting one into itself is refused before anything is
    // dropped (a time series included, whose uuid is its buckets').
    let p2 = profile(&s);
    let id2 = p2.conn_id();
    let req = ConnectRequest { profile: p2, password: s.password.clone(), ssh_secret: None };
    assert!(matches!(wait(&core, core.send(None, Command::Connect(Box::new(req)))), Event::Connected(_)));
    let across = |items: Vec<CopyItem>| {
        let ev = wait(&core, core.send(Some(id2), Command::CopyCollections { to: id, items, options: CopyOptions::default() }));
        let Event::Copied(r) = ev else { panic!("{ev:?}") };
        r
    };
    for name in ["orders", "ts", "capped"].into_iter().filter(|n| series || *n != "ts") {
        let r = across(vec![CopyItem { src: Namespace::new(&from, name), dst: Namespace::new(&from, name), if_exists: IfExists::Replace }]);
        assert_eq!(r.items[0].error.as_deref(), Some("a collection can't be pasted into itself"), "{name}: {r:?}");
        assert_eq!(count(&from, name), documents(name), "{name}");
    }
    // Through those two profiles, orders → orders_old (Replace) would drop
    // orders_old before orders_old → orders_old_2 copies it: refused by uuid.
    insert("orders_old", (0..5).map(|i| doc! { "_id": i, "old": true }).collect());
    let r = across(vec![
        CopyItem { src: Namespace::new(&from, "orders"), dst: Namespace::new(&from, "orders_old"), if_exists: IfExists::Replace },
        CopyItem { src: Namespace::new(&from, "orders_old"), dst: Namespace::new(&from, "orders_old_2"), if_exists: skip },
    ]);
    assert!(r.items[0].error.as_deref().is_some_and(|e| e.contains("is also being pasted")), "{r:?}");
    assert_eq!((r.items[1].inserted, r.items[1].error.as_deref()), (5, None), "{r:?}");
    let Event::Count(old) = send(Command::Count { ns: Namespace::new(&from, "orders_old"), filter: doc! { "old": true } }) else { panic!() };
    assert_eq!((old, count(&from, "orders_old"), count(&from, "orders_old_2")), (5, 5, 5));
    // A view pasted over what it reads, through the other profile: refused
    // before orders is dropped.
    let r = across(vec![CopyItem { src: Namespace::new(&from, "v"), dst: Namespace::new(&from, "orders"), if_exists: IfExists::Replace }]);
    assert!(r.items[0].error.as_deref().is_some_and(|e| e.contains("view v reads from")), "{r:?}");
    assert_eq!(count(&from, "orders"), 2500);

    // Overwrite replaces by _id; Stop names the first duplicate.
    let orders = || vec![CopyItem { src: Namespace::new(&from, "orders"), dst: Namespace::new(&to, "orders"), if_exists: IfExists::Append(OnDuplicateId::Overwrite) }];
    let ev = send(Command::Update { ns: Namespace::new(&to, "orders"), filter: doc! {}, update: rongo_core::UpdateBody::Doc(doc! { "$set": { "changed": true } }), many: true, upsert: false });
    assert!(matches!(ev, Event::Written(ref w) if w.modified == 2500), "{ev:?}");
    let Event::Copied(r) = send(Command::CopyCollections { to: id, items: orders(), options: CopyOptions::default() }) else { panic!() };
    assert_eq!((r.items[0].inserted, r.items[0].overwritten, r.items[0].error.as_deref()), (0, 2500, None));
    let Event::Count(changed) = send(Command::Count { ns: Namespace::new(&to, "orders"), filter: doc! { "changed": true } }) else { panic!() };
    assert_eq!(changed, 0);
    let mut stop = orders();
    stop[0].if_exists = IfExists::Append(OnDuplicateId::Stop);
    let Event::Copied(r) = send(Command::CopyCollections { to: id, items: stop, options: CopyOptions::default() }) else { panic!() };
    assert_eq!((r.items[0].inserted, r.items[0].error.as_deref()), (0, Some("duplicate _id 0")));

    // Documents written before the source got its validator are refused by
    // the target that got it; the rest of the unordered batch goes in, and
    // the item says exactly how many along with the error.
    insert("legacy", (0..2500).map(|i| if i % 500 == 7 { doc! { "_id": i } } else { doc! { "_id": i, "a": i } }).collect());
    run(&from, doc! { "collMod": "legacy", "validator": validator.clone() });
    let legacy = vec![CopyItem { src: Namespace::new(&from, "legacy"), dst: Namespace::new(&to, "legacy"), if_exists: skip }];
    let Event::Copied(r) = send(Command::CopyCollections { to: id, items: legacy, options: CopyOptions::default() }) else { panic!() };
    let error = r.items[0].error.as_deref().unwrap_or_default();
    assert!(error.contains("121") && error.contains("paste without collection options"), "{r:?}");
    assert_eq!((r.items[0].inserted, count(&to, "legacy")), (998, 998));

    send(Command::DropDatabase { db: from });
    send(Command::DropDatabase { db: to });
}

/// Collection History: a filtered delete and a pipeline update on mixed `_id`
/// types save exactly the documents they change, and the update's hashes are
/// of the documents as it left them.
#[test]
fn recorded_writes_against_a_real_server() {
    use std::collections::{BTreeSet, HashMap};

    use rongo_core::bson::RawDocumentBuf;
    use rongo_core::history::{HistoryKey, Kind, Store, doc_hash};
    use rongo_core::{Record, UpdateBody};
    let Some(s) = server() else { return };
    let (_rt, core) = start();
    let p = profile(&s);
    let (conn, profile_id) = (Some(p.conn_id()), p.id);
    let req = ConnectRequest { profile: p, password: s.password.clone(), ssh_secret: None };
    assert!(matches!(wait(&core, core.send(None, Command::Connect(Box::new(req)))), Event::Connected(_)));
    let send = |cmd| wait(&core, core.send(conn, cmd));
    let db = format!("rongo_hist_{}", std::process::id());
    let ns = Namespace::new(&db, "mixed");
    let root = std::env::temp_dir().join(format!("rongo-history-real-{}", std::process::id()));
    let key = HistoryKey::new([7; 32]);
    let record = Record { root: root.clone(), key: key.clone(), profile: profile_id, conn_name: "test".into(), keep_days: 30, max_bytes: 1 << 30 };
    let recorded = |cmd: Command| send(Command::Recorded { record: record.clone(), cmd: Box::new(cmd) });
    let store = Store::new(root.clone(), key);
    let id_of = |d: &RawDocumentBuf| format!("{:?}", Bson::try_from(d.get("_id").unwrap().unwrap()).unwrap());
    let count = |filter: Document| match send(Command::Count { ns: ns.clone(), filter }) {
        Event::Count(n) => n,
        other => panic!("{other:?}"),
    };

    // 2,500 documents whose `_id`s are ObjectIds, strings, ints, longs and embedded documents.
    let id = |i: i32| match i % 5 {
        0 => Bson::ObjectId(ObjectId::new()),
        1 => Bson::String(format!("s{i}")),
        2 => Bson::Int32(i),
        3 => Bson::Int64(i64::from(i) * 1_000_000_007),
        _ => Bson::Document(doc! { "k": i, "t": "e" }),
    };
    let docs: Vec<Document> = (0..2500).map(|i| doc! { "_id": id(i), "i": i, "g": i % 2, "n": i }).collect();
    let ev = send(Command::InsertMany { ns: ns.clone(), docs: docs.clone() });
    assert!(matches!(ev, Event::Written(ref w) if w.inserted == 2500), "{ev:?}");
    // The update below moves every document ahead in this index, where the cursor can meet it again.
    assert!(matches!(send(Command::CreateIndex { ns: ns.clone(), keys: doc! { "n": 1 }, options: None }), Event::Document(_)));

    // `countDocuments` refuses `$where` (find allows it): the estimate is
    // unknown and the recorded delete still runs, as the plain one would.
    let ev = recorded(Command::DeleteWhere { ns: ns.clone(), filter: doc! { "$where": "this.i === 0" }, many: true });
    let Event::Written(w) = ev else { panic!("{ev:?}") };
    assert_eq!((w.deleted, w.cancelled, w.warning.as_deref()), (1, false, None));
    let saved: Vec<_> = store.read(&w.recorded.expect("recorded")).unwrap().map(Result::unwrap).collect();
    assert_eq!(saved.iter().map(|s| id_of(&s.before)).collect::<Vec<_>>(), [format!("{:?}", docs[0].get("_id").unwrap())]);

    // A filtered delete saves and deletes exactly the 1,250 odd documents.
    let ev = recorded(Command::DeleteWhere { ns: ns.clone(), filter: doc! { "g": 1 }, many: true });
    let Event::Written(w) = ev else { panic!("{ev:?}") };
    assert_eq!((w.deleted, w.cancelled, w.warning.as_deref()), (1250, false, None));
    let saved: Vec<_> = store.read(&w.recorded.expect("recorded")).unwrap().map(Result::unwrap).collect();
    let odd: BTreeSet<String> = docs.iter().filter(|d| d.get_i32("g").ok() == Some(1)).map(|d| format!("{:?}", d.get("_id").unwrap())).collect();
    assert_eq!(saved.iter().map(|s| id_of(&s.before)).collect::<BTreeSet<_>>(), odd);
    assert_eq!((count(doc! { "g": 1 }), count(doc! {})), (0, 1249));

    // A pipeline update changes each remaining document exactly once, though it
    // moves each ahead of the cursor in the index on `n`.
    let pipeline = vec![doc! { "$set": { "n": { "$add": ["$n", 100_000] }, "seen": true } }];
    let ev = recorded(Command::Update { ns: ns.clone(), filter: doc! { "n": { "$gte": 0 } }, update: UpdateBody::Pipeline(pipeline), many: true, upsert: false });
    let Event::Written(w) = ev else { panic!("{ev:?}") };
    assert_eq!((w.matched, w.modified, w.cancelled, w.warning.as_deref()), (1249, 1249, false, None));
    assert_eq!(count(doc! { "$expr": { "$eq": ["$n", { "$add": ["$i", 100_000] }] } }), 1249);
    let update = w.recorded.expect("recorded");
    let saved: Vec<_> = store.read(&update).unwrap().map(Result::unwrap).collect();
    assert_eq!(saved.len(), 1249);
    let Event::Page { docs: now, .. } = send(Command::Find(FindSpec::all(ns.clone(), 5000))) else { panic!() };
    let now: HashMap<String, RawDocumentBuf> = now.into_iter().map(|d| (id_of(&d), d)).collect();
    for s in &saved {
        assert!(s.before.get("seen").unwrap().is_none(), "saved before the update");
        assert_eq!(s.after_hash, Some(doc_hash(&now[&id_of(&s.before)])), "the hash of the document as the update left it");
    }
    let list = store.list(profile_id, &ns).unwrap();
    assert_eq!(list.iter().map(|e| (e.kind, e.count, e.complete)).collect::<Vec<_>>(), [(Kind::Update, 1249, true), (Kind::Delete, 1250, true), (Kind::Delete, 1, true)]);

    send(Command::DropDatabase { db });
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn restore_against_a_real_server() {
    use std::collections::HashMap;

    use rongo_core::bson::RawDocumentBuf;
    use rongo_core::history::{Kind, HistoryKey, Store};
    use rongo_core::{OnConflict, Record};
    let Some(s) = server() else { return };
    let (_rt, core) = start();
    let p = profile(&s);
    let (conn, profile_id) = (Some(p.conn_id()), p.id);
    let req = ConnectRequest { profile: p, password: s.password.clone(), ssh_secret: None };
    assert!(matches!(wait(&core, core.send(None, Command::Connect(Box::new(req)))), Event::Connected(_)));
    let send = |cmd| wait(&core, core.send(conn, cmd));
    let db = format!("rongo_restore_{}", std::process::id());
    let ns = Namespace::new(&db, "mixed");
    let root = std::env::temp_dir().join(format!("rongo-restore-real-{}", std::process::id()));
    let key = HistoryKey::new([7; 32]);
    let record = Record { root: root.clone(), key: key.clone(), profile: profile_id, conn_name: "test".into(), keep_days: 30, max_bytes: 1 << 30 };
    let recorded = |cmd: Command| match send(Command::Recorded { record: record.clone(), cmd: Box::new(cmd) }) {
        Event::Written(w) => w,
        other => panic!("{other:?}"),
    };
    let restore = |entry, on_conflict| match send(Command::HistoryRestore { record: record.clone(), entry, on_conflict }) {
        Event::Restored { restored, skipped, overwritten, inserted_new, recorded, cancelled, warning, unconfirmed, unconfirmed_docs } => {
            assert_eq!((cancelled, warning, unconfirmed, unconfirmed_docs), (false, None, false, 0));
            (restored, skipped, overwritten, inserted_new, recorded)
        }
        other => panic!("{other:?}"),
    };
    let conflicts = |entry| match send(Command::HistoryConflicts { record: record.clone(), entry }) {
        Event::HistoryConflicts { total, conflicts, sample } => (total, conflicts, sample.len()),
        other => panic!("{other:?}"),
    };
    let count = |filter: Document| match send(Command::Count { ns: ns.clone(), filter }) {
        Event::Count(n) => n,
        other => panic!("{other:?}"),
    };
    let store = Store::new(root.clone(), key);
    let id_of = |d: &RawDocumentBuf| format!("{:?}", Bson::try_from(d.get("_id").unwrap().unwrap()).unwrap());
    let snapshot = || -> HashMap<String, RawDocumentBuf> {
        let Event::Page { docs, .. } = send(Command::Find(FindSpec::all(ns.clone(), 5000))) else { panic!() };
        docs.into_iter().map(|d| (id_of(&d), d)).collect()
    };

    // 2,500 documents whose `_id`s are ObjectIds, strings, ints, longs and embedded documents.
    let id = |i: i32| match i % 5 {
        0 => Bson::ObjectId(ObjectId::new()),
        1 => Bson::String(format!("s{i}")),
        2 => Bson::Int32(i),
        3 => Bson::Int64(i64::from(i) * 1_000_000_007),
        _ => Bson::Document(doc! { "k": i, "t": "e" }),
    };
    let docs: Vec<Document> = (0..2500).map(|i| doc! { "_id": id(i), "i": i, "g": i % 2, "n": i }).collect();
    let ev = send(Command::InsertMany { ns: ns.clone(), docs: docs.clone() });
    assert!(matches!(ev, Event::Written(ref w) if w.inserted == 2500), "{ev:?}");
    let original = snapshot();

    // A bulk delete, restored: every document is back byte for byte.
    let w = recorded(Command::DeleteWhere { ns: ns.clone(), filter: doc! { "g": 1 }, many: true });
    assert_eq!(w.deleted, 1250);
    let deleted = w.recorded.expect("recorded");
    assert_eq!(conflicts(deleted.clone()), (1250, 0, 0));
    assert_eq!(restore(deleted.clone(), OnConflict::Skip), (1250, 0, 0, 0, None));
    assert_eq!(snapshot(), original);

    // Overwrite replaces through a guard on the exact version it saved
    // (`$expr` on `$$ROOT`): it must match on the server for every `_id` type.
    // Deleted again, five come back changed: a string, a long, an ObjectId,
    // an int and an embedded-document `_id`.
    let w = recorded(Command::DeleteWhere { ns: ns.clone(), filter: doc! { "g": 1 }, many: true });
    let deleted = w.recorded.expect("recorded");
    let back: Vec<Document> = [1, 3, 5, 7, 9].iter().map(|&i| {
        let mut d = docs[i].clone();
        d.insert("changed", true);
        d
    }).collect();
    assert!(back.iter().all(|d| d.get_i32("g").ok() == Some(1)), "all among the deleted");
    let ev = send(Command::InsertMany { ns: ns.clone(), docs: back });
    assert!(matches!(ev, Event::Written(ref w) if w.inserted == 5), "{ev:?}");
    assert_eq!(conflicts(deleted.clone()), (1250, 5, 5));
    // The guard's form (see `restore::guard`) matches a document only as it
    // is, on this server: the version as it is now matches, another does not.
    for d in docs.iter().filter(|d| [1, 3, 5, 7, 9].contains(&d.get_i32("i").unwrap())) {
        let id = d.get("_id").unwrap().clone();
        let guard = |v: &Document| doc! { "_id": id.clone(), "$expr": { "$eq": [{ "$cmp": ["$$ROOT", { "$literal": v.clone() }] }, 0] } };
        let mut now = d.clone();
        now.insert("changed", true);
        assert_eq!(count(guard(&now)), 1, "{id:?}");
        assert_eq!(count(guard(d)), 0, "{id:?}: the saved version is not what is there");
    }
    let (restored, skipped, overwritten, inserted_new, new) = restore(deleted.clone(), OnConflict::Overwrite);
    assert_eq!((restored, skipped, overwritten, inserted_new), (1245, 0, 5, 0), "the guarded replace matched every _id type");
    assert_eq!(store.read(&new.expect("recorded")).unwrap().count(), 5);
    assert_eq!(snapshot(), original);

    // A bulk update, restored, after one document was changed again: that one
    // is a conflict and Skip leaves it; Overwrite then puts it back too.
    let w = recorded(Command::UpdateMany { ns: ns.clone(), filter: doc! {}, update: doc! { "$set": { "x": "updated" }, "$inc": { "n": 1 } } });
    assert_eq!(w.modified, 2500);
    let updated = w.recorded.expect("recorded");
    let again = docs[7].get("_id").unwrap().clone();
    let ev = send(Command::Update { ns: ns.clone(), filter: doc! { "_id": again.clone() }, update: rongo_core::UpdateBody::Doc(doc! { "$set": { "x": "again" } }), many: false, upsert: false });
    assert!(matches!(ev, Event::Written(ref w) if w.modified == 1), "{ev:?}");
    assert_eq!(conflicts(updated.clone()), (2500, 1, 1));
    let (restored, skipped, overwritten, inserted_new, new) = restore(updated.clone(), OnConflict::Skip);
    assert_eq!((restored, skipped, overwritten, inserted_new), (2499, 1, 0, 0));
    let new = new.expect("what the restore replaced is recorded");
    let now = snapshot();
    for (k, d) in &original {
        if *k == format!("{again:?}") {
            assert_eq!(now[k].get_str("x").unwrap(), "again");
        } else {
            assert_eq!(&now[k], d, "restored byte for byte");
        }
    }
    assert_eq!(restore(updated.clone(), OnConflict::Overwrite).0, 2499, "already as saved");
    assert_eq!(snapshot(), original);
    let list = store.list(profile_id, &ns).unwrap();
    assert!(list.iter().find(|e| e.id == updated).unwrap().restored);
    let h = list.iter().find(|e| e.id == new).unwrap();
    assert_eq!((h.kind, h.count, h.complete), (Kind::Update, 2499, true));

    send(Command::DropDatabase { db });
    let _ = std::fs::remove_dir_all(&root);
}

/// Paste with Append + Overwrite keeps the target documents it replaces:
/// the guarded replace must match on the server for every `_id` type, and
/// restoring the entry brings the target back byte for byte.
#[test]
fn paste_overwrite_records_against_a_real_server() {
    use std::collections::HashMap;

    use rongo_core::bson::RawDocumentBuf;
    use rongo_core::history::{HistoryKey, Kind, Store};
    use rongo_core::{CopyItem, CopyOptions, IfExists, OnConflict, OnDuplicateId, Record};
    let Some(s) = server() else { return };
    let (_rt, core) = start();
    let p = profile(&s);
    let (conn, profile_id) = (Some(p.conn_id()), p.id);
    let req = ConnectRequest { profile: p, password: s.password.clone(), ssh_secret: None };
    assert!(matches!(wait(&core, core.send(None, Command::Connect(Box::new(req)))), Event::Connected(_)));
    let send = |cmd| wait(&core, core.send(conn, cmd));
    let (from, to) = (format!("rongo_paste_from_{}", std::process::id()), format!("rongo_paste_to_{}", std::process::id()));
    let (src, dst) = (Namespace::new(&from, "mixed"), Namespace::new(&to, "mixed"));
    let root = std::env::temp_dir().join(format!("rongo-paste-real-{}", std::process::id()));
    let key = HistoryKey::new([7; 32]);
    let record = Record { root: root.clone(), key: key.clone(), profile: profile_id, conn_name: "test".into(), keep_days: 30, max_bytes: 1 << 30 };
    let id_of = |d: &RawDocumentBuf| format!("{:?}", Bson::try_from(d.get("_id").unwrap().unwrap()).unwrap());
    let snapshot = |ns: &Namespace| -> HashMap<String, RawDocumentBuf> {
        let Event::Page { docs, .. } = send(Command::Find(FindSpec::all(ns.clone(), 5000))) else { panic!() };
        docs.into_iter().map(|d| (id_of(&d), d)).collect()
    };

    // 2,500 `_id`s of every kind, the same in both collections, with other content.
    let ids: Vec<Bson> = (0..2500)
        .map(|i| match i % 5 {
            0 => Bson::ObjectId(ObjectId::new()),
            1 => Bson::String(format!("s{i}")),
            2 => Bson::Int32(i),
            3 => Bson::Int64(i64::from(i) * 1_000_000_007),
            _ => Bson::Document(doc! { "k": i, "t": "e" }),
        })
        .collect();
    let insert = |ns: &Namespace, label: &str| {
        let docs: Vec<Document> = ids.iter().enumerate().map(|(i, id)| doc! { "_id": id.clone(), "i": i as i32, "from": label }).collect();
        let ev = send(Command::InsertMany { ns: ns.clone(), docs });
        assert!(matches!(ev, Event::Written(ref w) if w.inserted == 2500), "{ev:?}");
    };
    insert(&src, "source");
    insert(&dst, "target");
    let (source_before, target_before) = (snapshot(&src), snapshot(&dst));

    let items = vec![CopyItem { src: src.clone(), dst: dst.clone(), if_exists: IfExists::Append(OnDuplicateId::Overwrite) }];
    let options = CopyOptions { history: Some(record.clone()), ..Default::default() };
    let Event::Copied(report) = send(Command::CopyCollections { to: conn.unwrap(), items, options }) else { panic!() };
    let item = &report.items[0];
    assert_eq!((item.inserted, item.overwritten, item.error.as_deref()), (0, 2500, None), "{report:?}");
    assert_eq!(snapshot(&dst), source_before);

    let store = Store::new(root.clone(), key);
    let entry = item.recorded.clone().expect("recorded");
    let list = store.list(profile_id, &dst).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!((list[0].id.clone(), list[0].kind, list[0].count, list[0].complete), (entry.clone(), Kind::Update, 2500, true));
    assert!(store.list(profile_id, &src).unwrap().is_empty());

    let ev = send(Command::HistoryRestore { record: record.clone(), entry, on_conflict: OnConflict::Skip });
    let Event::Restored { restored, skipped, overwritten, cancelled, warning, .. } = ev else { panic!("{ev:?}") };
    assert_eq!((restored, skipped, overwritten, cancelled, warning), (2500, 0, 0, false, None));
    assert_eq!(snapshot(&dst), target_before, "the target is back byte for byte");

    send(Command::DropDatabase { db: from });
    send(Command::DropDatabase { db: to });
    let _ = std::fs::remove_dir_all(&root);
}
