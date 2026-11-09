//! Shell scripts against a real server; see crates/core/tests/real_mongo.rs for
//! the environment variables. Without them, the test returns early.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rongo_core::bson::Bson;
use rongo_core::conn::Conn;
use rongo_core::profile::{Auth, ConnectionProfile, Target};
use rongo_core::{ConnectRequest, Session};
use rongo_shell::{ResultItem, ShellEvent, ShellHandle};

#[test]
fn script_against_a_real_server() {
    let Ok(host) = std::env::var("RONGO_TEST_HOST") else { return };
    let user = std::env::var("RONGO_TEST_USER").ok();
    let password = std::env::var("RONGO_TEST_PASSWORD").ok();
    let mut p = ConnectionProfile::new("t");
    p.target = Target::Manual { hosts: vec![host], replica_set: None, direct: true };
    if let Some(u) = user {
        p.auth = Auth::Password { user: u, auth_db: "admin".into(), mechanism: None };
    }
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let conn = rt.block_on(Conn::open(&ConnectRequest { profile: p, password, ssh_secret: None })).unwrap();
    let mut shell = ShellHandle::spawn(Session { conn, rt: rt.handle().clone() }, Arc::new(|| {}));

    let db = format!("rongo_shell_test_{}", std::process::id());
    let script = format!(
        r#"use {db}
db.orders.insertMany([{{ k: "a", v: 1 }}, {{ k: "b", v: 2 }}, {{ k: "a", v: NumberLong(40) }}]);
db.orders.createIndex({{ k: 1 }}, {{ name: "k_1" }});
db.orders.updateMany({{ k: "a" }}, {{ $set: {{ seen: true }} }});
print(db.orders.countDocuments({{ seen: true }}), db.orders.getIndexes().length, db.getCollectionNames().join(","));
print(db.version().split(".").length >= 2);
print(db.orders.createIndex({{ v: 1, k: -1 }}), Object.keys(db.orders.findOne({{ k: "b" }})).join(","));
db.orders.find({{ k: "a" }}).sort({{ v: -1 }})
db.orders.aggregate([{{ $group: {{ _id: "$k", total: {{ $sum: "$v" }} }} }}, {{ $sort: {{ _id: 1 }} }}])
db.dropDatabase()
"#
    );
    let run = shell.run(script, "test", 0);
    let (mut prints, mut results, mut done) = (Vec::new(), Vec::new(), None);
    let deadline = Instant::now() + Duration::from_secs(30);
    while done.is_none() {
        match shell.try_recv() {
            Some(ShellEvent::Print { text, .. }) => prints.push(text),
            Some(ShellEvent::Result { item, .. }) => results.push(item),
            Some(ShellEvent::Done { run: r, error, db, .. }) if r == run => done = Some((error, db)),
            Some(_) => {}
            None => {
                assert!(Instant::now() < deadline, "script did not finish; prints so far: {prints:?}");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
    let (error, final_db) = done.unwrap();
    assert_eq!(error, None, "{prints:?}");
    assert_eq!(final_db, db);
    assert_eq!(prints[0], format!("switched to db {db}"));
    assert_eq!(prints[1], "2 2 orders");
    assert_eq!(prints[2], "true");
    // Key order reaches the server: a compound index keeps its field order.
    assert_eq!(prints[3], "v_1_k_-1 _id,k,v");
    // The final value (dropDatabase's reply) is a document too, so it shows up
    // as a "Result" tab after the find and aggregate tabs.
    assert!(matches!(&results[0], ResultItem::Find { line: 8, spec, .. } if spec.ns.coll == "orders"), "{results:?}");
    let ResultItem::Docs { line: 9, docs, .. } = &results[1] else { panic!("{results:?}") };
    assert_eq!(docs[0].get("total"), Some(&Bson::Int64(41)));
    assert_eq!(docs[1].get("total"), Some(&Bson::Int32(2)));
}
