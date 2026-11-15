//! How much memory 50,000 documents take as `bson::Document` versus raw BSON,
//! and how much a large copy holds at its peak.
//! Run with `cargo test -p rongo-core --release --test memory -- --ignored --nocapture`
//! (the copy needs `RONGO_TEST_HOST`, see `real_mongo.rs`).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use rongo_core::bson::{Bson, Document, RawDocumentBuf, doc, oid::ObjectId};

struct Counting;
static LIVE: AtomicUsize = AtomicUsize::new(0);
/// The most `LIVE` has been; reset to `LIVE` before measuring.
static PEAK: AtomicUsize = AtomicUsize::new(0);
/// The counters are process-wide, so the measurements run one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let live = LIVE.fetch_add(l.size(), Ordering::Relaxed) + l.size();
        PEAK.fetch_max(live, Ordering::Relaxed);
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        LIVE.fetch_sub(l.size(), Ordering::Relaxed);
        unsafe { System.dealloc(p, l) }
    }
}

#[global_allocator]
static A: Counting = Counting;

fn sample(i: i32) -> Document {
    doc! {
        "_id": ObjectId::new(),
        "ref": format!("BET{i:08}"),
        "username": format!("player{:04}", i % 800),
        "provider": "PGSOFT",
        "amount": 123.5,
        "payout": 0.0,
        "status": "settled",
        "round": 1_234_567_i64,
        "createdAt": rongo_core::bson::DateTime::now(),
        "meta": { "ip": "10.0.1.2", "ua": "Mozilla/5.0", "tags": ["a", "b"] },
    }
}

#[test]
#[ignore = "measurement; run explicitly"]
fn documents_versus_raw_bson() {
    let _one_at_a_time = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let n = 50_000;
    let before = LIVE.load(Ordering::Relaxed);
    let docs: Vec<Bson> = (0..n).map(|i| Bson::Document(sample(i))).collect();
    let as_docs = LIVE.load(Ordering::Relaxed) - before;

    let before = LIVE.load(Ordering::Relaxed);
    let raw: Vec<RawDocumentBuf> = (0..n).map(|i| RawDocumentBuf::try_from(&sample(i)).unwrap()).collect();
    let as_raw = LIVE.load(Ordering::Relaxed) - before;

    eprintln!("{n} documents: bson::Document {:.1} MB, raw BSON {:.1} MB ({:.1}x)", as_docs as f64 / 1e6, as_raw as f64 / 1e6, as_docs as f64 / as_raw as f64);
    drop((docs, raw));
}

/// A game catalogue document: names and images in ten languages, the shape
/// that makes JSON views of a few thousand documents run to millions of lines.
fn game(i: i32) -> Document {
    let langs = |v: &str| {
        let mut d = Document::new();
        for l in ["en", "th", "cn", "la", "id", "vn", "my", "mm", "kh", "tw"] {
            d.insert(l, if l == "en" { v.to_string() } else { String::new() });
        }
        d
    };
    let image = |style: &str| doc! { "style": style, "vertical": langs(&format!("https://cdn.example/img/game/DGM/L/DGM_{i}.webp")), "horizontal": langs(""), "banner": langs(""), "square": langs("") };
    doc! {
        "_id": ObjectId::new(), "provider": "DGM",
        "dataForProvider": { "gameID": format!("{i}"), "gameCode": "", "providerGameID": "", "gameUrl": "" },
        "gameName": langs(&format!("Game {i}")), "gameStatus": "ACTIVE", "gameCategory": "SLOT", "gameType": ["SLOT"],
        "image": [image("COMMON"), image("COMMON_OG"), image("SQUARE")],
        "updateTime": rongo_core::bson::DateTime::now(), "updateBy": "system", "recommend": false, "showInGameList": true,
        "loseStatus": "NORMAL", "position": i, "brand": "DGM", "group": "DGM",
        "info": { "rtp": rongo_core::bson::Decimal128::from_bytes([0; 16]), "online": 0_i64 },
    }
}

