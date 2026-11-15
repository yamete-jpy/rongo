use std::sync::Arc;
use std::time::{Duration, Instant};

use rongo_core::{Namespace, Session};
use rongo_core::bson::{Bson, doc};
use rongo_core::conn::Conn;
use rongo_core::mock::MockStore;

use super::*;

struct Fixture {
    _rt: tokio::runtime::Runtime,
    shell: ShellHandle,
    conn: Conn,
}

impl Fixture {
    fn new() -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let conn = Conn::mock(MockStore::demo(), false);
        let session = Session { conn: conn.clone(), rt: rt.handle().clone() };
        let shell = ShellHandle::spawn(session, Arc::new(|| {}));
        Fixture { _rt: rt, shell, conn }
    }

    /// Documents put straight into the demo server, as stored, not through a script.
    fn insert(&self, coll: &str, docs: Vec<rongo_core::bson::Document>) {
        self._rt.block_on(self.conn.insert_many(&Namespace::new("demo-games", coll), docs)).unwrap();
    }

    /// A collection's documents as the server holds them, in `_id` order.
    fn stored(&self, coll: &str) -> Vec<rongo_core::bson::Document> {
        let opts = rongo_core::conn::FindOpts { sort: Some(doc! { "_id": 1 }), ..Default::default() };
        self._rt.block_on(async {
            let mut c = self.conn.open_find(&Namespace::new("demo-games", coll), doc! {}, opts).await?;
            c.next_batch(10_000).await
        })
        .unwrap()
    }

    /// Run a script and collect everything it produced.
    fn run(&mut self, code: &str) -> (Vec<String>, Vec<ResultItem>, String, Option<ShellError>) {
        let run = self.shell.run(code, "demo-games", 0);
        let (mut prints, mut results) = (Vec::new(), Vec::new());
        // Only a guard against a hang: a loaded machine can be slow.
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            match self.shell.try_recv() {
                Some(ShellEvent::Print { run: r, texts }) if r == run => prints.extend(texts),
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
fn shell_writes_on_a_read_only_connection_fail() {
    let mut f = Fixture::new();
    f.conn.set_read_only(true);
    let (_, _, _, err) = f.run("db.x.insertOne({a:1})");
    let err = err.expect("the write must fail");
    assert!(err.message.contains("read-only"), "{err:?}");
    let (_, _, _, err) = f.run("db.advertisers.deleteMany({})");
    assert!(err.unwrap().message.contains("read-only"));
    let (prints, _, _, err) = f.run("print(db.advertisers.countDocuments({}))");
    assert_eq!(err, None);
    assert_eq!(prints, ["4"]);
    f.conn.set_read_only(false);
    let (_, _, _, err) = f.run("db.x.insertOne({a:1})");
    assert_eq!(err, None);
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

/// An update given as a pipeline, and a large insertMany, reach the server.
#[test]
fn pipeline_updates_and_large_inserts() {
    let mut f = Fixture::new();
    let (prints, _, _, err) = f.run("db.pu.insertMany(Array.from({ length: 5000 }, (_, i) => ({ _id: i, n: i })));\nprint(db.pu.countDocuments({}), db.pu.findOne({ _id: 4999 }).n)");
    assert_eq!((prints, err), (vec!["5000 4999".to_string()], None));
    // The demo server refuses pipeline updates: getting that far is the point.
    let (_, _, _, err) = f.run("db.pu.updateMany({ n: { $lt: 10 } }, [{ $set: { m: '$n' } }])");
    assert!(err.as_ref().is_some_and(|e| e.message.contains("does not support pipeline updates")), "{err:?}");
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
    // `run` clears the flag before the job is queued, so an interrupt sent
    // at once (however late the script starts) still stops it.
    let run = f.shell.run("while (true) {}", "demo-games", 0);
    f.shell.interrupt();
    let deadline = Instant::now() + Duration::from_secs(60);
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

/// A dropped handle stops its script: a tab dropped while its script runs
/// (Disconnect) must not leave it writing, with no Stop to press.
#[test]
fn dropping_the_handle_stops_its_script() {
    let mut f = Fixture::new();
    let session = Session { conn: f.conn.clone(), rt: f._rt.handle().clone() };
    let mut doomed = ShellHandle::spawn(session, Arc::new(|| {}));
    let run = doomed.run("for (let i = 0; ; i++) { db.endless.insertOne({ i: i }); if (i == 0) print('started'); sleep(5) }", "demo-games", 0);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match doomed.try_recv() {
            Some(ShellEvent::Print { run: r, .. }) if r == run => break,
            Some(_) => {}
            None => {
                assert!(Instant::now() < deadline, "the script did not start");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
    drop(doomed);
    let mut count = || f.run("print(db.endless.countDocuments({}))").0[0].parse::<u64>().unwrap();
    // Running, it writes about one document every 5 ms.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let before = count();
        std::thread::sleep(Duration::from_millis(300));
        if count() == before {
            break;
        }
        assert!(Instant::now() < deadline, "the script goes on writing after its handle was dropped");
    }
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
    let run = f.shell.run("for (let i = 0; i < 20; i++) {\n  try { sleep(100) } catch (e) {}\n  try { db.stopped.insertOne({ i: i }) } catch (e) {}\n  print('step');\n}", "demo-games", 0);
    // Stop once the script has really started stepping, not after a fixed
    // time, which a loaded machine can overrun by many steps.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match f.shell.try_recv() {
            Some(ShellEvent::Print { run: r, .. }) if r == run => break,
            Some(_) => {}
            None => {
                assert!(Instant::now() < deadline, "the script did not start");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
    f.shell.interrupt();
    let deadline = Instant::now() + Duration::from_secs(60);
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
    // Caught, Stop would let all 20 steps write; it ends them soon after the
    // first (a few more may land while this thread is slow to interrupt).
    assert!(written <= 10, "{written} writes of 20 steps; Stop should end them");
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

/// Every regex the server can hold reads back as exactly itself from the
/// text the document editor, the dialogs and Copy show. A pattern must
/// never end its `/…/` early: whatever followed was read as code. A plain
/// pattern still shows as `/…/`.
#[test]
fn every_regex_reads_back_as_itself() {
    use rongo_core::bson::Regex;
    use rongo_core::format::{self, Style};
    let regex = |p: &str, o: &str| Bson::RegularExpression(Regex { pattern: p.try_into().unwrap(), options: o.try_into().unwrap() });
    let awkward = [
        ("abc\ndef", "i"),     // a line break
        ("a\r\u{2028}b", ""), // other line terminators
        ("abc[", "i"),         // a class with no ]
        ("tail\\", ""),        // a trailing backslash
        ("*star", ""),         // `/*` starts a comment
        ("", ""),              // `//` starts a comment
        ("a/b", ""),           // a slash
        ("a b", "ix"),         // options JS doesn't have
        ("(?i)abc", ""),       // PCRE's inline flags, which JS refuses
        ("(?P<n>x)", ""),      // a PCRE named group
        ("a{2,1}", ""),        // a quantifier range JS refuses
        (r"a\-b", "u"),        // an identity escape, refused with u
        (r"\p{L}", ""),        // fine without u, another meaning with it
        (r"(?<n>x)\k<n>", "u"), // a JS named group and its backreference
        ("a++b", ""),          // a possessive quantifier
        ("(abc", ""),          // a group never closed
        ("abc)", ""),          // a group never opened
        ("^*a", ""),           // nothing to repeat
        ("{2}x", ""),          // a quantifier with nothing before it
        ("x{99999999999}", ""), // a count too large
        ("[z-a]", ""),         // a class range out of order
        ("(?<=a)+b", ""),      // a repeated lookbehind
    ];
    for style in [Style::Editable, Style::Display] {
        let plain = regex("^SALE_", "i");
        assert_eq!(format::to_shell(&plain, style), "/^SALE_/i");
        assert_eq!(parse_value(&format::to_shell(&plain, style)).unwrap(), plain);
        // Everyday patterns stay `/…/`, and JS reads them as written.
        for (p, o) in [(r"^[a-z0-9._%+-]+@[a-z0-9.-]+\.[a-z]{2,}$", "i"), (r"(?:ab)+c??|d{1,3}?", ""), (r"(?=x)(?!y)(?<=z)(?<!w)\bq\B", "s"), (r"[]a]{,2}\p{L}", ""), ("é+", "mu")] {
            let v = regex(p, o);
            let text = format::to_shell(&v, style);
            assert_eq!(text, format!("/{p}/{o}"));
            assert_eq!(parse_value(&text).map_err(|e| e.message), Ok(v.clone()), "{text}");
        }
        for (p, o) in &awkward {
            let v = regex(p, o);
            let text = format::to_shell(&v, style);
            assert_eq!(parse_value(&text).map_err(|e| e.message), Ok(v.clone()), "{p:?} {o:?} written as {text}");
        }
        // Two on one line: an open class in the first must not swallow the second.
        let pair = Bson::Array(vec![regex("abc[", ""), regex("]", "i")]);
        let text = format::to_shell(&pair, style);
        assert_eq!(parse_value(&text).map_err(|e| e.message), Ok(pair.clone()), "written as {text}");
        let d = doc! { "_id": 1, "rule": regex("abc\ndef", "i") };
        assert_eq!(parse_document(&format::doc_to_shell(&d, style)).map_err(|e| e.message), Ok(d));
    }
}

/// A Symbol and a DBPointer (old types a server can still hold) read back
/// as themselves from the editor's text: a DBPointer once couldn't be
/// opened at all, and a Symbol came back a String.
#[test]
fn symbols_and_db_pointers_read_back_as_themselves() {
    use rongo_core::format::{self, Style};
    // A DbPointer can only be made from Extended JSON.
    let ptr = Bson::try_from(serde_json::json!({ "$dbPointer": { "$ref": "shop.\"orders\"", "$id": { "$oid": "64f6ef45435c6900a1b2c3d4" } } })).unwrap();
    let d = doc! { "_id": 1, "sym": Bson::Symbol("say \"hi\"".into()), "ptr": ptr, "list": [Bson::Symbol("x".into())] };
    for style in [Style::Editable, Style::Display] {
        let text = format::doc_to_shell(&d, style);
        assert_eq!(parse_document(&text).map_err(|e| e.message), Ok(d.clone()), "{text}");
    }
}

/// Text from the document editor and the dialogs runs without a shell:
/// sleep() there would hold up the window, so it refuses.
#[test]
fn sleep_does_not_run_outside_the_shell() {
    let started = Instant::now();
    let r = parse_value("(sleep(3000), 1)");
    assert!(r.is_err(), "{r:?}");
    // (Shorter than the 3 s it asked for, with room for a loaded machine.)
    assert!(started.elapsed() < Duration::from_millis(2900), "took {:?}, as if it slept", started.elapsed());
}

/// How long a paste of 10,000 documents takes to read: mongoexport's JSON
/// lines, and the shell syntax Copy writes.
/// `cargo test -p rongo-shell --release -- --ignored --nocapture report_paste_time`
#[test]
#[ignore]
fn report_paste_time() {
    use rongo_core::format::{self, Style};
    let docs: Vec<rongo_core::bson::Document> = (0..10_000_i64)
        .map(|i| doc! { "_id": rongo_core::bson::oid::ObjectId::new(), "name": format!("Player {i}"), "n": i, "big": i * 1_000_000_000, "price": 1.5 + i as f64, "at": rongo_core::bson::DateTime::from_millis(1_767_323_045_000 + i), "tags": ["a", "b"], "nested": { "x": i, "ok": true } })
        .collect();
    let export: String = docs.iter().map(|d| Bson::Document(d.clone()).into_relaxed_extjson().to_string() + "\n").collect();
    let shell: String = docs.iter().map(|d| format::doc_to_shell(d, Style::Editable) + "\n").collect();
    for (label, text) in [("mongoexport JSON lines", &export), ("shell syntax", &shell)] {
        let started = Instant::now();
        let got = parse_documents_text(text).map(|d| d.len());
        println!("{label}: {} KB, {got:?} in {:?}", text.len() / 1024, started.elapsed());
    }
    // One small parse, as the query bar and the dialogs do as you type.
    let started = Instant::now();
    for _ in 0..200 {
        parse_document("{ status: 'x', at: { $gte: ISODate('2026-01-01') } }").unwrap();
    }
    println!("one small parse_document: {:?}", started.elapsed() / 200);
}

/// How long common scripts take against the demo server, which answers at
/// once: what is left is the shell's own cost.
/// `cargo test -p rongo-shell --release -- --ignored --nocapture report_script_time`
#[test]
#[ignore]
fn report_script_time() {
    let mut f = Fixture::new();
    let doc = |i: i64| doc! { "_id": i, "name": format!("Player {i}"), "n": i, "big": i * 1_000_000_000, "price": 1.5 + i as f64, "whole": 3.0, "at": rongo_core::bson::DateTime::from_millis(1_767_323_045_000 + i), "tags": ["a", "b"], "nested": { "x": i, "ok": true } };
    f.insert("perf", (0..20_000).map(doc).collect());
    let uuid = |i: u8| Bson::Binary(rongo_core::bson::Binary { subtype: rongo_core::bson::spec::BinarySubtype::Uuid, bytes: [i; 16].to_vec() });
    f.insert("uuids", (0..5_000).map(|i: i64| doc! { "_id": i, "ref": uuid(7), "u": uuid((i % 251) as u8) }).collect());
    for (label, script) in [
        ("aggregate tab, 1000 documents", "db.perf.aggregate([{ $sort: { _id: 1 } }])"),
        ("toArray, 20,000 documents", "print(db.perf.find().toArray().length)"),
        ("toArray, 20,000 documents in one batch", "print(db.perf.find().batchSize(100000).toArray().length)"),
        ("toArray, 5,000 documents with two UUIDs", "print(db.uuids.find().toArray().length)"),
        ("the same, then insertMany of them", "const u = db.uuids.find().toArray();\ndb.uuids2.insertMany(u); print(db.uuids2.countDocuments({}))"),
        ("insertMany, 20,000 documents", "const docs = []; for (let i = 0; i < 20000; i++) docs.push({ i: i, name: 'x' + i, at: new Date(), nested: { a: [1, 2, 3] } });\ndb.ins.insertMany(docs); print(db.ins.countDocuments({}))"),
        ("100,000 prints", "for (let i = 0; i < 100000; i++) print(i)"),
    ] {
        let started = Instant::now();
        let (prints, results, _, err) = f.run(script);
        assert_eq!(err, None, "{label}");
        println!("{label}: {:?} ({} prints, {} results)", started.elapsed(), prints.len(), results.len());
    }
    // How many events (each a repaint) 100,000 prints make.
    let run = f.shell.run("for (let i = 0; i < 100000; i++) print(i)", "demo-games", 0);
    let mut events = 0;
    loop {
        match f.shell.try_recv() {
            Some(ShellEvent::Print { .. }) => events += 1,
            Some(ShellEvent::Done { run: r, .. }) if r == run => break,
            _ => std::thread::sleep(Duration::from_millis(1)),
        }
    }
    println!("100,000 prints: {events} print events");
}

/// 100,000 cursors read one after another: what a run keeps of them shows
/// in the process's peak memory.
/// `/usr/bin/time -l cargo test -p rongo-shell --release -- --ignored --nocapture report_many_cursors`
#[test]
#[ignore]
fn report_many_cursors() {
    let mut f = Fixture::new();
    f.insert("few", (0..3).map(|i| doc! { "_id": i, "pad": "x".repeat(500) }).collect());
    let started = Instant::now();
    let (prints, _, _, err) = f.run("let n = 0;\nfor (let i = 0; i < 100000; i++) n += db.few.find({}).toArray().length;\nprint(n)");
    assert_eq!(err, None);
    println!("100,000 find().toArray(): {:?} ({prints:?})", started.elapsed());
}

/// A quote inside a regex or a block comment must not hide a second object
/// from the count (JS's comma operator would keep only the last one), and
/// text between or after pasted documents is an error, not dropped.
#[test]
fn quotes_in_regexes_and_comments_do_not_hide_objects() {
    assert!(parse_document("{ a: /'/ }, { b: 1 }").is_err());
    assert!(parse_document("{ a: 1 } /* don't */, { b: 1 }").is_err());
    assert!(parse_value("1, 2").is_err());
    // A lone comma is a hole in the list, not null.
    assert!(parse_value(",").is_err(), "{:?}", parse_value(","));
    assert!(parse_document("{ a: 1 },,").is_err());
    assert_eq!(parse_value("[1, , 2]").unwrap(), Bson::Array(vec![Bson::Int32(1), Bson::Null, Bson::Int32(2)]), "a hole inside the value is JS's own");
    let docs = parse_documents_text("{ a: /'/ }, { b: 1 }").unwrap();
    assert_eq!(docs.len(), 2, "{docs:?}");
    assert_eq!(docs[1], doc! { "b": 1 });
    let e = parse_documents_text("{ a: 1 }\n{ b: 2 } oops(").unwrap_err();
    assert!(e.message.contains("oops("), "{e:?}");
    assert!(parse_documents_text("{ a: 1 }\nNumberLong(5)\n{ b: 2 }").is_err());
    assert_eq!(parse_documents_text("{ a: 1 } /* don't */ { b: 2 }\n// the end").unwrap(), [doc! { "a": 1 }, doc! { "b": 2 }]);
    assert_eq!(parse_documents_text("// 1\n{ a: 1 }\n// 2\n{ b: 2 }").unwrap(), [doc! { "a": 1 }, doc! { "b": 2 }]);
    // The second document is the broken one, and the message says so.
    let e = parse_documents_text("{ a: 1 }\n{ b: }\n{ c: 3 }").unwrap_err();
    assert!(e.message.contains("document 2"), "{e:?}");
}

/// What a detached parse says when the text is empty, too slow or too big.
#[test]
fn detached_parse_explains_empty_slow_and_huge_text() {
    let e = parse_value("  ").unwrap_err();
    assert!(e.message.contains("empty"), "{e:?}");
    let e = parse_value("(() => { while (true) {} })()").unwrap_err();
    assert!(e.message.contains("2 seconds"), "{e:?}");
    let e = parse_documents_text("{ a: (() => { while (true) {} })() }").unwrap_err();
    assert!(e.message.contains("2 seconds") && e.message.contains("fewer"), "{e:?}");
    // 128 MB in one native call: refused at once, not after a second of work.
    let started = Instant::now();
    let e = parse_value("'x'.repeat(2 ** 27).length").unwrap_err();
    assert!(e.message.contains("memory"), "{e:?}");
    assert!(started.elapsed() < Duration::from_millis(1500), "{:?}", started.elapsed());
}

/// Pasted (Extended) JSON is read directly, without the JS engine: key order
/// stays as written (JS moves integer-like keys first), every Extended JSON
/// type is read, `5.0` is a Double, and every double keeps its exact bits.
#[test]
fn pasted_json_keeps_order_types_and_exact_doubles() {
    let keys = |d: &rongo_core::bson::Document| d.keys().cloned().collect::<Vec<_>>();
    let docs = parse_documents_text("{\"10\": 1, \"9\": 2, \"a\": 3, \"_id\": {\"1\": 1, \"0\": 0}}\n{\"b\": 1}").unwrap();
    assert_eq!(keys(&docs[0]), ["10", "9", "a", "_id"]);
    assert_eq!(keys(docs[0].get_document("_id").unwrap()), ["1", "0"]);
    let d = &parse_documents_text(r#"{"t": {"$timestamp": {"t": 1, "i": 2}}, "lo": {"$minKey": 1}, "hi": {"$maxKey": 1}, "f": 5.0, "i": 5, "l": 3000000000}"#).unwrap()[0];
    assert_eq!(d.get("t"), Some(&Bson::Timestamp(rongo_core::bson::Timestamp { time: 1, increment: 2 })));
    assert_eq!((d.get("lo"), d.get("hi")), (Some(&Bson::MinKey), Some(&Bson::MaxKey)));
    assert_eq!((d.get("f"), d.get("i"), d.get("l")), (Some(&Bson::Double(5.0)), Some(&Bson::Int32(5)), Some(&Bson::Int64(3_000_000_000))));
    // Doubles from all over the range, written as JSON numbers, read back bit for bit.
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let doubles: Vec<f64> = (0..5000)
        .map(|_| {
            x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            f64::from_bits(x)
        })
        .filter(|f| f.is_finite())
        .chain([0.1, 0.3_f64 + 0.000_000_000_000_000_04, 5e-324, f64::MAX, f64::MIN_POSITIVE, 123_456_789_012_345_680.0])
        .collect();
    let text: String = doubles.iter().map(|f| format!("{{\"f\": {}}}\n", serde_json::to_string(f).unwrap())).collect();
    let docs = parse_documents_text(&text).unwrap();
    for (f, d) in doubles.iter().zip(&docs) {
        let Some(Bson::Double(g)) = d.get("f") else { panic!("{f} read as {d:?}") };
        assert_eq!(g.to_bits(), f.to_bits(), "{f} read as {g}");
    }
}

/// An aggregation's result tab shows the documents as the server sent them:
/// a whole-number double stays a double (3.0, 1e12, -0.0), not Int32/Int64.
#[test]
fn aggregate_tabs_keep_doubles() {
    let mut f = Fixture::new();
    f.insert("dbl", vec![doc! { "_id": 1, "v": 3.0 }, doc! { "_id": 2, "v": 1e12 }, doc! { "_id": 3, "v": -0.0 }, doc! { "_id": 4, "v": 2.5 }]);
    let (_, results, _, err) = f.run("db.dbl.aggregate([{ $sort: { _id: 1 } }])");
    assert_eq!(err, None);
    let [ResultItem::Docs { docs, title, line, truncated }] = results.as_slice() else { panic!("{results:?}") };
    assert_eq!((title.as_str(), *line, *truncated), ("Aggregate", 1, false));
    let v: Vec<_> = docs.iter().map(|d| d.get("v").cloned()).collect();
    assert_eq!(v, [Some(Bson::Double(3.0)), Some(Bson::Double(1e12)), Some(Bson::Double(-0.0)), Some(Bson::Double(2.5))]);
    assert!(matches!(docs[2].get("v"), Some(Bson::Double(z)) if z.is_sign_negative()), "{:?}", docs[2]);
    // More than a tab holds: the first 1000, marked truncated.
    f.insert("many", (0..1200).map(|i| doc! { "_id": i, "v": 1.0 }).collect());
    let (_, results, _, err) = f.run("db.many.aggregate([{ $sort: { _id: 1 } }])");
    assert_eq!(err, None);
    let [ResultItem::Docs { docs, truncated, .. }] = results.as_slice() else { panic!("{results:?}") };
    assert_eq!((docs.len(), *truncated, docs[999].get("_id"), docs[999].get("v")), (1000, true, Some(&Bson::Int32(999)), Some(&Bson::Double(1.0))));
    // Read by the script, it is consumed: no tab, and every document seen once.
    let (prints, results, _, err) = f.run("const c = db.many.aggregate([{ $sort: { _id: 1 } }]);\nlet n = 0; c.forEach(() => n++); print(n)");
    assert_eq!((prints, results.len(), err), (vec!["1200".to_string()], 0, None));
}

/// -0 is a Double with its sign: typed, through Copy (both text styles) and
/// Paste, and written by a script.
#[test]
fn negative_zero_keeps_its_sign() {
    use rongo_core::format::{self, Style};
    let neg = |v: Option<&Bson>| matches!(v, Some(Bson::Double(z)) if *z == 0.0 && z.is_sign_negative());
    assert!(neg(Some(&parse_value("-0.0").unwrap())), "{:?}", parse_value("-0.0"));
    assert!(neg(Some(&parse_value("Double(-0)").unwrap())), "{:?}", parse_value("Double(-0)"));
    let d = doc! { "z": -0.0 };
    for style in [Style::Editable, Style::Display] {
        let text = format::doc_to_shell(&d, style);
        assert!(neg(parse_documents_text(&text).unwrap()[0].get("z")), "{text}");
    }
    let mut f = Fixture::new();
    let (_, _, _, err) = f.run("db.nz.insertOne({ _id: 1, a: -0, b: Double(-0), c: 0 })");
    assert_eq!(err, None);
    let stored = &f.stored("nz")[0];
    assert!(neg(stored.get("a")) && neg(stored.get("b")), "{stored:?}");
    assert_eq!(stored.get("c"), Some(&Bson::Int32(0)));
}

/// A server error thrown into a script carries its code and code name, so
/// the usual `if (e.code === 11000)` works; errors of rongo's own have none.
#[test]
fn server_errors_carry_their_code() {
    let mut f = Fixture::new();
    let (prints, _, _, err) = f.run(
        "db.dup.insertOne({ _id: 1 });\ntry { db.dup.insertOne({ _id: 1 }) } catch (e) { print(e.code, e.codeName, e instanceof Error, e.message.includes('E11000')) }\ntry { db.dup.dropIndex('nope') } catch (e) { print(e.code, e.codeName) }",
    );
    assert_eq!(err, None);
    assert_eq!(prints, ["11000 DuplicateKey true true", "27 IndexNotFound"]);
    f.conn.set_read_only(true);
    let (prints, _, _, err) = f.run("try { db.dup.insertOne({ _id: 2 }) } catch (e) { print(e.code === undefined, e.codeName === undefined) }");
    assert_eq!((prints, err), (vec!["true true".to_string()], None));
}

/// A write-concern error (applied on the primary, not confirmed) carries
/// its code and code name like any server error, so a script can tell it
/// from a refusal; an error of rongo's own still has none.
#[test]
fn write_concern_errors_carry_their_code() {
    use rongo_core::CoreError;
    let reply = |e: CoreError| serde_json::from_str::<serde_json::Value>(&runtime::core_error_reply(e)).unwrap();
    let r = reply(CoreError::WriteConcern { code: 64, msg: "waiting for replication timed out".into() });
    assert_eq!((r["err"].as_str(), r["code"].as_i64(), r["codeName"].as_str()), (Some("write concern error 64: waiting for replication timed out"), Some(64), Some("WriteConcernFailed")));
    let r = reply(CoreError::Server { code: 11000, msg: "E11000 duplicate key error".into() });
    assert_eq!((r["code"].as_i64(), r["codeName"].as_str()), (Some(11000), Some("DuplicateKey")));
    let r = reply(CoreError::ReadOnly("insert".into()));
    assert!(r.get("code").is_none() && r.get("codeName").is_none(), "{r}");
}

/// Promise jobs run once the script's statements have: everything after an
/// `await` in an async function, and `.then()` callbacks. A rejection
/// nobody handles is the run's error, and a promise as the final value
/// shows what it resolved to.
#[test]
fn awaits_and_thens_run() {
    let mut f = Fixture::new();
    let (prints, _, _, err) = f.run(
        "async function main() {\n  print('start');\n  await null;\n  db.pj.insertOne({ _id: 1 });\n  print('after await', db.pj.countDocuments({}));\n}\nmain();\nPromise.resolve(5).then(v => print('then', v));\nprint('sync end')",
    );
    assert_eq!(err, None);
    assert_eq!(prints, ["start", "sync end", "after await 1", "then 5"]);
    let (_, _, _, err) = f.run("async function late() {\n  await null;\n  throw new Error('late failure');\n}\nlate()\nprint('more')");
    let err = err.expect("the rejection is reported");
    assert!(err.message.contains("late failure"), "{err:?}");
    assert_eq!(err.line, Some(3));
    let (prints, _, _, err) = f.run("const p = Promise.reject(new Error('handled'));\np.catch(e => print('caught', e.message))");
    assert_eq!((prints, err), (vec!["caught handled".to_string()], None));
    let (_, results, _, err) = f.run("(async () => { await null; return db.pj.findOne({ _id: 1 }) })()");
    assert_eq!(err, None);
    let [ResultItem::Docs { docs, .. }] = results.as_slice() else { panic!("{results:?}") };
    assert_eq!(docs[0], doc! { "_id": 1 });
}

/// Stop ends a script whose promise jobs never finish, and they do not go
/// on in the next run.
#[test]
fn stop_ends_endless_promise_jobs() {
    let mut f = Fixture::new();
    let run = f.shell.run("let n = 0;\nfunction again() { if (++n % 1000 == 0) print('spinning'); return Promise.resolve().then(again) }\nagain()", "demo-games", 0);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match f.shell.try_recv() {
            Some(ShellEvent::Print { run: r, .. }) if r == run => break,
            Some(ShellEvent::Done { run: r, error, .. }) if r == run => panic!("finished early: {error:?}"),
            Some(_) => {}
            None => {
                assert!(Instant::now() < deadline, "the jobs did not start");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
    f.shell.interrupt();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match f.shell.try_recv() {
            Some(ShellEvent::Done { run: r, error, .. }) if r == run => {
                assert_eq!(error.map(|e| e.message), Some("Interrupted".to_string()));
                break;
            }
            _ => {
                assert!(Instant::now() < deadline, "Stop did not end the jobs");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
    let (prints, _, _, err) = f.run("print('still alive')");
    assert_eq!((prints, err), (vec!["still alive".to_string()], None));
}

/// A regex a script reads and writes back keeps its pattern bytes: JS
/// rewrites `source` for an empty pattern, a slash and a line break.
#[test]
fn regexes_read_and_written_back_keep_their_pattern() {
    use rongo_core::bson::Regex;
    let regex = |p: &str, o: &str| Bson::RegularExpression(Regex { pattern: p.try_into().unwrap(), options: o.try_into().unwrap() });
    let original = doc! { "_id": 1, "empty": regex("", ""), "slash": regex("a/b", "i"), "nl": regex("a\nb", "m"), "plain": regex("^ab+c", "i") };
    let mut f = Fixture::new();
    f.insert("rx", vec![original.clone()]);
    let (prints, _, _, err) = f.run("const d = db.rx.findOne({ _id: 1 });\ndb.rx.replaceOne({ _id: 1 }, d);\nprint(d.plain instanceof RegExp, d.plain.test('ABBC'))");
    assert_eq!(err, None);
    assert_eq!(prints, ["true true"]);
    assert_eq!(f.stored("rx"), [original]);
}

/// An option or cursor method the shell can't honour, and whose loss could
/// change the result, throws instead of being dropped without a word (a
/// dropped `explain` returned documents, a dropped `limit()` on an
/// aggregation returned everything; a dropped `hint` can change which
/// document updateOne acts on). Options whose loss changes nothing
/// (`comment`, `maxTimeMS`, `allowDiskUse`, `batchSize`, `writeConcern`)
/// are accepted. The small ones are done properly.
#[test]
fn options_the_shell_cannot_honour_throw() {
    let mut f = Fixture::new();
    f.insert("op", (0..10).map(|i| doc! { "_id": i, "k": i % 2 }).collect());
    for call in [
        "db.op.aggregate([{ $match: {} }], { explain: true })",
        "db.op.aggregate([{ $match: {} }], { hint: { _id: 1 } })",
        "db.op.aggregate([{ $match: {} }]).limit(5).toArray()",
        "db.op.aggregate([{ $match: {} }]).sort({ _id: -1 }).toArray()",
        "db.op.find().hint({ _id: 1 }).toArray()",
        "db.op.find({}, {}, { collation: { locale: 'en' } }).toArray()",
        "db.op.findOne({}, {}, { hint: { _id: 1 } })",
        "db.op.updateOne({ _id: 1 }, { $set: { a: 1 } }, { arrayFilters: [] })",
        "db.op.updateMany({}, { $set: { a: 1 } }, { collation: { locale: 'en' } })",
        "db.op.replaceOne({ _id: 1 }, { a: 1 }, { hint: { _id: 1 } })",
        "db.op.insertMany([{ a: 1 }], { ordered: false })",
        "db.op.deleteOne({ k: 1 }, { hint: { _id: 1 } })",
        "db.op.deleteMany({ k: 1 }, { collation: { locale: 'en' } })",
        "db.op.distinct('k', {}, { collation: { locale: 'en' } })",
        "db.op.countDocuments({}, { hint: { _id: 1 } })",
        "db.createCollection('capped', { capped: true, size: 1024 })",
    ] {
        let (_, _, _, err) = f.run(call);
        assert!(err.as_ref().is_some_and(|e| e.message.contains("not supported")), "{call}: {err:?}");
    }
    assert_eq!(f.stored("op"), (0..10).map(|i| doc! { "_id": i, "k": i % 2 }).collect::<Vec<_>>(), "a refused call wrote");
    let (prints, _, _, err) = f.run(
        "print(db.op.find({}, {}, { maxTimeMS: 50, comment: 'c', allowDiskUse: true, batchSize: 3 }).maxTimeMS(50).comment('c').allowDiskUse().toArray().length)\n\
         print(db.op.aggregate([{ $sort: { _id: 1 } }], { allowDiskUse: true, maxTimeMS: 50, comment: 'c', cursor: { batchSize: 2 }, writeConcern: { w: 1 } }).toArray().length)\n\
         print(db.op.findOne({ _id: 2 }, null, { maxTimeMS: 50, comment: 'c' })._id, db.op.countDocuments({}, { maxTimeMS: 50, comment: 'c' }), db.op.estimatedDocumentCount({ maxTimeMS: 50 }), db.op.distinct('k', {}, { comment: 'c' }).length)\n\
         print(db.op.insertOne({ _id: 50 }, { writeConcern: { w: 1 }, comment: 'c' }).insertedId, db.op.insertMany([{ _id: 51 }], { ordered: true, writeConcern: { w: 1 } }).insertedIds[0])\n\
         print(db.op.updateOne({ _id: 50 }, { $set: { u: 1 } }, { writeConcern: { w: 1 }, comment: 'c' }).modifiedCount, db.op.replaceOne({ _id: 51 }, { r: 1 }, { comment: 'c' }).modifiedCount, db.op.deleteMany({ _id: { $in: [50, 51] } }, { writeConcern: { w: 1 }, comment: 'c' }).deletedCount)",
    );
    assert_eq!(err, None, "{prints:?}");
    assert_eq!(prints, ["10", "10", "2 10 10 2", "50 51", "1 1 2"]);
    let (_, _, _, err) = f.run("db.getCollectionInfos({ 'options.capped': true })");
    assert_eq!(err.map(|e| e.message), Some("TypeError: not supported: getCollectionInfos() filter on options.capped".to_string()));
    let (prints, _, _, err) = f.run(
        "print(db.op.countDocuments({}, { skip: 3, limit: 5 }), db.op.countDocuments({ k: 1 }, { skip: 4 }), db.op.find().skip(8).limit(5).size(), db.op.find().skip(2).count(), db.op.find().skip(2).count(true))\n\
         print(db.op.updateOne({ _id: 99 }, { $set: { a: 1 } }, { upsert: true }).upsertedCount, db.op.updateOne({ _id: 1 }, { $set: { b: 1 } }).upsertedCount)\n\
         print(db.op.find({}, { k: 1 }, { sort: { _id: -1 }, limit: 2 }).toArray().map(d => d._id).join(','))\n\
         print(db.op.aggregate([{ $sort: { _id: 1 } }]).batchSize(3).toArray().length)\n\
         const infos = db.getCollectionInfos({ name: 'op' });\n\
         print(infos.length, infos[0].name, infos[0].type, db.getCollectionInfos({ type: 'view' }).length, db.getCollectionInfos({ name: /^o/ }).length)\n\
         print(db.op.remove({ k: 1 }, { justOne: false }).deletedCount)",
    );
    assert_eq!(err, None, "{prints:?}");
    assert_eq!(prints, ["5 1 2 10 8", "1 0", "99,9", "11", "1 op collection 0 1", "5"]);
}

/// An unread cursor with a `.map()` shows the mapped values, as mongosh
/// does; and a run makes at most 20 result tabs of unread cursors, saying
/// how many more there were.
#[test]
fn mapped_cursors_show_mapped_values_and_unread_tabs_are_capped() {
    let mut f = Fixture::new();
    f.insert("mp", (0..3).map(|i| doc! { "_id": i, "name": format!("n{i}") }).collect());
    let (prints, results, _, err) = f.run("db.mp.find().sort({ _id: 1 }).map(d => d.name)");
    assert_eq!(err, None);
    assert!(results.is_empty(), "{results:?}");
    let [text] = prints.as_slice() else { panic!("{prints:?}") };
    assert!(text.contains("\"n0\"") && text.contains("\"n2\"") && !text.contains("_id"), "{text}");
    let (_, results, _, err) = f.run("db.mp.find().sort({ _id: 1 })\n  .map(d => ({ upper: d.name.toUpperCase() }))");
    assert_eq!(err, None);
    let [ResultItem::Docs { docs, line, .. }] = results.as_slice() else { panic!("{results:?}") };
    assert_eq!((docs.as_slice(), *line), ([doc! { "upper": "N0" }, doc! { "upper": "N1" }, doc! { "upper": "N2" }].as_slice(), 1));
    let (prints, results, _, err) = f.run("for (let i = 0; i < 30; i++) db.mp.find({ _id: i })");
    assert_eq!(err, None);
    assert_eq!(results.len(), 20);
    assert!(prints.iter().any(|p| p.contains("10 more")), "{prints:?}");
}

/// An aggregation that fails, caught by the script, leaves no tab and
/// doesn't fail the run at its end.
#[test]
fn a_caught_aggregate_failure_ends_the_run_cleanly() {
    let mut f = Fixture::new();
    let (prints, results, _, err) = f.run("try { db.advertisers.aggregate([{ $bogus: 1 }]) } catch (e) { print('caught') }");
    assert_eq!((prints, results.len(), err), (vec!["caught".to_string()], 0, None));
}

/// The held batches of a run, as the shell thread counts them (tests).
fn held(prints: &[String]) -> (u64, u64) {
    let v: serde_json::Value = serde_json::from_str(prints.iter().find(|p| p.starts_with("{\"ok\"")).expect("held_stats printed")).unwrap();
    (v["ok"]["count"].as_u64().unwrap(), v["ok"]["bytes"].as_u64().unwrap())
}

/// Unread aggregations keep their first batch in Rust only up to a cap (as
/// many as a run shows as tabs, and a byte budget), so a loop of them can't
/// grow memory without bound. One past the cap still reads in full when the
/// script reads it, and still shows, from its pipeline, when it doesn't.
#[test]
fn unread_aggregations_hold_a_bounded_amount() {
    let mut f = Fixture::new();
    f.insert("small", (0..3).map(|i| doc! { "_id": i }).collect());
    let (prints, results, _, err) = f.run("for (let i = 0; i < 100; i++) db.small.aggregate([{ $match: {} }]);\nprint(__native('held_stats', '{}'))");
    assert_eq!(err, None);
    assert!(held(&prints).0 <= 20, "{prints:?}");
    assert_eq!(results.len(), 20);
    // Big batches: the byte budget (1 MiB in tests) holds only two of them.
    let pad = "x".repeat(50_000);
    f.insert("big", (0..10).map(|i| doc! { "_id": i, "pad": pad.clone(), "d": 3.0 }).collect());
    let (prints, results, _, err) = f.run(
        "const cs = [];\nfor (let i = 0; i < 30; i++) cs.push(db.big.aggregate([{ $sort: { _id: 1 } }]));\nprint(__native('held_stats', '{}'));\nprint(cs[29].toArray().length, cs[1].toArray().length)",
    );
    assert_eq!(err, None, "{prints:?}");
    let (count, bytes) = held(&prints);
    assert!(bytes <= runtime::HELD_BYTES as u64 && count < 30, "{count} batches, {bytes} bytes held");
    assert!(prints.iter().any(|p| p == "10 10"), "{prints:?}");
    // 28 unread: 20 tabs, each in full and with its types, held or not.
    assert_eq!(results.len(), 20);
    for r in &results {
        let ResultItem::Docs { docs, truncated: false, .. } = r else { panic!("{r:?}") };
        assert_eq!(docs.len(), 10);
        assert_eq!(docs[9].get("d"), Some(&Bson::Double(3.0)));
    }
}

/// How many times a writing pipeline has run in this shell (tests).
fn writing_runs(f: &mut Fixture) -> u64 {
    let (prints, _, _, err) = f.run("print(__native('held_stats', '{}'))");
    assert_eq!(err, None);
    let v: serde_json::Value = serde_json::from_str(&prints[0]).unwrap();
    v["ok"]["writing_runs"].as_u64().unwrap()
}

/// A pipeline that writes (`$out`/`$merge`, as the read-only guard spells
/// them) runs exactly once: past the held cap it is still held, never let go
/// to run again when the script reads it or it is shown. On a server its
/// first batch is empty; `{ $merge: "__echo" }` (tests) forces documents in.
#[test]
fn a_writing_pipeline_never_runs_twice() {
    let mut f = Fixture::new();
    f.insert("small", (0..3).map(|i| doc! { "_id": i }).collect());
    // Past the count cap (20 held), read by the script.
    let (prints, _, _, err) = f.run(
        "for (let i = 0; i < 20; i++) db.small.aggregate([{ $match: {} }]);\nconst w = db.small.aggregate([{ $match: {} }, { $merge: '__echo' }]);\nprint(w.toArray().length)",
    );
    assert_eq!(err, None, "{prints:?}");
    assert_eq!(prints, ["3"]);
    assert_eq!(writing_runs(&mut f), 1);
    // Past the byte budget (1 MiB in tests), shown as a result tab.
    let pad = "x".repeat(100_000);
    f.insert("big", (0..10).map(|i| doc! { "_id": i, "pad": pad.clone() }).collect());
    let (prints, results, _, err) = f.run("db.big.aggregate([{ $match: {} }]);\ndb.big.aggregate([{ $match: {} }, { $merge: '__echo' }])");
    assert_eq!(err, None, "{prints:?}");
    assert_eq!(results.len(), 2);
    let ResultItem::Docs { docs, .. } = &results[1] else { panic!("{:?}", results[1]) };
    assert_eq!(docs.len(), 10);
    assert_eq!(writing_runs(&mut f), 2);
}

/// "Interrupted" is said only of Stop: an error that merely mentions the
/// word keeps its text. A value print() can't show says why. `use x` takes
/// a trailing comment.
#[test]
fn errors_keep_their_text_and_print_says_why() {
    let mut f = Fixture::new();
    let (_, _, _, err) = f.run("throw new Error('interrupted by the user')");
    assert_eq!(err.map(|e| e.message), Some("Error: interrupted by the user".to_string()));
    let (prints, _, _, err) = f.run("print({ d: new Date(NaN) })");
    assert_eq!(err, None);
    assert!(prints[0].contains("invalid Date"), "{prints:?}");
    let (prints, _, db, err) = f.run("use analytics // the events\nprint(db.getName())");
    assert_eq!((prints, db, err), (vec!["switched to db analytics".to_string(), "analytics".to_string()], "analytics".to_string(), None));
    // Stop during a wait on the server (here, sleep) is still "Interrupted".
    let run = f.shell.run("print('waiting');\nsleep(60000)", "demo-games", 0);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match f.shell.try_recv() {
            Some(ShellEvent::Print { run: r, .. }) if r == run => f.shell.interrupt(),
            Some(ShellEvent::Done { run: r, error, .. }) if r == run => {
                assert_eq!(error.map(|e| e.message), Some("Interrupted".to_string()));
                break;
            }
            _ => {
                assert!(Instant::now() < deadline, "Stop did not end the wait");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

/// UUIDs keep every byte through a script and the detached parser: read as
/// UUID objects, written back as subtype 4.
#[test]
fn uuids_keep_every_byte() {
    let bytes: Vec<u8> = (0u8..16).map(|i| i.wrapping_mul(37).wrapping_add(0xf0)).collect();
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let uuid = Bson::Binary(rongo_core::bson::Binary { subtype: rongo_core::bson::spec::BinarySubtype::Uuid, bytes: bytes.clone() });
    assert_eq!(parse_value(&format!("UUID('{hex}')")).unwrap(), uuid);
    let dashed = format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..]);
    assert_eq!(parse_value(&format!("UUID('{}')", dashed.to_uppercase())).unwrap(), uuid);
    let mut f = Fixture::new();
    f.insert("uu", vec![doc! { "_id": 1, "u": uuid.clone(), "short": Bson::Binary(rongo_core::bson::Binary { subtype: rongo_core::bson::spec::BinarySubtype::Uuid, bytes: vec![1, 2, 3] }) }]);
    let (prints, _, _, err) = f.run("const d = db.uu.findOne({ _id: 1 });\nprint(d.u.toString(), d.short instanceof Object && !('hex' in d.short));\ndb.uu.insertOne({ _id: 2, u: d.u, fresh: UUID() })");
    assert_eq!(err, None);
    assert_eq!(prints, [format!("{dashed} true")]);
    let stored = f.stored("uu");
    assert_eq!(stored[1].get("u"), Some(&uuid));
    assert!(matches!(stored[1].get("fresh"), Some(Bson::Binary(b)) if b.subtype == rongo_core::bson::spec::BinarySubtype::Uuid && b.bytes.len() == 16));
}

/// Prints travel in batches, not one event (and one repaint) each; all of
/// them arrive, in order.
#[test]
fn prints_arrive_in_batches() {
    let mut f = Fixture::new();
    let run = f.shell.run("for (let i = 0; i < 20000; i++) print(i)", "demo-games", 0);
    let (mut events, mut texts) = (0, Vec::new());
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        match f.shell.try_recv() {
            Some(ShellEvent::Print { run: r, texts: t }) if r == run => {
                events += 1;
                texts.extend(t);
            }
            Some(ShellEvent::Done { run: r, error, .. }) if r == run => {
                assert_eq!(error, None);
                break;
            }
            Some(_) => {}
            None => {
                assert!(Instant::now() < deadline, "the script did not finish");
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
    assert_eq!(texts, (0..20000).map(|i| i.to_string()).collect::<Vec<_>>());
    assert!(events < 2000, "{events} events for 20,000 prints");
}

/// Documents reach a script as JSON text written straight from BSON. JS
/// must revive from it exactly the values it revived from canonical
/// Extended JSON: every BSON type, awkward values of each, and doubles,
/// Int32s and Int64s from all over their range.
#[test]
fn documents_for_scripts_revive_as_canonical_extended_json_did() {
    use rongo_core::bson::{Binary, DateTime, JavaScriptCodeWithScope, Regex, Timestamp, oid::ObjectId, spec::BinarySubtype};
    let bin = |st: BinarySubtype, b: &[u8]| Bson::Binary(Binary { subtype: st, bytes: b.to_vec() });
    let regex = |p: &str, o: &str| Bson::RegularExpression(Regex { pattern: p.try_into().unwrap(), options: o.try_into().unwrap() });
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut values: Vec<Bson> = vec![
        Bson::Int32(0), Bson::Int32(-1), Bson::Int32(i32::MIN), Bson::Int32(i32::MAX),
        Bson::Int64(0), Bson::Int64(5), Bson::Int64(i64::MIN), Bson::Int64(i64::MAX), Bson::Int64(9_007_199_254_740_993),
        Bson::Double(0.0), Bson::Double(-0.0), Bson::Double(0.5), Bson::Double(-0.1), Bson::Double(3.0), Bson::Double(1e12), Bson::Double(1e21), Bson::Double(1e300),
        Bson::Double(1e-7), Bson::Double(5e-324), Bson::Double(-5e-324), Bson::Double(f64::MIN_POSITIVE), Bson::Double(f64::MAX), Bson::Double(f64::MIN),
        Bson::Double(f64::NAN), Bson::Double(f64::INFINITY), Bson::Double(f64::NEG_INFINITY), Bson::Double(9_007_199_254_740_992.0), Bson::Double(123_456_789.123_456_78),
        Bson::String(String::new()), Bson::String("plain".into()), Bson::String("q\"uo\\te /".into()), Bson::String((0u8..0x20).map(char::from).chain(['\u{7f}']).collect()),
        Bson::String("é中文😀 \u{2028}\u{2029} </script>".into()), Bson::String("x".repeat(5000)),
        Bson::Boolean(true), Bson::Boolean(false), Bson::Null, Bson::Undefined, Bson::MinKey, Bson::MaxKey,
        Bson::ObjectId(ObjectId::parse_str("64f6ef45435c6900a1b2c3d4").unwrap()),
        Bson::DateTime(DateTime::from_millis(0)), Bson::DateTime(DateTime::from_millis(-1)), Bson::DateTime(DateTime::from_millis(1_767_323_045_123)),
        Bson::DateTime(DateTime::from_millis(i64::MAX)), Bson::DateTime(DateTime::from_millis(i64::MIN)),
        bin(BinarySubtype::Generic, b"hello"), bin(BinarySubtype::Generic, b""), bin(BinarySubtype::Uuid, &[0xab; 16]), bin(BinarySubtype::Uuid, &[1, 2, 3]),
        bin(BinarySubtype::UserDefined(0x80), &[0, 255]),
        regex("^SALE_", "i"), regex("a/b", ""), regex("", ""), regex("a\nb", "m"), regex("(?i)x", ""), regex("a b", "ix"), regex("[", ""),
        Bson::Timestamp(Timestamp { time: 1_700_000_000, increment: 7 }),
        Bson::Decimal128("1.10".parse().unwrap()), Bson::Decimal128("-12.50E+3".parse().unwrap()), Bson::Decimal128("NaN".parse().unwrap()),
        Bson::Symbol("sym \"x\"".into()),
        Bson::JavaScriptCode("function () { return 1; }".into()),
        Bson::JavaScriptCodeWithScope(JavaScriptCodeWithScope { code: "x + 1".into(), scope: doc! { "x": 1, "d": 2.5, "l": 3_i64 } }),
        Bson::try_from(serde_json::json!({ "$dbPointer": { "$ref": "a.b", "$id": { "$oid": "64f6ef45435c6900a1b2c3d4" } } })).unwrap(),
        Bson::Array(vec![]), Bson::Document(doc! {}),
        Bson::Document(doc! { "": 1, "a.b": 2.5, "\"q\"\n": [1, 2.5, { "x": -0.0 }], "10": "ten", "9": "nine", "$weird": true, "nested": { "deep": { "deeper": [[1_i64, Bson::Null]] } } }),
    ];
    for _ in 0..3000 {
        let r = next();
        values.push(Bson::Double(f64::from_bits(r)));
        values.push(Bson::Int32(r as i32));
        values.push(Bson::Int64(r as i64));
    }
    values.push(Bson::Array(values.clone()));
    assert_eq!(runtime::revive_difference(&values), None);
}

/// The document editor saves a document of several MB (MongoDB allows 16)
/// made of many small values, which needs far more than 64 MiB in the
/// engine; small typed text (the query bar, dialog fields) keeps 64 MiB,
/// and says so. (Release only: unoptimised QuickJS can't read it in 2 s.)
#[test]
#[cfg_attr(debug_assertions, ignore = "unoptimised QuickJS can't read it within 2 s; run with --release")]
fn the_document_editor_saves_multi_mb_documents() {
    use rongo_core::format::{self, Style};
    let d = doc! { "_id": 1, "rows": (0..200_000).map(|i| doc! { "i": i, "s": "ab" }).collect::<Vec<_>>() };
    let text = format::doc_to_shell(&d, Style::Editable);
    assert!(text.len() > 10 << 20, "{} bytes", text.len());
    assert_eq!(parse_editor_document(&text).map_err(|e| e.message), Ok(d));
    assert_eq!(parse_document(&text).map_err(|e| e.message), Err("the text needs more than 64 MiB of memory to evaluate".to_string()));
}
