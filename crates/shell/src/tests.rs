use std::sync::Arc;
use std::time::{Duration, Instant};

use rongo_core::Session;
use rongo_core::bson::{Bson, doc};
use rongo_core::conn::Conn;
use rongo_core::mock::MockStore;

use super::*;

struct Fixture {
    _rt: tokio::runtime::Runtime,
    shell: ShellHandle,
}

impl Fixture {
    fn new() -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let session = Session { conn: Conn::mock(MockStore::demo(), false), rt: rt.handle().clone() };
        let shell = ShellHandle::spawn(session, Arc::new(|| {}));
        Fixture { _rt: rt, shell }
    }

    /// Run a script and collect everything it produced.
    fn run(&mut self, code: &str) -> (Vec<String>, Vec<ResultItem>, String, Option<ShellError>) {
        let run = self.shell.run(code, "demo-games", 0);
        let (mut prints, mut results) = (Vec::new(), Vec::new());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match self.shell.try_recv() {
                Some(ShellEvent::Print { run: r, text }) if r == run => prints.push(text),
                Some(ShellEvent::Result { run: r, item }) if r == run => results.push(item),
                Some(ShellEvent::Done { run: r, db, error, .. }) if r == run => return (prints, results, db, error),
                Some(_) => {}
                None => {
                    assert!(Instant::now() < deadline, "script did not finish: {code}");
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        }
    }
}

#[test]
fn unread_find_becomes_a_result_tab_with_its_line() {
    let mut f = Fixture::new();
    let (_, results, _, err) = f.run("// comment\ndb.getCollection(\"advertisers\").find({ position: { $gte: 2 } }).sort({ position: -1 }).limit(2)");
    assert_eq!(err, None);
    let [ResultItem::Find { line, spec, max }] = results.as_slice() else { panic!("{results:?}") };
    assert_eq!(*line, 2);
    assert_eq!(spec.ns.coll, "advertisers");
    assert_eq!(spec.filter, doc! { "position": { "$gte": 2 } });
    assert_eq!(spec.sort, Some(doc! { "position": -1 }));
    assert_eq!(*max, Some(2));
}

#[test]
fn scripts_read_write_and_keep_types() {
    let mut f = Fixture::new();
    let script = r#"
        const id = ObjectId();
        db.scratch.insertOne({ _id: id, n: 1, big: NumberLong("9007199254740993"), at: ISODate("2026-01-02T03:04:05Z"), d: NumberDecimal("1.10") });
        db.scratch.updateOne({ _id: id }, { $inc: { n: 41 } });
        const got = db.scratch.findOne({ _id: id });
        print(got.n, typeof got.at.getTime(), got.big.toString(), got.d.toString());
        for (const doc of db.advertisers.find().sort({ position: 1 }).limit(2)) print(doc.name);
        db.scratch.find({ _id: id }).toArray()
    "#;
    let (prints, results, _, err) = f.run(script);
    assert_eq!(err, None, "{prints:?}");
    assert_eq!(prints, ["42 number 9007199254740993 1.10", "Position 1", "Position 2"]);
    let [ResultItem::Docs { docs, title, .. }] = results.as_slice() else { panic!("{results:?}") };
    assert_eq!(title, "Result");
    let d = &docs[0];
    assert_eq!(d.get("n"), Some(&Bson::Int32(42)));
    assert_eq!(d.get("big"), Some(&Bson::Int64(9_007_199_254_740_993)));
    assert!(matches!(d.get("at"), Some(Bson::DateTime(_))));
    assert!(matches!(d.get("d"), Some(Bson::Decimal128(_))));
}

#[test]
fn use_switches_db_and_errors_report_lines() {
    let mut f = Fixture::new();
    let (prints, _, db, err) = f.run("use analytics\nprint(db.events.countDocuments({ type: 'login' }) > 0)");
    assert_eq!(err, None);
    assert_eq!(db, "analytics");
    assert_eq!(prints, ["switched to db analytics", "true"]);

    let (_, _, db, err) = f.run("use analytics\nconst a = 1;\nnosuch.call()");
    assert_eq!(db, "analytics");
    let err = err.unwrap();
    assert_eq!(err.line, Some(3), "{err:?}");
    assert!(err.message.contains("nosuch"), "{err:?}");

    let (_, _, _, err) = f.run("db.c.insertOne({ _id: 1 });\ndb.c.insertOne({ _id: 1 })");
    let err = err.unwrap();
    assert!(err.message.contains("E11000"), "{err:?}");
    assert_eq!(err.line, Some(2));
}