#[test]
#[ignore = "measurement; run explicitly"]
fn json_view_text_for_game_documents() {
    use rongo_core::format::{Style, value_to_shell_pretty};
    let _one_at_a_time = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let n = 9_602;
    let docs: Vec<Bson> = (0..n).map(|i| Bson::Document(game(i))).collect();
    let before = LIVE.load(Ordering::Relaxed);
    let raw: Vec<RawDocumentBuf> = (0..n).map(|i| RawDocumentBuf::try_from(&game(i)).unwrap()).collect();
    let as_raw = LIVE.load(Ordering::Relaxed) - before;
    let before = LIVE.load(Ordering::Relaxed);
    let parsed: Vec<Bson> = (0..n).map(|i| Bson::Document(game(i))).collect();
    let as_docs = LIVE.load(Ordering::Relaxed) - before;

    let t = std::time::Instant::now();
    let before = LIVE.load(Ordering::Relaxed);
    let mut lines: Vec<String> = Vec::new();
    for (i, v) in docs.iter().enumerate() {
        lines.push(format!("// {}", i + 1));
        lines.extend(value_to_shell_pretty(v, Style::Display).lines().map(str::to_string));
    }
    let as_lines = LIVE.load(Ordering::Relaxed) - before;
    let lines_ms = t.elapsed().as_secs_f64() * 1000.0;

    let t = std::time::Instant::now();
    let before = LIVE.load(Ordering::Relaxed);
    let mut text = String::new();
    let mut starts: Vec<u32> = Vec::new();
    for (i, v) in docs.iter().enumerate() {
        for l in std::iter::once(format!("// {}", i + 1)).chain(value_to_shell_pretty(v, Style::Display).lines().map(str::to_string)) {
            starts.push(text.len() as u32);
            text.push_str(&l);
        }
    }
    text.shrink_to_fit();
    starts.shrink_to_fit();
    let as_buffer = LIVE.load(Ordering::Relaxed) - before;
    let buffer_ms = t.elapsed().as_secs_f64() * 1000.0;

    let mb = |b: usize| b as f64 / 1e6;
    eprintln!("{n} game documents, {} JSON lines ({:.1} MB of text)", lines.len(), mb(text.len()));
    eprintln!("  rows: bson::Document {:.1} MB, raw BSON {:.1} MB", mb(as_docs), mb(as_raw));
    eprintln!("  JSON view: Vec<String> {:.1} MB in {lines_ms:.0} ms, one buffer + offsets {:.1} MB in {buffer_ms:.0} ms", mb(as_lines), mb(as_buffer));
    drop((docs, raw, parsed, lines, text, starts));
}

/// Copying streams in batches: 200,000 documents of about 300 bytes (60 MB)
/// between two databases never hold more than a few batches at once.
#[test]
#[ignore = "measurement against a real server; run explicitly"]
fn copying_200k_documents_keeps_memory_bounded() {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use rongo_core::profile::{Auth, ConnectionProfile, Target};
    use rongo_core::{Command, ConnectRequest, CopyItem, CopyOptions, CoreHandle, Event, IfExists, Namespace, RequestId};

    let _one_at_a_time = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(host) = std::env::var("RONGO_TEST_HOST") else { return };
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    let core = CoreHandle::start(rt.handle().clone(), Arc::new(|| {}));
    let wait = |id: RequestId| -> Event {
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            match core.try_recv() {
                Some(r) if r.id == id && !matches!(r.event, Event::Progress { .. }) => return r.event,
                Some(_) => {}
                None => {
                    assert!(Instant::now() < deadline, "no answer for {id}");
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
    };

    let mut p = ConnectionProfile::new("memory");
    p.target = Target::Manual { hosts: vec![host], replica_set: None, direct: true };
    if let Ok(user) = std::env::var("RONGO_TEST_USER") {
        p.auth = Auth::Password { user, auth_db: "admin".into(), mechanism: None };
    }
    let conn = p.conn_id();
    let req = ConnectRequest { profile: p, password: std::env::var("RONGO_TEST_PASSWORD").ok(), ssh_secret: None };
    assert!(matches!(wait(core.send(None, Command::Connect(Box::new(req)))), Event::Connected(_)));

    let (from, to) = (format!("rongo_mem_from_{}", std::process::id()), format!("rongo_mem_to_{}", std::process::id()));
    let src = Namespace::new(&from, "big");
    let n = 200_000;
    for chunk in 0..n / 10_000 {
        let docs = (0..10_000).map(|i| doc! { "n": chunk * 10_000 + i, "pad": "x".repeat(250) }).collect();
        assert!(matches!(wait(core.send(Some(conn), Command::InsertMany { ns: src.clone(), docs })), Event::Written(_)));
    }

    let before = LIVE.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let items = vec![CopyItem { src, dst: Namespace::new(&to, "big"), if_exists: IfExists::Append(Default::default()) }];
    let started = Instant::now();
    let ev = wait(core.send(Some(conn), Command::CopyCollections { to: conn, items, options: CopyOptions::default() }));
    let growth = PEAK.load(Ordering::Relaxed).saturating_sub(before);
    eprintln!("copied {n} documents in {:.1} s; peak heap growth {:.1} MB", started.elapsed().as_secs_f64(), growth as f64 / 1e6);
    let Event::Copied(report) = ev else { panic!("{ev:?}") };
    assert_eq!((report.items[0].inserted, report.items[0].error.as_deref()), (n as u64, None));
    assert!(growth < 64_000_000, "peak heap growth {growth} bytes");

    wait(core.send(Some(conn), Command::DropDatabase { db: from }));
    wait(core.send(Some(conn), Command::DropDatabase { db: to }));
}
