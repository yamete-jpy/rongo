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

fn wait(core: &CoreHandle, id: RequestId) -> Event {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(r) = core.try_recv() {
            if r.id == id {
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
    let ev = send(Command::DeleteMany { ns: renamed.clone(), filter: doc! { "i": { "$gte": 8 } } });
    assert!(matches!(ev, Event::Written(ref w) if w.deleted == 2), "{ev:?}");
    let ev = send(Command::ClearCollection { ns: renamed.clone() });
    assert!(matches!(ev, Event::Written(ref w) if w.deleted == 8), "{ev:?}");

    let path = std::env::temp_dir().join(format!("rongo-export-{}.jsonl", std::process::id()));
    let spec = ExportSpec { ns: ns.clone(), filter: doc! { "even": false }, sort: Some(doc! { "i": 1 }), projection: None, format: ExportFormat::JsonLines, path: path.clone(), fields: None };
    assert!(matches!(send(Command::Export(Box::new(spec))), Event::Exported { count: 5, .. }));
    let ev = send(Command::Import { ns: renamed.clone(), path: path.clone(), format: ImportFormat::Auto });
    assert!(matches!(ev, Event::Written(ref w) if w.inserted == 5), "{ev:?}");
    std::fs::remove_file(path).ok();

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
