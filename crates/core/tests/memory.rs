//! How much memory 50,000 documents take as `bson::Document` versus raw BSON.
//! Run with `cargo test -p rongo-core --release --test memory -- --ignored --nocapture`.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use rongo_core::bson::{Bson, Document, RawDocumentBuf, doc, oid::ObjectId};

struct Counting;
static LIVE: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        LIVE.fetch_add(l.size(), Ordering::Relaxed);
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