#[test]
fn rerunning_const_declarations_is_fine_and_aggregate_fetches() {
    let mut f = Fixture::new();
    let script = "const p = [{ $group: { _id: '$status', n: { $sum: 1 } } }, { $sort: { _id: 1 } }];\ndb.bet_logs.aggregate(p)";
    for _ in 0..2 {
        let (_, results, _, err) = f.run(script);
        assert_eq!(err, None);
        let [ResultItem::Docs { line, title, docs, truncated }] = results.as_slice() else { panic!("{results:?}") };
        assert_eq!((*line, title.as_str(), docs.len(), *truncated), (2, "Aggregate", 3, false));
    }
}

#[test]
fn interrupt_stops_an_infinite_loop() {
    let mut f = Fixture::new();
    let run = f.shell.run("while (true) {}", "demo-games", 0);
    std::thread::sleep(Duration::from_millis(100));
    f.shell.interrupt();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(ShellEvent::Done { run: r, error, .. }) = f.shell.try_recv()
            && r == run
        {
            assert_eq!(error.unwrap().message, "Interrupted");
            break;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    let (prints, _, _, err) = f.run("print('still alive')");
    assert_eq!((prints, err), (vec!["still alive".to_string()], None));
}

#[test]
fn parse_document_reads_shell_syntax() {
    let d = parse_document(r#"{ _id: ObjectId("64f6ef45435c6900a1b2c3d4"), at: ISODate("2026-01-01"), n: 5, f: Double(5), l: NumberLong(7), u: UUID("0123456789abcdef0123456789abcdef"), re: /ab+c/i, nested: { arr: [1, "x", null] } }"#).unwrap();
    assert!(matches!(d.get("_id"), Some(Bson::ObjectId(_))));
    assert!(matches!(d.get("at"), Some(Bson::DateTime(_))));
    assert_eq!(d.get("n"), Some(&Bson::Int32(5)));
    assert_eq!(d.get("f"), Some(&Bson::Double(5.0)));
    assert_eq!(d.get("l"), Some(&Bson::Int64(7)));
    assert!(matches!(d.get("u"), Some(Bson::Binary(b)) if b.bytes.len() == 16));
    assert!(matches!(d.get("re"), Some(Bson::RegularExpression(r)) if r.pattern.as_str() == "ab+c" && r.options.as_str() == "i"));
    assert_eq!(parse_document("").unwrap(), doc! {});
    assert_eq!(parse_document("  { status: 'x' };").unwrap(), doc! { "status": "x" });
    assert!(parse_document("[1]").is_err());
    let e = parse_document("{ a: }").unwrap_err();
    assert!(e.message.contains("SyntaxError"), "{e:?}");
    assert!(parse_value("(() => { while (true) {} })()").is_err());
    assert!(parse_document("db.users.find()").is_err());
}

#[test]
fn editable_format_round_trips_through_parse() {
    let original = doc! {
        "_id": rongo_core::bson::oid::ObjectId::new(),
        "i": 1,
        "l": 2_i64,
        "f": 3.0,
        "g": 3.5,
        "s": "quote \" and \\ backslash",
        "at": rongo_core::bson::DateTime::from_millis(1_760_000_000_123),
        "arr": [1, { "x": Bson::Null }],
        "dec": "12.50".parse::<rongo_core::bson::Decimal128>().unwrap(),
        "t": true,
    };
    let text = rongo_core::format::doc_to_shell(&original, rongo_core::format::Style::Editable);
    assert_eq!(parse_document(&text).unwrap(), original, "{text}");
}

#[test]
fn aggregate_runs_in_script_order() {
    let mut f = Fixture::new();
    let script = "db.tmp.insertMany([{ a: 1 }, { a: 2 }]);\ndb.tmp.aggregate([{ $match: {} }])\ndb.tmp.deleteMany({});";
    let (_, results, _, err) = f.run(script);
    assert_eq!(err, None);
    // results[1] is the script's final value, deleteMany's reply.
    let ResultItem::Docs { docs, line, .. } = &results[0] else { panic!("{results:?}") };
    assert_eq!((docs.len(), *line), (2, 2));
    assert_eq!(results.len(), 2);
}

#[test]
fn pasted_text_in_every_common_shape() {
    let one = parse_documents_text(r#"{ "_id" : ObjectId("64f6ef45435c6900a1b2c3d4"), "name" : "Position 1" }"#).unwrap();
    assert!(matches!(one[0].get("_id"), Some(Bson::ObjectId(_))));
    // Studio 3T / mongosh style: documents one after another.
    let many = parse_documents_text("{ \"a\" : 1, \"s\": \"x } {\" }\n{ \"a\" : NumberLong(2) }\n").unwrap();
    assert_eq!(many.len(), 2);
    assert_eq!(many[0].get_str("s").unwrap(), "x } {");
    assert_eq!(many[1].get("a"), Some(&Bson::Int64(2)));
    // mongoexport JSON lines with canonical Extended JSON.
    let lines = parse_documents_text("{\"_id\":{\"$oid\":\"64f6ef45435c6900a1b2c3d4\"},\"n\":{\"$numberLong\":\"5\"}}\n{\"n\":1}").unwrap();
    assert!(matches!(lines[0].get("_id"), Some(Bson::ObjectId(_))));
    assert_eq!(lines[0].get("n"), Some(&Bson::Int64(5)));
    assert_eq!(parse_documents_text("[{ a: 1 }, { a: 2 }]").unwrap().len(), 2);
    assert!(parse_documents_text("hello world").is_err());
    assert!(parse_documents_text("[1, 2]").is_err());
    assert!(parse_documents_text("   ").is_err());
}

#[test]
fn offset_dates_read_back_as_the_same_instant() {
    let utc = parse_value("ISODate(\"2026-01-02T03:04:05.000Z\")").unwrap();
    let bkk = parse_value("ISODate(\"2026-01-02T10:04:05.000+07:00\")").unwrap();
    assert_eq!(utc, bkk);
    let text = rongo_core::format::iso_date_in(rongo_core::bson::DateTime::from_millis(1_767_323_045_123), rongo_core::format::DisplayTz::Offset(420));
    assert_eq!(parse_value(&format!("ISODate(\"{text}\")")).unwrap(), Bson::DateTime(rongo_core::bson::DateTime::from_millis(1_767_323_045_123)));
}

/// Key order is meaning in MongoDB (sorts, compound indexes, `_id`s): it
/// must survive parsing and the trip through the JS engine.
#[test]
fn key_order_is_kept() {
    let keys = |d: &rongo_core::bson::Document| d.keys().cloned().collect::<Vec<_>>();
    let d = parse_document("{ status: 1, createdAt: -1, Zed: 1, _id: 0 }").unwrap();
    assert_eq!(keys(&d), ["status", "createdAt", "Zed", "_id"]);
    let docs = parse_documents_text(r#"[{ "_id": { "z": 1, "a": 2 }, "name": "x", "age": 3 }]"#).unwrap();
    assert_eq!(keys(&docs[0]), ["_id", "name", "age"]);
    assert_eq!(keys(docs[0].get_document("_id").unwrap()), ["z", "a"]);

    let mut f = Fixture::new();
    let (prints, _, _, err) = f.run("db.ko.insertOne({ _id: 1, zeta: 1, alpha: 2 });\nprint(Object.keys(db.ko.findOne({ _id: 1 })).join(','))");
    assert!(err.is_none(), "{err:?}");
    assert_eq!(prints, ["_id,zeta,alpha"]);
}

/// Like mongosh: `undefined` in a filter is `null` (a typo'd property must
/// not widen a delete to everything), and writes need a filter argument.
#[test]
fn undefined_and_missing_filters_do_not_match_everything() {
    let mut f = Fixture::new();
    let (_, _, _, err) = f.run("db.uf.insertMany([{ _id: 1 }, { _id: 2 }, { _id: 3 }])");
    assert!(err.is_none(), "{err:?}");
    let (prints, _, _, err) = f.run("const user = { userId: 1 };\nprint(db.uf.deleteMany({ _id: user.id }).deletedCount);\nprint(db.uf.updateMany({ _id: undefined }, { $set: { x: 1 } }).matchedCount)");
    assert!(err.is_none(), "{err:?}");
    assert_eq!(prints, ["0", "0"]);
    for call in ["deleteMany()", "deleteOne()", "updateMany(undefined, { $set: { x: 1 } })", "updateOne()", "replaceOne()", "remove()"] {
        let (_, _, _, err) = f.run(&format!("db.uf.{call}"));
        assert!(err.is_some_and(|e| e.message.contains("filter")), "{call} ran without a filter");
    }
    let (prints, _, _, _) = f.run("print(db.uf.countDocuments({}))");
    assert_eq!(prints, ["3"]);
}

/// Like mongosh, ISODate text without an offset is UTC, not local time.
#[test]
fn isodate_without_an_offset_is_utc() {
    let mut f = Fixture::new();
    let (prints, _, _, err) = f.run(
        "print(ISODate('2026-01-02T03:04:05').toISOString());\nprint(ISODate('2026-01-02 03:04:05.5').toISOString());\nprint(ISODate('2026-01-02T03:04').toISOString());\nprint(ISODate('2026-01-02T03:04:05+07:00').toISOString());\nprint(ISODate('2026-01-02').toISOString())",
    );
    assert!(err.is_none(), "{err:?}");
    assert_eq!(prints, ["2026-01-02T03:04:05.000Z", "2026-01-02T03:04:05.500Z", "2026-01-02T03:04:00.000Z", "2026-01-01T20:04:05.000Z", "2026-01-02T00:00:00.000Z"]);
    let d = parse_value("ISODate('2026-01-02T03:04:05')").unwrap();
    assert_eq!(d, Bson::DateTime(rongo_core::bson::DateTime::from_millis(1_767_323_045_000)));
}

/// Objects separated by commas are several documents, not (as JS's comma
/// operator would have it) the last one.
#[test]
fn comma_separated_documents_are_all_kept() {
    let docs = parse_documents_text("{ a: 1 },\n{ a: 2 },\n{ a: { b: 3 } }").unwrap();
    assert_eq!(docs, [doc! { "a": 1 }, doc! { "a": 2 }, doc! { "a": { "b": 3 } }]);
    assert_eq!(parse_documents_text("{ a: 1 }\n{ a: 2 }").unwrap().len(), 2);
    assert_eq!(parse_documents_text("[{ a: 1 }, { a: 2 }]").unwrap().len(), 2);
    assert_eq!(parse_documents_text("{ a: '}, {' }").unwrap(), [doc! { "a": "}, {" }]);
    assert!(parse_document("{ status: 'x' }, { name: 1 }").is_err());
    assert_eq!(parse_document("{ a: { b: 1 } }").unwrap(), doc! { "a": { "b": 1 } });
}

/// Stop can't be caught and carried on from: after it, every database call
/// fails, so a try/catch around each step can't keep writing.
#[test]
fn stop_cannot_be_caught_to_keep_writing() {
    let mut f = Fixture::new();
    let run = f.shell.run("for (let i = 0; i < 20; i++) {\n  try { sleep(100) } catch (e) {}\n  try { db.stopped.insertOne({ i: i }) } catch (e) {}\n}", "demo-games", 0);
    std::thread::sleep(Duration::from_millis(250));
    f.shell.interrupt();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match f.shell.try_recv() {
            Some(ShellEvent::Done { run: r, .. }) if r == run => break,
            _ => {
                assert!(Instant::now() < deadline, "the script did not stop");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
    let (_, _, _, err) = f.run("db.stopped.insertOne({ i: 'after' })");
    assert!(err.is_none(), "the next run works: {err:?}");
    let (prints, _, _, _) = f.run("print(db.stopped.countDocuments({ i: { $type: 'number' } }))");
    let written: u32 = prints[0].parse().unwrap();
    assert!(written <= 3, "{written} writes after about 2 steps; Stop should end them");
}

/// mongosh idioms: cursor.map(...).toArray(), dotted collection names as
/// properties, and documents with values JS can't hold natively.
#[test]
fn map_dotted_collections_and_odd_values() {
    let mut f = Fixture::new();
    let (prints, _, _, err) = f.run(
        "db.fs.files.insertOne({ _id: 1, n: 'a' });\nprint(db.getCollection('fs.files').countDocuments({}));\nprint(db.fs.files.find().map(d => d.n).map(n => n.toUpperCase()).toArray().join(','));\nprint(db.fs.files.getFullName())",
    );
    assert!(err.is_none(), "{err:?}");
    assert_eq!(prints, ["1", "A", "demo-games.fs.files"]);

    // A subtype-4 binary that isn't 16 bytes, and a regex with the x option.
    let (prints, _, _, err) = f.run(
        "db.odd.insertOne({ _id: 1, b: BinData(4, 'AQID'), r: BSONRegExp('a b', 'ix') });\nconst d = db.odd.findOne({ _id: 1 });\ndb.odd.replaceOne({ _id: 1 }, d);\nprint(db.odd.findOne({ _id: 1 }).r.options)",
    );
    assert!(err.is_none(), "{err:?}");
    assert_eq!(prints, ["ix"]);
}
