#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use super::*;
use crate::testkit::{TempDir, files, ns};

fn key(b: u8) -> HistoryKey {
    HistoryKey::new([b; 32])
}

fn store(t: &TempDir) -> Store {
    Store::new(t.root(), key(7))
}

fn raw(d: Document) -> RawDocumentBuf {
    RawDocumentBuf::try_from(&d).unwrap()
}

fn orders(n: i32) -> Vec<RawDocumentBuf> {
    (0..n).map(|i| raw(doc! { "_id": i, "note": format!("order {i}") })).collect()
}

fn record_one(s: &Store, p: Uuid, ns: &Namespace, kind: Kind, at: DateTime, docs: &[RawDocumentBuf]) -> EntryId {
    let mut w = s.begin_at(p, "local", ns, kind, doc! {}, at).unwrap();
    w.save(docs).unwrap();
    w.finish(false).unwrap()
}

#[test]
fn writes_and_reads_an_entry() {
    let t = TempDir::new();
    let s = store(&t);
    let p = Uuid::new_v4();
    let docs = orders(3);
    let mut w = s.begin(p, "Local dev", &ns(), Kind::Delete, doc! { "filter": { "status": "old" } }).unwrap();
    w.save(&docs).unwrap();
    let id = w.finish(false).unwrap();

    let list = s.list(p, &ns()).unwrap();
    assert_eq!(list.len(), 1);
    let h = &list[0];
    assert_eq!(h.id, id);
    assert_eq!((h.kind, h.count, h.complete, h.restored, h.conn_name.as_str()), (Kind::Delete, 3, true, false, "Local dev"));
    assert!(h.bytes > 0);
    assert!(h.id.file.ends_with("-delete.rhist"), "{}", h.id.file);
    let read: Vec<Saved> = s.read(&id).unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(read.len(), 3);
    for (r, d) in read.iter().zip(&docs) {
        assert_eq!(r.before.as_bytes(), d.as_bytes());
        assert_eq!(r.after_hash, None);
    }
}

#[test]
fn file_names_carry_time_and_kind_and_stay_unique() {
    let t = TempDir::new();
    let s = store(&t);
    let p = Uuid::new_v4();
    let at = DateTime::from_millis(1_794_596_340_123); // 2026-11-13T18:59:00.123Z
    let a = record_one(&s, p, &ns(), Kind::Delete, at, &orders(1));
    let b = record_one(&s, p, &ns(), Kind::Delete, at, &orders(1));
    let c = record_one(&s, p, &ns(), Kind::Delete, at, &orders(1));
    assert_eq!(a.file, "20261113T185900123Z-delete.rhist");
    assert_ne!(b.file, a.file);
    assert_ne!(c.file, b.file);
    let list = s.list(p, &ns()).unwrap();
    assert_eq!(list.len(), 3);
    // Newest first: later begins in the same millisecond come first.
    assert_eq!(list.iter().map(|h| h.id.clone()).collect::<Vec<_>>(), vec![c, b, a]);
    assert!(list.iter().all(|h| h.at == at));
}

#[test]
fn update_entries_keep_after_hashes() {
    let t = TempDir::new();
    let s = store(&t);
    let p = Uuid::new_v4();
    let docs = orders(2);
    let mut w = s.begin(p, "c", &ns(), Kind::Update, doc! {}).unwrap();
    w.save(&docs).unwrap();
    w.after(&[(Bson::Int32(0), 111), (Bson::Int32(1), 222)]).unwrap();
    let id = w.finish(false).unwrap();
    let read: Vec<Saved> = s.read(&id).unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(read.iter().map(|r| r.after_hash).collect::<Vec<_>>(), vec![Some(111), Some(222)]);
    assert_eq!(read[1].before.as_bytes(), docs[1].as_bytes());
    assert_eq!(s.list(p, &ns()).unwrap()[0].kind, Kind::Update);
}

#[test]
fn update_batches_pair_each_before_image_with_its_hash() {
    let t = TempDir::new();
    let s = store(&t);
    let p = Uuid::new_v4();
    let docs = orders(5);
    let mut w = s.begin(p, "c", &ns(), Kind::Update, doc! {}).unwrap();
    w.save(&docs[0..2]).unwrap();
    // The results may come back in another order.
    w.after(&[(Bson::Int32(1), 11), (Bson::Int32(0), 10)]).unwrap();
    w.save(&docs[2..4]).unwrap();
    w.after(&[(Bson::Int32(2), 12), (Bson::Int32(3), 13)]).unwrap();
    // A batch whose results were never written (cut short) still reads.
    w.save(&docs[4..5]).unwrap();
    let id = w.finish(true).unwrap();
    let read: Vec<Saved> = s.read(&id).unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(read.iter().map(|r| r.after_hash).collect::<Vec<_>>(), vec![Some(10), Some(11), Some(12), Some(13), None]);
    for (r, d) in read.iter().zip(&docs) {
        assert_eq!(r.before.as_bytes(), d.as_bytes());
    }
    let h = &s.list(p, &ns()).unwrap()[0];
    assert_eq!((h.count, h.complete), (5, true));
}

#[test]
fn deletes_refuse_after_hashes() {
    let t = TempDir::new();
    let s = store(&t);
    let mut w = s.begin(Uuid::new_v4(), "c", &ns(), Kind::Delete, doc! {}).unwrap();
    assert!(w.after(&[(Bson::Int32(0), 1)]).is_err());
}

#[test]
fn an_entry_cut_short_still_lists_and_reads() {
    let t = TempDir::new();
    let s = store(&t);
    let p = Uuid::new_v4();
    let docs = orders(2);
    let id = {
        let mut w = s.begin(p, "c", &ns(), Kind::Delete, doc! {}).unwrap();
        w.save(&docs).unwrap();
        w.id.clone()
        // dropped without finish
    };
    let list = s.list(p, &ns()).unwrap();
    assert_eq!(list.len(), 1);
    assert!(!list[0].complete);
    assert_eq!(list[0].count, 2);
    assert_eq!(list[0].conn_name, "c");
    let read: Vec<Saved> = s.read(&id).unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(read.len(), 2);
}

#[test]
fn a_torn_last_record_is_ignored() {
    let t = TempDir::new();
    let s = store(&t);
    let p = Uuid::new_v4();
    let id = record_one(&s, p, &ns(), Kind::Delete, DateTime::now(), &orders(3));
    let path = s.entry_path(&id).unwrap();
    let len = fs::metadata(&path).unwrap().len();
    // Cut the end record in half.
    OpenOptions::new().write(true).open(&path).unwrap().set_len(len - 20).unwrap();
    let h = &s.list(p, &ns()).unwrap()[0];
    assert_eq!((h.count, h.complete), (3, false));
    assert_eq!(s.read(&id).unwrap().count(), 3);
}

#[test]
fn abandon_removes_the_file_and_empty_entries_leave_none() {
    let t = TempDir::new();
    let s = store(&t);
    let p = Uuid::new_v4();
    let w = s.begin(p, "c", &ns(), Kind::Delete, doc! {}).unwrap();
    let id = w.finish(false).unwrap();
    assert!(s.list(p, &ns()).unwrap().is_empty());
    assert!(!s.entry_path(&id).unwrap().exists());

    let mut w = s.begin(p, "c", &ns(), Kind::Delete, doc! {}).unwrap();
    w.save(&orders(1)).unwrap();
    assert_eq!(s.list(p, &ns()).unwrap().len(), 1);
    w.abandon();
    assert!(s.list(p, &ns()).unwrap().is_empty());
}

#[test]
fn empty_entries_leave_no_file() {
    let t = TempDir::new();
    let s = store(&t);
    let p = Uuid::new_v4();
    let w = s.begin(p, "c", &ns(), Kind::Update, doc! {}).unwrap();
    w.finish(false).unwrap();
    assert!(s.list(p, &ns()).unwrap().is_empty());
    assert!(files(&t.root()).is_empty());
}

#[test]
fn every_save_is_on_disk_before_it_returns() {
    let t = TempDir::new();
    let s = store(&t);
    let mut w = s.begin(Uuid::new_v4(), "c", &ns(), Kind::Delete, doc! {}).unwrap();
    let path = w.path.clone();
    let before = fs::metadata(&path).unwrap().len();
    w.save(&orders(2)).unwrap();
    let after = fs::metadata(&path).unwrap().len();
    assert!(after > before);
    assert_eq!(after, w.written);
}

#[test]
fn unsafe_names_stay_inside_the_root() {
    let t = TempDir::new();
    let s = store(&t);
    let p = Uuid::new_v4();
    let long = "n".repeat(120);
    let spaces = vec![
        Namespace::new("../x", "a/b"),
        Namespace::new(".", ".."),
        Namespace::new("ภาษาไทย", "ลูกค้า"),
        Namespace::new(long.clone(), long.clone()),
        Namespace::new("100%", "a%2Fb"),
        Namespace::new("Users", "users"),
    ];
    for n in &spaces {
        record_one(&s, p, n, Kind::Delete, DateTime::now(), &orders(1));
    }
    let all_files = files(&t.root());
    assert_eq!(all_files.len(), spaces.len());
    let root = t.root();
    for f in &all_files {
        assert!(f.starts_with(&root), "{f:?}");
        let rel = f.strip_prefix(&root).unwrap();
        assert_eq!(rel.components().count(), 4, "{rel:?}");
        assert!(rel.components().all(|c| matches!(c, std::path::Component::Normal(_))));
    }
    let dirs: HashSet<&Path> = all_files.iter().map(|f| f.parent().unwrap()).collect();
    assert_eq!(dirs.len(), spaces.len(), "every namespace has its own directory");
    // Nothing was written next to the root either.
    assert_eq!(fs::read_dir(&t.0).unwrap().count(), 1);

    let mut got: Vec<(Namespace, u64)> = s.all().unwrap().into_iter().map(|(pr, n, entries, _)| {
        assert_eq!(pr, p);
        (n, entries)
    }).collect();
    let mut want: Vec<(Namespace, u64)> = spaces.iter().cloned().map(|n| (n, 1)).collect();
    got.sort_by(|a, b| (&a.0.db, &a.0.coll).cmp(&(&b.0.db, &b.0.coll)));
    want.sort_by(|a, b| (&a.0.db, &a.0.coll).cmp(&(&b.0.db, &b.0.coll)));
    assert_eq!(got, want);
    for n in &spaces {
        assert_eq!(s.list(p, n).unwrap().len(), 1);
    }
}

#[test]
fn empty_names_and_ids_outside_their_directory_are_refused() {
    let t = TempDir::new();
    let s = store(&t);
    let p = Uuid::new_v4();
    let err = s.begin(p, "c", &Namespace::new("db", ""), Kind::Delete, doc! {}).err().unwrap();
    assert!(matches!(err, CoreError::History(_)), "{err:?}");
    assert!(err.to_string().starts_with("Collection History: "), "{err}");
    // Only the file names this module writes are accepted, so nothing can leave the directory.
    for file in ["../../../etc/passwd", "x.rhist", "20261113T185900123Z-delete.rhist/../..", "20261113T185900123Z-delete-0.rhist", "20261113T185900123Z-delete-+1.rhist", "20261113T185900123Z-delete.rhist.restored"] {
        let bad = EntryId { profile: p, ns: ns(), file: file.into() };
        assert!(s.read(&bad).is_err(), "{file}");
        assert!(s.remove(&bad).is_err(), "{file}");
        assert!(s.mark_restored(&bad).is_err(), "{file}");
    }
}

#[test]
fn names_round_trip() {
    for s in ["a", "orders", "Orders", "a.b", "a/b", "..", "100%", "%41", "ภาษาไทย", "tab\t", "a b", "x-y_z"] {
        let (e, long) = encode_name(s).unwrap();
        assert!(!long);
        assert!(e.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-' | b'%' | b'A'..=b'F')), "{e}");
        assert_eq!(decode_name(&e).as_deref(), Some(s), "{e}");
    }
    assert_eq!(decode_name("a.b"), None);
    assert_eq!(decode_name("%zz"), None);
    assert_eq!(decode_name("%4"), None);
    // Only exactly what `encode_name` writes is a name.
    for non_canonical in ["%61", "%2e", "%2E%2e", "%5a", "%41%", "", &"a".repeat(201)] {
        assert_eq!(decode_name(non_canonical), None, "{non_canonical}");
    }
    assert_eq!(decode_name("%2E").as_deref(), Some("."));
}

/// Windows refuses a folder named like a device (`con`, `aux`, `com1`, also
/// `con.x`): there the first character is written as `%XX` (so the name
/// still reads back); on macOS nothing changes, history already written
/// keeps its folders.
#[test]
fn windows_device_names_are_legal_folder_names() {
    let reserved = |folder: &str| {
        let stem = folder.split('.').next().unwrap().to_ascii_lowercase();
        ["con", "prn", "aux", "nul"].contains(&stem.as_str()) || ["com", "lpt"].iter().any(|p| stem.strip_prefix(p).is_some_and(|n| n.len() == 1 && n != "0" && n.bytes().all(|b| b.is_ascii_digit())))
    };
    let names = ["con", "prn", "aux", "nul", "com1", "com9", "lpt1", "lpt9", "con.txt", "aux.x.y", "nul.", "COM1", "Con.Txt"];
    for n in names {
        let (win, long) = encode_name_for(n, true).unwrap();
        assert!(!long && !reserved(&win), "{n} -> {win}");
        assert_eq!(decode_name_for(&win, true).as_deref(), Some(n), "{n} -> {win}");
        // macOS: the same folders as before.
        let (mac, _) = encode_name_for(n, false).unwrap();
        assert_eq!(decode_name_for(&mac, false).as_deref(), Some(n), "{n} -> {mac}");
    }
    for (n, win, mac) in [("con", "%63on", "con"), ("aux", "%61ux", "aux"), ("lpt3", "%6Cpt3", "lpt3"), ("con.txt", "%63on%2Etxt", "con%2Etxt"), ("nul.", "%6Eul%2E", "nul%2E")] {
        assert_eq!((encode_name_for(n, true).unwrap().0, encode_name_for(n, false).unwrap().0), (win.to_string(), mac.to_string()), "{n}");
    }
    // Only exactly what each mode writes is a name; other names are as before.
    assert_eq!(decode_name_for("con", true), None);
    assert_eq!(decode_name_for("%63on", false), None);
    for n in ["com0", "com10", "lpt", "console", "cona", "a.con", "con-1", "con_x", "Orders", "x"] {
        assert_eq!(encode_name_for(n, true).unwrap(), encode_name_for(n, false).unwrap(), "{n}");
    }
    // The running OS's pair is the one `encode_name` uses.
    assert_eq!(encode_name("aux").unwrap().0, encode_name_for("aux", cfg!(windows)).unwrap().0);
    assert_eq!(decode_name("a").as_deref(), Some("a"));
}

#[test]
fn long_name_directories_use_a_pinned_sha256_suffix() {
    let (dir, long) = encode_name(&"a".repeat(250)).unwrap();
    assert!(long);
    assert_eq!(dir, format!("{}~3f3e35e0a775d9b1d5ec2eccca06381c", "a".repeat(200)));
    // A multi-byte name is never cut inside a %XX.
    let (dir, _) = encode_name(&"ก".repeat(80)).unwrap();
    let (prefix, hash) = dir.split_once('~').unwrap();
    assert_eq!(hash.len(), 32);
    assert!(prefix.len() <= 200 && prefix.len() % 3 == 0 && decode_name(prefix).is_some(), "{prefix}");
    assert_eq!(decode_name(&dir), None, "a shortened name is not a plain one");
}

#[test]
fn key_hex_accepts_only_hex_digits() {
    let k = HistoryKey::new([0xAB; 32]);
    assert_eq!(HistoryKey::from_hex(&k.to_hex()), Some(k.clone()));
    assert_eq!(HistoryKey::from_hex(&k.to_hex().to_uppercase()), Some(k));
    assert_eq!(HistoryKey::from_hex(&format!("+f{}", "0".repeat(62))), None);
    assert_eq!(HistoryKey::from_hex(&format!("-f{}", "0".repeat(62))), None);
    assert_eq!(HistoryKey::from_hex(&"0".repeat(63)), None);
    assert_eq!(HistoryKey::from_hex(&"0".repeat(65)), None);
    assert_eq!(HistoryKey::from_hex(&format!("{}g", "0".repeat(63))), None);
}

#[cfg(unix)]
#[test]
fn files_are_owner_only() {
    let t = TempDir::new();
    let s = store(&t);
    let p = Uuid::new_v4();
    let id = record_one(&s, p, &ns(), Kind::Delete, DateTime::now(), &orders(1));
    s.mark_restored(&id).unwrap();
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    let file = s.entry_path(&id).unwrap();
    assert_eq!(mode(&file), 0o600);
    assert_eq!(mode(&restored_path(&file)), 0o600);
    let mut dir = file.parent().unwrap().to_path_buf();
    while dir != t.0 {
        assert_eq!(mode(&dir), 0o700, "{dir:?}");
        dir.pop();
    }
}

#[test]
fn records_are_encrypted() {
    let t = TempDir::new();
    let s = store(&t);
    let p = Uuid::new_v4();
    let docs: Vec<RawDocumentBuf> = (0..3).map(|i| raw(doc! { "_id": i, "secret": "distinctive-marker-4711" })).collect();
    let id = record_one(&s, p, &Namespace::new("shop", "orders"), Kind::Delete, DateTime::now(), &docs);
    let path = s.entry_path(&id).unwrap();
    let bytes = fs::read(&path).unwrap();
    assert!(bytes.starts_with(b"RONGOHIST1\n"));
    let has = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).any(|w| w == needle);
    assert!(!has(&bytes, b"distinctive-marker-4711"));
    assert!(!has(&bytes, b"secret"));
    assert!(!has(&bytes, b"local"), "the connection name is sealed too");

    // Another key reads nothing, and says so without failing.
    let other = Store::new(t.root(), key(8));
    let list = other.list(p, &ns_of("shop", "orders")).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!((list[0].count, list[0].complete, list[0].conn_name.as_str(), list[0].kind), (0, false, "", Kind::Delete));
    assert_eq!(list[0].id, id);
    assert_eq!(other.read(&id).unwrap().count(), 0);

    // A flipped byte in the second before-image: the reader stops after the first.
    let mut starts = Vec::new();
    let mut off = MAGIC.len();
    while off + 4 <= bytes.len() {
        starts.push(off);
        off += 4 + u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap()) as usize;
    }
    assert_eq!(starts.len(), 6, "header, batch marker, 3 documents, end");
    let mut damaged = bytes.clone();
    damaged[starts[3] + 4 + NONCE_LEN + 3] ^= 0x01;
    fs::write(&path, &damaged).unwrap();
    let read: Vec<Saved> = s.read(&id).unwrap().collect::<Result<_, _>>().unwrap();
    assert_eq!(read.len(), 1);
    assert_eq!(read[0].before.as_bytes(), docs[0].as_bytes());
    // Listing reads only the header and the last record, so a finished entry
    // is still listed by its end record; reading it is what stops at the damage.
    let h = &s.list(p, &ns_of("shop", "orders")).unwrap()[0];
    assert_eq!((h.count, h.complete), (3, true));

    // Dropping a whole record (here the batch marker) is noticed too (positions are authenticated).
    let mut dropped = bytes[..starts[1]].to_vec();
    dropped.extend_from_slice(&bytes[starts[2]..]);
    fs::write(&path, &dropped).unwrap();
    assert_eq!(s.read(&id).unwrap().count(), 0);
    // The end record's position no longer matches, so listing falls back to a full scan.
    let h = &s.list(p, &ns_of("shop", "orders")).unwrap()[0];
    assert_eq!((h.count, h.complete), (0, false));
}

fn ns_of(db: &str, coll: &str) -> Namespace {
    Namespace::new(db, coll)
}

#[test]
fn a_damaged_length_does_not_allocate_or_panic() {
    let t = TempDir::new();
    let s = store(&t);
    let p = Uuid::new_v4();
    let id = record_one(&s, p, &ns(), Kind::Delete, DateTime::now(), &orders(2));
    let path = s.entry_path(&id).unwrap();
    let mut bytes = fs::read(&path).unwrap();
    // Second record's length says 4 GB.
    let hdr_len = u32::from_le_bytes(bytes[MAGIC.len()..MAGIC.len() + 4].try_into().unwrap()) as usize;
    let at = MAGIC.len() + 4 + hdr_len;
    bytes[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
    fs::write(&path, &bytes).unwrap();
    assert_eq!(s.read(&id).unwrap().count(), 0);
    // Not a history file at all.
    fs::write(&path, b"hello").unwrap();
    assert_eq!(s.read(&id).unwrap().count(), 0);
    assert_eq!(s.list(p, &ns()).unwrap()[0].count, 0);
}

#[test]
fn purge_by_age_and_size_spares_protected() {
    let t = TempDir::new();
    let s = store(&t);
    let p = Uuid::new_v4();
    let now = DateTime::now().timestamp_millis();
    let ago = |days: i64| DateTime::from_millis(now - days * DAY_MS);
    let docs = orders(2);
    let old = record_one(&s, p, &ns(), Kind::Delete, ago(40), &docs);
    let mid = record_one(&s, p, &ns(), Kind::Delete, ago(10), &docs);
    let new = record_one(&s, p, &ns(), Kind::Delete, ago(1), &docs);
    let one = s.list(p, &ns()).unwrap()[0].bytes;
    assert_eq!(s.usage().unwrap(), 3 * one);

    // The forged time is also the one the listing shows.
    let list = s.list(p, &ns()).unwrap();
    assert_eq!(list.iter().map(|h| h.id.clone()).collect::<Vec<_>>(), vec![new.clone(), mid.clone(), old.clone()]);
    assert_eq!(list[2].at, ago(40));

    // Protected entries survive even when too old.
    assert_eq!(s.purge(30, u64::MAX, std::slice::from_ref(&old)).unwrap(), 0);
    assert_eq!(s.list(p, &ns()).unwrap().len(), 3);

    assert_eq!(s.purge(30, u64::MAX, &[]).unwrap(), one);
    let ids: Vec<EntryId> = s.list(p, &ns()).unwrap().into_iter().map(|h| h.id).collect();
    assert_eq!(ids, vec![new.clone(), mid.clone()]);

    // Below the size of two: the oldest unprotected one goes, the protected newest stays.
    assert_eq!(s.purge(30, 2 * one - 1, std::slice::from_ref(&new)).unwrap(), one);
    let ids: Vec<EntryId> = s.list(p, &ns()).unwrap().into_iter().map(|h| h.id).collect();
    assert_eq!(ids, vec![new.clone()]);

    // A limit nothing can meet removes only what is not protected.
    assert_eq!(s.purge(30, 0, std::slice::from_ref(&new)).unwrap(), 0);
    assert_eq!(s.list(p, &ns()).unwrap().len(), 1);
    assert_eq!(s.purge(30, 0, &[]).unwrap(), one);
    assert!(s.list(p, &ns()).unwrap().is_empty());
    assert!(s.all().unwrap().is_empty());
}

/// An entry another operation is still writing is never removed: not
/// by retention at the end of an operation (which protects only its own
/// entry), nor by Remove, Empty, Clear All or deleting its profile, and
/// a rename does not move it from under its writer. Once finished, it
/// goes like any other.
#[test]
fn an_entry_being_written_is_never_removed_or_moved() {
    let t = TempDir::new();
    let s = store(&t);
    let (p, q) = (Uuid::new_v4(), Uuid::new_v4());
    let now = DateTime::now().timestamp_millis();
    let ago = |days: i64| DateTime::from_millis(now - days * DAY_MS);
    // B is being written: older than A, and older than the age limit.
    let mut b = s.begin_at(p, "c", &ns(), Kind::Delete, doc! {}, ago(40)).unwrap();
    b.save(&orders(2)).unwrap();
    let b_path = s.entry_path(b.id()).unwrap();
    let a = record_one(&s, p, &ns(), Kind::Delete, ago(1), &orders(1));
    let still = |b: &mut EntryWriter| {
        assert!(b_path.is_file());
        b.save(&orders(1)).unwrap();
    };

    // Another operation ends: its own entry A is protected, the limits are passed.
    assert_eq!(s.purge(30, 0, std::slice::from_ref(&a)).unwrap(), 0);
    still(&mut b);
    assert_eq!(s.list(p, &ns()).unwrap().len(), 2);

    let err = s.remove(b.id()).unwrap_err();
    assert!(err.to_string().contains("still being written"), "{err}");
    still(&mut b);
    // The others go; B stays, and the answer says so.
    let err = s.empty(p, &ns(), i64::MAX).unwrap_err();
    assert!(err.to_string().contains("still being written"), "{err}");
    assert_eq!(s.list(p, &ns()).unwrap().iter().map(|h| &h.id).collect::<Vec<_>>(), [b.id()]);
    still(&mut b);
    let other = record_one(&s, q, &ns(), Kind::Delete, ago(1), &orders(1));
    assert!(s.clear().unwrap_err().to_string().contains("still being written"));
    assert!(s.list(q, &ns()).unwrap().is_empty(), "another profile's history is cleared: {other:?}");
    still(&mut b);
    assert!(s.delete_profile(p).unwrap_err().to_string().contains("still being written"));
    still(&mut b);
    // A rename refuses rather than move the file from under its writer.
    let to = Namespace::new("shop", "orders_renamed");
    assert!(s.move_collection(p, &ns(), &to).unwrap_err().to_string().contains("still being written"));
    assert!(s.list(p, &to).unwrap().is_empty());
    still(&mut b);

    let id = b.finish(false).unwrap();
    let h = &s.list(p, &ns()).unwrap()[0];
    assert_eq!((&h.id, h.count, h.complete), (&id, 8, true), "every save reached the file");
    // Finished, it is history like any other.
    s.move_collection(p, &ns(), &to).unwrap();
    assert_eq!(s.list(p, &to).unwrap().len(), 1);
    assert!(s.purge(30, u64::MAX, &[]).unwrap() > 0);
    assert!(s.list(p, &to).unwrap().is_empty());
}

/// Reset's deletion: every entry begun at or before the cutoff (its file
/// name's time) goes, whatever key sealed it, with its restored mark and
/// the folders that leaves empty. Later entries stay, and so does one
/// still being written, which the answer names; once finished it goes
/// too. Files that aren't history stay.
#[test]
fn clear_before_keeps_what_was_recorded_after() {
    let t = TempDir::new();
    let s = store(&t);
    let (p, q) = (Uuid::new_v4(), Uuid::new_v4());
    let cut: i64 = 1_794_596_340_123;
    let at = DateTime::from_millis;
    let old = record_one(&s, p, &ns(), Kind::Delete, at(cut - 5000), &orders(1));
    let mark = restored_path(&s.entry_path(&old).unwrap());
    fs::write(&mark, "").unwrap();
    record_one(&s, q, &Namespace::new("a", "b"), Kind::Update, at(cut), &orders(1));
    record_one(&Store::new(t.root(), key(9)), p, &Namespace::new("x", "y"), Kind::Delete, at(cut - 1), &orders(1));
    let new = record_one(&s, p, &ns(), Kind::Delete, at(cut + 1), &orders(2));
    let mut open = s.begin_at(p, "c", &ns(), Kind::Delete, doc! {}, at(cut - 10)).unwrap();
    open.save(&orders(1)).unwrap();
    let open_id = open.id().clone();
    fs::write(t.root().join("not-history.txt"), "x").unwrap();

    let err = s.clear_before(cut).unwrap_err();
    assert!(err.to_string().contains("still being written"), "{err}");
    let mut ids: Vec<EntryId> = s.list(p, &ns()).unwrap().into_iter().map(|h| h.id).collect();
    ids.sort_by(|a, b| a.file.cmp(&b.file));
    assert_eq!(ids, [open_id, new.clone()], "the later entry and the open one stay");
    assert!(!mark.exists(), "its restored mark went with it");
    assert!(!t.root().join(q.to_string()).exists(), "a profile left empty goes");
    assert!(s.list(p, &Namespace::new("x", "y")).unwrap().is_empty(), "another key's entry goes too");
    assert!(t.root().join("not-history.txt").is_file());

    open.finish(false).unwrap();
    s.clear_before(cut).unwrap();
    let left: Vec<(EntryId, u64)> = s.list(p, &ns()).unwrap().into_iter().map(|h| (h.id, h.count)).collect();
    assert_eq!(left, [(new, 2)]);
}

/// Retention goes on past what it can't read or remove (a folder or file
/// whose permissions keep it out): everything else is trimmed, and the
/// answer names what was left, so the caller can say so.
#[cfg(unix)]
#[test]
fn retention_goes_on_past_what_it_cannot_read_or_remove() {
    let t = TempDir::new();
    let s = store(&t);
    let (p, q) = (Uuid::new_v4(), Uuid::new_v4());
    let old = DateTime::from_millis(DateTime::now().timestamp_millis() - 40 * DAY_MS);
    let gone = record_one(&s, p, &ns(), Kind::Delete, old, &orders(1));
    let kept = record_one(&s, p, &ns(), Kind::Delete, DateTime::now(), &orders(1));
    // Another profile's collection can't be read; a third one's entry can't be removed.
    record_one(&s, q, &ns(), Kind::Delete, old, &orders(1));
    let stuck_ns = Namespace::new("shop", "stuck");
    let stuck = record_one(&s, p, &stuck_ns, Kind::Delete, old, &orders(1));
    let (closed, locked) = (s.ns_dir(q, &ns()).unwrap(), s.ns_dir(p, &stuck_ns).unwrap());
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
    let purged = s.purge(30, u64::MAX, &[]);
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o700)).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(!s.entry_path(&gone).unwrap().exists(), "the rest was trimmed all the same");
    let err = purged.unwrap_err().to_string();
    assert!(err.contains("could not") && err.contains("ermission denied"), "{err}");
    assert!(s.entry_path(&kept).unwrap().exists());
    assert!(s.entry_path(&stuck).unwrap().exists());
    assert_eq!(s.list(q, &ns()).unwrap().len(), 1, "what could not be read is left alone");
    // Readable again: trimmed as usual.
    assert!(s.purge(30, u64::MAX, &[]).unwrap() > 0);
    assert!(s.list(q, &ns()).unwrap().is_empty() && !s.entry_path(&stuck).unwrap().exists());
}

/// Retention also removes what a crash or a race leaves behind: a
/// restored mark whose entry is gone, and an entry file holding only its
/// header, once it is over a minute old and no writer has it open.
#[test]
fn retention_sweeps_orphan_marks_and_header_only_files() {
    let t = TempDir::new();
    let s = store(&t);
    let p = Uuid::new_v4();
    let backdate = |path: &Path, secs: u64| {
        let f = OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(secs)).unwrap();
    };
    // A mark whose entry was removed between the check and the mark.
    let orphan = record_one(&s, p, &ns(), Kind::Delete, DateTime::now(), &orders(1));
    s.mark_restored(&orphan).unwrap();
    fs::remove_file(s.entry_path(&orphan).unwrap()).unwrap();
    let orphan_mark = restored_path(&s.entry_path(&orphan).unwrap());
    // A restored entry keeps its mark.
    let restored = record_one(&s, p, &ns(), Kind::Delete, DateTime::now(), &orders(1));
    s.mark_restored(&restored).unwrap();
    // Header only: a crash between beginning an entry and saving its first batch.
    let crashed = s.begin(p, "c", &ns(), Kind::Delete, doc! {}).unwrap();
    let crashed_path = crashed.path.clone();
    drop(crashed);
    backdate(&crashed_path, 120);
    // Header only, but just begun, or still open in a writer.
    let fresh = s.begin(p, "c", &ns(), Kind::Update, doc! {}).unwrap();
    let fresh_path = fresh.path.clone();
    drop(fresh);
    let open = s.begin(p, "c", &ns(), Kind::Delete, doc! {}).unwrap();
    backdate(&open.path, 120);
    // An old entry with a document: not header only.
    let full = record_one(&s, p, &ns(), Kind::Delete, DateTime::now(), &orders(1));
    backdate(&s.entry_path(&full).unwrap(), 120);
    // A crash in the middle of beginning an entry: nothing, the magic
    // line only, part of the header's length, part of the header.
    let header = fs::read(&crashed_path).unwrap();
    let dir = crashed_path.parent().unwrap().to_path_buf();
    let at = parse_file_name(&full.file).unwrap().ms - 60_000;
    let torn: Vec<PathBuf> = [0, MAGIC.len(), MAGIC.len() + 2, header.len() - 10]
        .iter()
        .enumerate()
        .map(|(i, &len)| {
            let path = dir.join(file_name(at, Kind::Delete, i as u32 + 1));
            fs::write(&path, &header[..len]).unwrap();
            backdate(&path, 120);
            path
        })
        .collect();
    // A file under an entry's name that is not history at all is left to age out.
    let stranger = dir.join(file_name(at, Kind::Update, 1));
    fs::write(&stranger, b"hello").unwrap();
    backdate(&stranger, 120);

    s.purge(30, u64::MAX, &[]).unwrap();
    for (path, len) in torn.iter().zip([0, MAGIC.len(), MAGIC.len() + 2, header.len() - 10]) {
        assert!(!path.exists(), "a torn beginning of {len} bytes is gone");
    }
    assert!(stranger.exists());
    assert!(!orphan_mark.exists(), "the orphan mark is gone");
    assert!(s.header(&restored).unwrap().restored, "a mark with its entry stays");
    assert!(!crashed_path.exists(), "the header-only file over a minute old is gone");
    assert!(fresh_path.exists() && open.path.exists(), "a new one, and one being written, stay");
    assert!(s.entry_path(&full).unwrap().exists());
    let mut open = open;
    let open_id = open.id().clone();
    open.save(&orders(1)).unwrap();
    open.finish(false).unwrap();
    assert_eq!(s.header(&open_id).unwrap().count, 1, "the writer carried on");
}

/// Retention after an operation runs only when it can matter: the first
/// time in this process (a full purge, which also measures the history),
/// then when the running total passes the size limit, and for the age
/// limit at most once an hour. In between it adds the new entry's size
/// and reads nothing else.
#[test]
fn retention_after_an_operation_walks_only_when_it_can_matter() {
    let t = TempDir::new();
    let s = store(&t);
    let p = Uuid::new_v4();
    let ago = |days: i64| DateTime::from_millis(DateTime::now().timestamp_millis() - days * DAY_MS);
    let walks = || WALKS.with(|c| c.replace(0));
    let old = record_one(&s, p, &ns(), Kind::Delete, ago(40), &orders(1));
    let new = record_one(&s, p, &ns(), Kind::Delete, DateTime::now(), &orders(1));
    walks();
    assert!(s.trim(30, u64::MAX, &new, &[]).unwrap() > 0, "the first time: a full purge");
    assert_eq!(walks(), 1);
    assert!(!s.entry_path(&old).unwrap().exists());
    assert_eq!(s.running_total(), Some(s.usage().unwrap()));

    // Within the hour and under the limit: nothing is read, and an entry
    // past its age waits for the next purge.
    let old = record_one(&s, p, &ns(), Kind::Delete, ago(40), &orders(1));
    let new = record_one(&s, p, &ns(), Kind::Delete, DateTime::now(), &orders(1));
    walks();
    assert_eq!(s.trim(30, u64::MAX, &new, &[]).unwrap(), 0);
    assert_eq!(walks(), 0, "nothing walked");
    assert!(s.entry_path(&old).unwrap().exists());
    let one = s.header(&new).unwrap().bytes;
    assert_eq!(s.running_total(), Some(s.usage().unwrap() - one), "the new entry counted; the one recorded without a trim not");

    // The running total passes the limit: purged now (oldest first), never the new entry.
    let newest = record_one(&s, p, &ns(), Kind::Delete, DateTime::now(), &orders(1));
    let limit = s.running_total().unwrap() + s.header(&newest).unwrap().bytes - 1;
    walks();
    assert!(s.trim(30, limit, &newest, &[]).unwrap() > 0);
    assert_eq!(walks(), 1);
    assert!(!s.entry_path(&old).unwrap().exists() && s.entry_path(&newest).unwrap().exists());
    assert_eq!(s.running_total(), Some(s.usage().unwrap()));

    // An hour on, entries past their age go again.
    s.aged_ago(std::time::Duration::from_secs(3601));
    let old = record_one(&s, p, &ns(), Kind::Delete, ago(40), &orders(1));
    let new = record_one(&s, p, &ns(), Kind::Delete, DateTime::now(), &orders(1));
    walks();
    assert!(s.trim(30, u64::MAX, &new, &[]).unwrap() > 0);
    assert_eq!(walks(), 1);
    assert!(!s.entry_path(&old).unwrap().exists());
    // A purge called directly (start-up, new limits in Settings) counts as one too.
    let new = record_one(&s, p, &ns(), Kind::Delete, DateTime::now(), &orders(1));
    s.aged_ago(std::time::Duration::from_secs(3601));
    s.purge(30, u64::MAX, &[]).unwrap();
    walks();
    assert_eq!(s.trim(30, u64::MAX, &new, &[]).unwrap(), 0);
    assert_eq!(walks(), 0);
}

/// File names are read by position, without chrono, and accept exactly
/// what the chrono reading did (and `file_name` writes).
#[test]
fn file_names_read_by_position_accept_what_they_did() {
    fn by_chrono(name: &str) -> Option<(i64, Kind, u32)> {
        let stem = name.strip_suffix(".rhist")?;
        let (ts, rest) = stem.split_once('-')?;
        if ts.len() != 19 || !ts.is_ascii() || !ts.ends_with('Z') {
            return None;
        }
        let secs = chrono::NaiveDateTime::parse_from_str(&ts[..15], "%Y%m%dT%H%M%S").ok()?.and_utc().timestamp();
        let millis: i64 = ts[15..18].parse().ok()?;
        let (kind, n) = match rest.split_once('-') {
            Some((k, n)) => (k, n.parse().ok()?),
            None => (rest, 0),
        };
        let (ms, kind) = (secs * 1000 + millis, Kind::from_name(kind)?);
        (file_name(ms, kind, n) == name).then_some((ms, kind, n))
    }
    let mut names: Vec<String> = Vec::new();
    let day = DAY_MS;
    let mut ms = -2 * 365 * day;
    while ms < 60 * 365 * day {
        for (k, n) in [(Kind::Delete, 0), (Kind::Update, 2), (Kind::Delete, 4_294_967_295)] {
            names.push(file_name(ms, k, n));
        }
        ms += 7 * day + 3_723_456;
    }
    // Leap days and the ends of months and years, before 1970 too.
    for date in ["19000228", "19000229", "19000301", "19601231", "19691231", "19700101", "20000229", "20230229", "20240229", "20241231", "20250101", "20240431", "20240430", "20241301", "20240001", "20240100", "00000101", "99991231", "21000229"] {
        for time in ["T000000000Z", "T235959999Z", "T240000000Z", "T236000000Z", "T235960000Z", "T12345601Z", "t000000000Z", "T000000000z"] {
            names.push(format!("{date}{time}-delete.rhist"));
        }
    }
    let base = "20261113T185900123Z-delete";
    for tail in ["", "-0", "-1", "-01", "-+1", "--1", "-1-1", "-4294967295", "-4294967296", "-a", "-"] {
        names.push(format!("{base}{tail}.rhist"));
    }
    for odd in ["20261113T185900123Z-Delete.rhist", "20261113T185900123Z-update.rhist", "20261113T185900123Z-insert.rhist", "20261113T185900123Z_delete.rhist", "20261113T185900123Z-delete.rhist.restored", "20261113T185900123Z-delete", "2026111T185900123Z-delete.rhist", "+2026111T185900123Z-delete.rhist", "20261113T185900123-delete.rhist", "20261113T1859001234Z-delete.rhist", "２0261113T185900123Z-delete.rhist", ".rhist", "", "-delete.rhist"] {
        names.push(odd.to_string());
    }
    // Every one-byte change of a valid name.
    let valid = file_name(1_794_596_340_123, Kind::Update, 12);
    for i in 0..valid.len() {
        for c in [b'0', b'1', b'9', b'-', b'T', b'Z', b'x', b'+', b' '] {
            let mut b = valid.clone().into_bytes();
            b[i] = c;
            names.push(String::from_utf8(b).unwrap());
        }
    }
    let mut accepted = 0;
    for n in &names {
        let new = parse_file_name(n).map(|f| (f.ms, f.kind, f.n));
        assert_eq!(new, by_chrono(n), "{n}");
        accepted += usize::from(new.is_some());
    }
    assert!(accepted > 9000 && accepted < names.len(), "{accepted} of {}", names.len());
}

/// A purge reads and removes files without holding retention's lock:
/// another operation's trim on the same history goes ahead meanwhile,
/// and its entry is counted when the purge settles the total.
#[test]
fn a_purge_under_way_does_not_hold_up_a_trim() {
    let t = TempDir::new();
    let s = store(&t);
    let p = Uuid::new_v4();
    record_one(&s, p, &ns(), Kind::Delete, DateTime::now(), &orders(1));
    s.purge(30, u64::MAX, &[]).unwrap();
    let (entered_tx, entered) = std::sync::mpsc::channel();
    let (go, go_rx) = std::sync::mpsc::channel::<()>();
    PURGE_PAUSE.lock().unwrap().insert(s.root.clone(), Box::new(move || {
        entered_tx.send(()).unwrap();
        go_rx.recv().unwrap();
    }));
    let purging = s.clone();
    let purger = std::thread::spawn(move || purging.purge(30, u64::MAX, &[]));
    entered.recv_timeout(std::time::Duration::from_secs(10)).expect("the purge began");
    let new = record_one(&s, p, &ns(), Kind::Delete, DateTime::now(), &orders(1));
    let (done_tx, done) = std::sync::mpsc::channel();
    let (trimming, id) = (s.clone(), new.clone());
    let trimmer = std::thread::spawn(move || done_tx.send(trimming.trim(30, u64::MAX, &id, &[]).is_ok()).unwrap());
    let trimmed = done.recv_timeout(std::time::Duration::from_secs(2));
    go.send(()).unwrap();
    purger.join().unwrap().unwrap();
    trimmer.join().unwrap();
    assert_eq!(trimmed, Ok(true), "the trim went ahead while the purge was under way");
    assert_eq!(s.running_total(), Some(s.usage().unwrap()), "both entries counted");
}

/// Finished, abandoned or dropped, a writer leaves nothing open behind;
/// nor does one whose file could not be made.
#[test]
fn writers_leave_the_open_set_when_they_end() {
    let t = TempDir::new();
    let s = store(&t);
    let p = Uuid::new_v4();
    let open_here = || open_writers().iter().filter(|o| o.starts_with(&t.0)).count();
    let w = s.begin(p, "c", &ns(), Kind::Delete, doc! {}).unwrap();
    let w2 = s.begin(p, "c", &ns(), Kind::Update, doc! {}).unwrap();
    let mut w3 = s.begin(p, "c", &ns(), Kind::Update, doc! {}).unwrap();
    w3.save(&orders(1)).unwrap();
    assert_eq!(open_here(), 3);
    w.abandon();
    w2.finish(false).unwrap();
    drop(w3);
    assert_eq!(open_here(), 0);
    // No file can be made in the collection's folder: nothing is left open.
    #[cfg(unix)]
    {
        let blocked = Namespace::new("shop", "blocked");
        let dir = s.ns_dir(p, &blocked).unwrap();
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).unwrap();
        let refused = s.begin(p, "c", &blocked, Kind::Delete, doc! {});
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(refused.is_err());
        assert_eq!(open_here(), 0);
    }
}

#[test]
fn purge_ages_an_entry_by_the_time_in_its_name_when_the_header_is_unreadable() {
    let t = TempDir::new();
    let s = store(&t);
    let p = Uuid::new_v4();
    let at = DateTime::from_millis(DateTime::now().timestamp_millis() - 45 * DAY_MS);
    let id = record_one(&s, p, &ns(), Kind::Delete, at, &orders(1));
    let other = Store::new(t.root(), key(9));
    assert!(other.purge(30, u64::MAX, &[]).unwrap() > 0);
    assert!(s.list(p, &ns()).unwrap().is_empty());
    assert!(!s.entry_path(&id).unwrap().exists());
}

#[test]
fn restored_marker_move_and_delete() {
    let t = TempDir::new();
    let s = store(&t);
    let (p, q) = (Uuid::new_v4(), Uuid::new_v4());
    let id = record_one(&s, p, &ns(), Kind::Delete, DateTime::now(), &orders(1));
    record_one(&s, q, &ns(), Kind::Update, DateTime::now(), &orders(2));

    assert!(!s.list(p, &ns()).unwrap()[0].restored);
    s.mark_restored(&id).unwrap();
    assert!(s.list(p, &ns()).unwrap()[0].restored);
    // The marker leaves the entry itself alone.
    assert_eq!(s.list(p, &ns()).unwrap()[0].count, 1);
    assert_eq!(s.usage().unwrap(), s.all().unwrap().iter().map(|a| a.3).sum::<u64>());

    let to = Namespace::new("shop", "orders_renamed");
    s.move_collection(p, &ns(), &to).unwrap();
    assert!(s.list(p, &ns()).unwrap().is_empty());
    let moved = s.list(p, &to).unwrap();
    assert_eq!(moved.len(), 1);
    assert!(moved[0].restored);
    assert_eq!(s.read(&moved[0].id).unwrap().count(), 1);
    assert_eq!(s.list(q, &ns()).unwrap().len(), 1, "another profile is untouched");
    // Moving a collection that has no history is not an error.
    s.move_collection(p, &Namespace::new("shop", "nothing"), &Namespace::new("shop", "else")).unwrap();

    // Moving onto a collection that already has history merges, keeping both.
    let again = record_one(&s, p, &ns(), Kind::Delete, DateTime::from_millis(moved[0].at.timestamp_millis()), &orders(1));
    s.move_collection(p, &ns(), &to).unwrap();
    let merged = s.list(p, &to).unwrap();
    assert_eq!(merged.len(), 2);
    assert_ne!(again.file, "");
    assert_eq!(merged.iter().filter(|h| h.restored).count(), 1);
    assert!(s.list(p, &ns()).unwrap().is_empty());

    s.remove(&merged[0].id).unwrap();
    assert_eq!(s.list(p, &to).unwrap().len(), 1);
    s.remove(&merged[0].id).unwrap();

    s.delete_profile(p).unwrap();
    let all = s.all().unwrap();
    assert!(all.iter().all(|a| a.0 != p));
    assert_eq!(all.len(), 1);
    assert_eq!((all[0].0, all[0].2), (q, 1));
    s.delete_profile(p).unwrap();
}

/// Settings shows the space each profile's history takes, counting every
/// entry file (also one in a directory whose name can't be read back),
/// and "Clear all" removes every profile's history but nothing else.
#[test]
fn usage_by_profile_and_clear() {
    let t = TempDir::new();
    let s = store(&t);
    let (p, q) = (Uuid::new_v4(), Uuid::new_v4());
    let long = Namespace::new("shop", "ก".repeat(80));
    record_one(&s, p, &ns(), Kind::Delete, DateTime::now(), &orders(2));
    record_one(&s, p, &long, Kind::Delete, DateTime::now(), &orders(1));
    record_one(&s, q, &ns(), Kind::Update, DateTime::now(), &orders(3));
    fs::remove_file(s.ns_dir(p, &long).unwrap().join(NAME_FILE)).unwrap();
    let by = s.usage_by_profile().unwrap();
    let mut both = vec![p, q];
    both.sort();
    assert_eq!(by.keys().copied().collect::<Vec<_>>(), both);
    assert_eq!(by.values().sum::<u64>(), s.usage().unwrap());
    let listed: u64 = s.all().unwrap().iter().filter(|a| a.0 == p).map(|a| a.3).sum();
    assert!(by[&p] > listed, "the unnamed directory counts too");

    // Not history: left alone.
    fs::write(t.root().join("notes.txt"), "mine").unwrap();
    fs::create_dir_all(t.root().join("not-a-profile")).unwrap();
    s.clear().unwrap();
    assert_eq!(s.usage().unwrap(), 0);
    assert!(s.usage_by_profile().unwrap().is_empty());
    assert!(s.all().unwrap().is_empty());
    assert!(t.root().join("notes.txt").is_file() && t.root().join("not-a-profile").is_dir());
    // Clearing again, or a root that was never made, is fine.
    s.clear().unwrap();
    Store::new(t.0.join("never"), key(7)).clear().unwrap();
}

/// Empty (and Settings' Delete) removes the entries begun at or before
/// the time the user asked, with their restored marks; one recorded after
/// (while the removal waited behind other work) stays, mark and all.
#[test]
fn empty_keeps_what_was_recorded_after_the_question() {
    let t = TempDir::new();
    let s = store(&t);
    let p = Uuid::new_v4();
    let cut: i64 = 1_794_596_340_123;
    let at = DateTime::from_millis;
    let old = record_one(&s, p, &ns(), Kind::Delete, at(cut - 5000), &orders(1));
    s.mark_restored(&old).unwrap();
    record_one(&s, p, &ns(), Kind::Update, at(cut), &orders(1));
    let new = record_one(&s, p, &ns(), Kind::Delete, at(cut + 1), &orders(2));
    s.mark_restored(&new).unwrap();
    s.empty(p, &ns(), cut).unwrap();
    let left: Vec<(EntryId, u64, bool)> = s.list(p, &ns()).unwrap().into_iter().map(|h| (h.id, h.count, h.restored)).collect();
    assert_eq!(left, [(new.clone(), 2, true)], "the later entry stays, still marked restored");
    assert!(!restored_path(&s.entry_path(&old).unwrap()).exists(), "the removed entry's mark went with it");
    s.empty(p, &ns(), cut + 1).unwrap();
    assert!(files(&t.root()).is_empty(), "nothing left once it is included");
}

#[test]
fn empty_removes_one_collection_and_removing_the_last_entry_prunes_directories() {
    let t = TempDir::new();
    let s = store(&t);
    let p = Uuid::new_v4();
    let other = Namespace::new("shop", "carts");
    let id = record_one(&s, p, &ns(), Kind::Delete, DateTime::now(), &orders(1));
    record_one(&s, p, &ns(), Kind::Update, DateTime::from_millis(1), &orders(1));
    s.mark_restored(&id).unwrap();
    record_one(&s, p, &other, Kind::Delete, DateTime::now(), &orders(1));
    s.empty(p, &ns(), i64::MAX).unwrap();
    assert!(s.list(p, &ns()).unwrap().is_empty());
    assert_eq!(s.list(p, &other).unwrap().len(), 1);
    assert_eq!(files(&t.root()).len(), 1, "no leftover marker");
    s.empty(p, &ns(), i64::MAX).unwrap();

    let id = s.list(p, &other).unwrap()[0].id.clone();
    s.remove(&id).unwrap();
    assert!(!t.root().join(p.to_string()).exists());
    assert!(t.root().exists());
}

#[test]
fn free_bytes_works_before_the_root_exists() {
    let t = TempDir::new();
    let s = store(&t);
    assert!(!t.root().exists());
    assert!(s.free_bytes().unwrap() > 0);
    assert_eq!(s.usage().unwrap(), 0);
    assert!(s.all().unwrap().is_empty());
    assert!(s.list(Uuid::new_v4(), &ns()).unwrap().is_empty());
    assert_eq!(s.purge(30, 0, &[]).unwrap(), 0);
}

#[test]
fn doc_hash_follows_the_bytes() {
    let a = raw(doc! { "_id": 1, "x": 1 });
    assert_eq!(doc_hash(&a), doc_hash(&raw(doc! { "_id": 1, "x": 1 })));
    assert_ne!(doc_hash(&a), doc_hash(&raw(doc! { "_id": 1, "x": 2 })));
}

/// The hashes are kept on disk, so they must never change: the first 8
/// bytes (little-endian) of the SHA-256 of the document's bytes.
#[test]
fn doc_hash_is_pinned() {
    // SHA-256 of 05 00 00 00 00 starts 49 e8 e3 29 75 45 c1 5a.
    assert_eq!(doc_hash(&raw(doc! {})), 6_539_584_503_434_963_017);
    // SHA-256 of {_id: 1, x: 1} (21 bytes) starts 9c 23 95 27 20 44 f9 50.
    assert_eq!(doc_hash(&raw(doc! { "_id": 1, "x": 1 })), 5_834_769_697_128_915_868);
}

#[test]
fn the_key_never_shows_in_debug_output() {
    let k = HistoryKey::new([0xAB; 32]);
    assert_eq!(format!("{k:?}"), "HistoryKey(…)");
    let s = Store::new(PathBuf::from("root"), k.clone());
    let dbg = format!("{s:?}");
    assert!(!dbg.contains("171") && !dbg.to_lowercase().contains("abab"), "{dbg}");
    assert_eq!(HistoryKey::from_hex(&k.to_hex()), Some(k));
}

#[test]
fn the_store_is_clone_and_shares_its_files() {
    let t = TempDir::new();
    let s = store(&t);
    let c = s.clone();
    let p = Uuid::new_v4();
    record_one(&s, p, &ns(), Kind::Delete, DateTime::now(), &orders(1));
    assert_eq!(c.list(p, &ns()).unwrap().len(), 1);
}

#[test]
fn long_names_are_shortened_and_still_found_again() {
    let t = TempDir::new();
    let s = store(&t);
    let p = Uuid::new_v4();
    let thai = "ก".repeat(80); // 240 bytes, 720 encoded
    let long_db = "d.".repeat(100); // 200 characters, 400 encoded
    let spaces = vec![
        Namespace::new(long_db.clone(), thai.clone()),
        Namespace::new("shop", "x".repeat(255)),
        Namespace::new("shop", "a".repeat(250)),
        Namespace::new("shop", format!("{}b", "a".repeat(250))), // same first 200 encoded bytes as the one above
    ];
    for n in &spaces {
        let id = record_one(&s, p, n, Kind::Delete, DateTime::now(), &orders(2));
        let h = s.list(p, n).unwrap();
        assert_eq!((h.len(), h[0].count, h[0].complete), (1, 2, true));
        assert_eq!(s.read(&id).unwrap().count(), 2);
    }
    let dirs: HashSet<PathBuf> = files(&t.root()).iter().filter(|f| f.extension().is_some_and(|e| e == EXT)).map(|f| f.parent().unwrap().to_path_buf()).collect();
    assert_eq!(dirs.len(), spaces.len(), "distinct long names get distinct directories");
    for f in files(&t.root()) {
        for c in f.strip_prefix(t.root()).unwrap().components() {
            assert!(c.as_os_str().len() <= 255, "{c:?}");
        }
        #[cfg(unix)]
        if f.file_name().unwrap() == NAME_FILE {
            assert_eq!(fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }
    let names = |s: &Store| {
        let mut v: Vec<(String, String, u64)> = s.all().unwrap().into_iter().map(|(_, n, e, _)| (n.db, n.coll, e)).collect();
        v.sort();
        v
    };
    let mut want: Vec<(String, String, u64)> = spaces.iter().map(|n| (n.db.clone(), n.coll.clone(), 1)).collect();
    want.sort();
    assert_eq!(names(&s), want);

    // Moving between long names, to a short one and across databases keeps the history and the names.
    let to_short = Namespace::new(long_db.clone(), "plain");
    s.move_collection(p, &spaces[0], &to_short).unwrap();
    assert!(s.list(p, &spaces[0]).unwrap().is_empty());
    assert_eq!(s.list(p, &to_short).unwrap().len(), 1);
    let coll_dir = s.ns_dir(p, &to_short).unwrap();
    assert!(!coll_dir.join(NAME_FILE).exists(), "a short name keeps no stale name file");
    assert!(coll_dir.parent().unwrap().join(NAME_FILE).is_file());
    let to_other = Namespace::new("other".to_string(), "ข".repeat(90));
    s.move_collection(p, &to_short, &to_other).unwrap();
    assert_eq!(s.list(p, &to_other).unwrap().len(), 1);
    assert_eq!(s.read(&s.list(p, &to_other).unwrap()[0].id).unwrap().count(), 2);
    let mut want: Vec<(String, String, u64)> = spaces[1..].iter().map(|n| (n.db.clone(), n.coll.clone(), 1)).collect();
    want.push((to_other.db.clone(), to_other.coll.clone(), 1));
    want.sort();
    assert_eq!(names(&s), want);
    assert!(!t.root().join(p.to_string()).join(encode_name(&long_db).unwrap().0).exists(), "the emptied long database directory is gone");

    // Removing everything leaves nothing behind, name files included.
    for n in spaces[1..].iter().chain([&to_other]) {
        s.empty(p, n, i64::MAX).unwrap();
    }
    assert!(files(&t.root()).is_empty());
    assert!(!t.root().join(p.to_string()).exists());
}

#[test]
fn a_directory_without_a_readable_name_is_still_counted_and_purged() {
    let t = TempDir::new();
    let s = store(&t);
    let p = Uuid::new_v4();
    let now = DateTime::now().timestamp_millis();
    let n = Namespace::new("shop", "ก".repeat(80));
    let old = record_one(&s, p, &n, Kind::Delete, DateTime::from_millis(now - 45 * DAY_MS), &orders(1));
    let recent = record_one(&s, p, &n, Kind::Delete, DateTime::from_millis(now - DAY_MS), &orders(1));
    let one = s.list(p, &n).unwrap()[0].bytes;
    let name_file = s.ns_dir(p, &n).unwrap().join(NAME_FILE);
    for damage in [Some("somewhere else"), None] {
        match damage {
            Some(text) => fs::write(&name_file, text).unwrap(),
            None => fs::remove_file(&name_file).unwrap(),
        }
        // Not misnamed in the overview, but counted, listed by its real name and protected by path.
        assert!(s.all().unwrap().is_empty());
        assert_eq!(s.usage().unwrap(), 2 * one);
        assert_eq!(s.list(p, &n).unwrap().len(), 2);
        assert_eq!(s.purge(30, u64::MAX, std::slice::from_ref(&old)).unwrap(), 0);
    }
    // Purged by age and by size all the same.
    assert_eq!(s.purge(30, u64::MAX, &[]).unwrap(), one);
    assert_eq!(s.list(p, &n).unwrap().len(), 1);
    assert_eq!(s.purge(30, 0, &[]).unwrap(), one);
    assert_eq!(s.usage().unwrap(), 0);
    assert!(files(&t.root()).is_empty());
    let _ = recent;

    // Recording again writes the name back.
    record_one(&s, p, &n, Kind::Delete, DateTime::now(), &orders(1));
    assert_eq!(s.all().unwrap().len(), 1);
    assert_eq!(fs::read_to_string(&name_file).unwrap(), n.coll);
}

#[test]
fn listing_a_finished_entry_decrypts_only_its_first_and_last_record() {
    let t = TempDir::new();
    let s = store(&t);
    let p = Uuid::new_v4();
    let docs = orders(60);
    let mut w = s.begin(p, "c", &ns(), Kind::Update, doc! {}).unwrap();
    for batch in docs.chunks(20) {
        w.save(batch).unwrap();
        let hashes: Vec<(Bson, i64)> = (0..batch.len() as i64).map(|i| (Bson::Int64(i), i)).collect();
        w.after(&hashes).unwrap();
    }
    w.finish(false).unwrap();

    DECRYPTS.with(|c| c.set(0));
    let list = s.list(p, &ns()).unwrap();
    assert_eq!((list[0].count, list[0].complete, list[0].kind, list[0].conn_name.as_str()), (60, true, Kind::Update, "c"));
    assert_eq!(DECRYPTS.with(|c| c.get()), 2, "header and end record only");

    // Reading still decrypts everything.
    DECRYPTS.with(|c| c.set(0));
    assert_eq!(s.read(&list[0].id).unwrap().count(), 60);
    assert_eq!(DECRYPTS.with(|c| c.get()), 1 + 3 + 60 + 60 + 1, "header, 3 markers, documents, results, end");
}

/// The end record says whether the operation was cancelled (stopped
/// between batches); listing shows it, whichever way it reads the file.
#[test]
fn a_cancelled_entry_lists_as_cancelled() {
    let t = TempDir::new();
    let s = store(&t);
    let p = Uuid::new_v4();
    let mut w = s.begin(p, "c", &ns(), Kind::Delete, doc! {}).unwrap();
    w.save(&orders(2)).unwrap();
    let stopped = w.finish(true).unwrap();
    let done = record_one(&s, p, &ns(), Kind::Update, DateTime::now(), &orders(1));
    let mut w = s.begin(p, "c", &ns(), Kind::Delete, doc! {}).unwrap();
    w.save(&orders(1)).unwrap();
    let cut = w.id().clone();
    drop(w);
    let state = |id: &EntryId| {
        let h = s.list(p, &ns()).unwrap().into_iter().find(|h| h.id == *id).unwrap();
        (h.complete, h.cancelled)
    };
    assert_eq!(state(&stopped), (true, true));
    assert_eq!(state(&done), (true, false));
    assert_eq!(state(&cut), (false, false));
    // Read by a full scan (bytes after its end record): the same.
    OpenOptions::new().append(true).open(s.entry_path(&stopped).unwrap()).unwrap().write_all(b"xyz").unwrap();
    assert_eq!(state(&stopped), (true, true));
}

/// One entry is read alone (a restore's count), as `list` reads it: a
/// finished one by its first and last record only.
#[test]
fn one_entry_is_read_without_the_others() {
    let t = TempDir::new();
    let s = store(&t);
    let p = Uuid::new_v4();
    let ids: Vec<EntryId> = (0..3).map(|i| record_one(&s, p, &ns(), Kind::Delete, DateTime::now(), &orders(i + 1))).collect();
    let mut w = s.begin(p, "c", &ns(), Kind::Delete, doc! {}).unwrap();
    w.save(&orders(4)).unwrap();
    let open = w.id().clone();
    DECRYPTS.with(|c| c.set(0));
    let h = s.header(&ids[1]).unwrap();
    assert_eq!(DECRYPTS.with(|c| c.get()), 2, "its header and end record only");
    assert_eq!((h.count, h.complete), (2, true));
    DECRYPTS.with(|c| c.set(0));
    let o = s.header(&open).unwrap();
    assert_eq!((o.count, o.complete), (4, false));
    assert_eq!(DECRYPTS.with(|c| c.get()), 2 + (1 + 1 + 4), "its first and last record, then a full scan of that one: header, marker, documents");
    let listed = s.list(p, &ns()).unwrap();
    assert_eq!(&h, listed.iter().find(|l| l.id == ids[1]).unwrap());
    assert_eq!(&o, listed.iter().find(|l| l.id == open).unwrap());
    drop(w);
    s.remove(&ids[0]).unwrap();
    assert!(s.header(&ids[0]).is_err(), "gone");
    assert!(s.header(&EntryId { profile: p, ns: ns(), file: "../x.rhist".into() }).is_err());
}

/// An entry whose file could not be read just now (its permissions, say)
/// is listed as such, not as an incomplete entry of 0 documents, which is
/// what a damaged one is; once it can be read it lists as it is.
#[cfg(unix)]
#[test]
fn an_entry_that_could_not_be_read_is_listed_as_such() {
    let t = TempDir::new();
    let s = store(&t);
    let p = Uuid::new_v4();
    let id = record_one(&s, p, &ns(), Kind::Delete, DateTime::from_millis(1_794_596_340_123), &orders(3));
    let path = s.entry_path(&id).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    let listed = s.list(p, &ns()).unwrap();
    let one = s.header(&id);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let h = &listed[0];
    assert!(h.unreadable.as_deref().is_some_and(|why| why.contains("ermission denied")), "{h:?}");
    assert_eq!((&h.id, h.kind, h.at, h.count, h.complete, h.conn_name.as_str()), (&id, Kind::Delete, DateTime::from_millis(1_794_596_340_123), 0, false, ""));
    assert_eq!(one.unwrap(), *h);
    // Readable again: listed as it is.
    let h = &s.list(p, &ns()).unwrap()[0];
    assert_eq!((h.unreadable.as_deref(), h.count, h.complete, h.conn_name.as_str()), (None, 3, true, "local"));
    // A damaged file, or one sealed with another key, reads (as nothing): not "could not be read".
    let h = &Store::new(t.root(), key(8)).list(p, &ns()).unwrap()[0];
    assert_eq!((h.unreadable.as_deref(), h.count, h.complete), (None, 0, false));
    fs::write(&path, b"damaged").unwrap();
    let h = &s.list(p, &ns()).unwrap()[0];
    assert_eq!((h.unreadable.as_deref(), h.count, h.complete), (None, 0, false));
}

/// A listed entry is read once while its file stays the same: listing
/// again (or reading its header alone) decrypts nothing, also for an
/// entry cut short, which is otherwise read whole every time. A change to
/// the file, or another key, reads it again; the restored mark is always
/// looked at.
#[test]
fn listing_again_reads_an_unchanged_entry_from_memory() {
    let t = TempDir::new();
    let s = store(&t);
    let p = Uuid::new_v4();
    let done = record_one(&s, p, &ns(), Kind::Delete, DateTime::now(), &orders(5));
    let mut w = s.begin(p, "c", &ns(), Kind::Delete, doc! {}).unwrap();
    w.save(&orders(50)).unwrap();
    drop(w);
    DECRYPTS.with(|c| c.set(0));
    let first = s.list(p, &ns()).unwrap();
    assert!(DECRYPTS.with(|c| c.get()) >= 2 + 52);
    DECRYPTS.with(|c| c.set(0));
    assert_eq!(s.list(p, &ns()).unwrap(), first);
    assert_eq!(s.header(&done).unwrap(), *first.iter().find(|h| h.id == done).unwrap());
    assert_eq!(DECRYPTS.with(|c| c.get()), 0, "nothing decrypted again");

    s.mark_restored(&done).unwrap();
    assert!(s.header(&done).unwrap().restored);
    // Another key reads for itself.
    let other = Store::new(t.root(), key(8)).list(p, &ns()).unwrap();
    assert!(other.iter().all(|h| h.count == 0 && h.conn_name.is_empty()), "{other:?}");
    assert_eq!(s.list(p, &ns()).unwrap().iter().map(|h| h.count).collect::<Vec<_>>(), first.iter().map(|h| h.count).collect::<Vec<_>>());
    // A changed file is read again.
    let path = s.entry_path(&done).unwrap();
    let len = fs::metadata(&path).unwrap().len();
    OpenOptions::new().write(true).open(&path).unwrap().set_len(len - 20).unwrap();
    DECRYPTS.with(|c| c.set(0));
    let h = s.header(&done).unwrap();
    assert_eq!((h.count, h.complete), (5, false));
    assert!(DECRYPTS.with(|c| c.get()) > 0);
}

#[test]
fn an_unfinished_entry_lists_by_a_full_scan() {
    let t = TempDir::new();
    let s = store(&t);
    let p = Uuid::new_v4();
    let mut w = s.begin(p, "c", &ns(), Kind::Delete, doc! {}).unwrap();
    w.save(&orders(4)).unwrap();
    drop(w);
    DECRYPTS.with(|c| c.set(0));
    let h = &s.list(p, &ns()).unwrap()[0];
    assert_eq!((h.count, h.complete), (4, false));
    assert!(DECRYPTS.with(|c| c.get()) >= 5);
}

#[test]
fn the_header_records_database_collection_and_version() {
    let t = TempDir::new();
    let s = store(&t);
    let n = Namespace::new("shop", "orders");
    let id = record_one(&s, Uuid::new_v4(), &n, Kind::Update, DateTime::now(), &orders(1));
    let mut recs = Records::open(&s.entry_path(&id).unwrap(), &key(7)).unwrap();
    let h = recs.next_raw().unwrap().unwrap();
    assert_eq!(h.get_str("k").unwrap(), "h");
    assert_eq!((h.get_str("kind").unwrap(), h.get_str("db").unwrap(), h.get_str("coll").unwrap()), ("update", "shop", "orders"));
    assert_eq!(h.get_str("v").unwrap(), env!("CARGO_PKG_VERSION"));
}

fn record_starts(bytes: &[u8]) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut off = MAGIC.len();
    while off + 4 <= bytes.len() {
        starts.push(off);
        off += 4 + u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap()) as usize;
    }
    starts
}

#[test]
fn a_record_from_another_entry_cannot_be_spliced_in() {
    let t = TempDir::new();
    let s = store(&t);
    let p = Uuid::new_v4();
    let at = DateTime::now();
    let a = record_one(&s, p, &ns(), Kind::Delete, at, &orders(2));
    let b = record_one(&s, p, &ns(), Kind::Delete, at, &[raw(doc! { "_id": 99, "note": "from another entry" })]);
    let (pa, pb) = (s.entry_path(&a).unwrap(), s.entry_path(&b).unwrap());
    let (abytes, bbytes) = (fs::read(&pa).unwrap(), fs::read(&pb).unwrap());
    let (sa, sb) = (record_starts(&abytes), record_starts(&bbytes));
    // Same key, same position (2: the first document), but another file's id.
    let mut spliced = abytes[..sa[2]].to_vec();
    spliced.extend_from_slice(&bbytes[sb[2]..sb[3]]);
    spliced.extend_from_slice(&abytes[sa[3]..]);
    fs::write(&pa, &spliced).unwrap();
    assert_eq!(s.read(&a).unwrap().count(), 0, "the reader stops at the foreign record");
    // Swapping in a record of the same file at the same position is what a genuine copy looks like.
    fs::write(&pa, &abytes).unwrap();
    assert_eq!(s.read(&a).unwrap().count(), 2);
    // Another entry's header ahead of this entry's records does not authenticate them.
    let mut grafted = bbytes[..sb[1]].to_vec();
    grafted.extend_from_slice(&abytes[sa[1]..]);
    fs::write(&pa, &grafted).unwrap();
    assert_eq!(s.read(&a).unwrap().count(), 0);
}

#[test]
fn an_update_without_results_reads_one_batch_at_a_time() {
    let t = TempDir::new();
    let s = store(&t);
    let p = Uuid::new_v4();
    let docs = orders(60);
    let mut w = s.begin(p, "c", &ns(), Kind::Update, doc! {}).unwrap();
    w.save(&docs[0..20]).unwrap();
    w.save(&docs[20..40]).unwrap();
    w.after(&[]).unwrap();
    w.save(&docs[40..60]).unwrap();
    let id = w.finish(false).unwrap();
    let mut r = s.read(&id).unwrap();
    let mut got = Vec::new();
    while let Some(saved) = r.next() {
        assert!(r.befores.len() + r.out.len() <= 20, "{} + {}", r.befores.len(), r.out.len());
        got.push(saved.unwrap());
    }
    assert_eq!(got.len(), 60);
    for (g, d) in got.iter().zip(&docs) {
        assert_eq!((g.before.as_bytes(), g.after_hash), (d.as_bytes(), None));
    }
}

#[test]
fn moving_into_an_existing_collection_keeps_each_restored_mark_with_its_entry() {
    for collide in [false, true] {
        let t = TempDir::new();
        let s = store(&t);
        let p = Uuid::new_v4();
        let at = DateTime::now();
        let to = Namespace::new("shop", "orders_new");
        // The destination already has an entry; ours has three documents and is restored.
        let theirs = record_one(&s, p, &to, Kind::Delete, if collide { at } else { DateTime::from_millis(at.timestamp_millis() - 5000) }, &orders(1));
        let ours = record_one(&s, p, &ns(), Kind::Delete, at, &orders(3));
        s.mark_restored(&ours).unwrap();
        assert_eq!(collide, theirs.file == ours.file);

        s.move_collection(p, &ns(), &to).unwrap();
        let list = s.list(p, &to).unwrap();
        assert_eq!(list.len(), 2);
        let moved = list.iter().find(|h| h.count == 3).unwrap();
        let other = list.iter().find(|h| h.count == 1).unwrap();
        assert!(moved.restored, "collide={collide}: the moved entry kept its mark");
        assert!(!other.restored, "collide={collide}: the entry that was there is not marked");
        let markers: Vec<PathBuf> = files(&t.root()).into_iter().filter(|f| f.extension().is_some_and(|e| e == RESTORED_EXT)).collect();
        assert_eq!(markers, vec![restored_path(&s.entry_path(&moved.id).unwrap())], "no orphan marker");
        assert!(s.list(p, &ns()).unwrap().is_empty());
        assert!(!t.root().join(p.to_string()).join("shop").join("orders").exists());
    }

    // A marker with no entry left behind in the source is dropped, not carried over.
    let t = TempDir::new();
    let s = store(&t);
    let p = Uuid::new_v4();
    let to = Namespace::new("shop", "orders_new");
    record_one(&s, p, &to, Kind::Delete, DateTime::now(), &orders(1));
    let ours = record_one(&s, p, &ns(), Kind::Delete, DateTime::from_millis(1), &orders(1));
    fs::write(restored_path(&s.entry_path(&ours).unwrap()).with_file_name("20200101T000000000Z-delete.rhist.restored"), "").unwrap();
    s.move_collection(p, &ns(), &to).unwrap();
    assert_eq!(s.list(p, &to).unwrap().len(), 2);
    assert!(files(&t.root()).iter().all(|f| f.extension().is_none_or(|e| e != RESTORED_EXT)));
}

/// What retention costs after an operation, on a history of 5,001
/// entries in 51 collections, and what reading their file names costs.
/// `cargo test -p rongo-core --release --lib -- --ignored --nocapture report_retention_time`
#[test]
#[ignore = "timing; run explicitly in release mode"]
fn report_retention_time() {
    let t = TempDir::new();
    let s = store(&t);
    let p = Uuid::new_v4();
    let model = record_one(&s, p, &ns(), Kind::Delete, DateTime::now(), &orders(10));
    let bytes = fs::read(s.entry_path(&model).unwrap()).unwrap();
    let now = DateTime::now().timestamp_millis();
    let mut names = Vec::new();
    for c in 0..50 {
        let dir = s.ns_dir(p, &Namespace::new("shop", format!("c{c}"))).unwrap();
        fs::create_dir_all(&dir).unwrap();
        for e in 0..100 {
            let name = file_name(now - (c * 100 + e + 1) * 1000, Kind::Delete, 0);
            fs::write(dir.join(&name), &bytes).unwrap();
            // Older than a minute, as entries are: a full purge looks inside the small ones.
            File::options().write(true).open(dir.join(&name)).unwrap().set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600)).unwrap();
            names.push(name);
        }
    }
    let rounds = 20;
    let t0 = std::time::Instant::now();
    for _ in 0..rounds {
        s.purge(30, u64::MAX, std::slice::from_ref(&model)).unwrap();
    }
    println!("full purge (start-up, hourly; was after every operation), {} entries: {:?} a run", names.len() + 1, t0.elapsed() / rounds);
    let t0 = std::time::Instant::now();
    for _ in 0..rounds {
        s.trim(30, u64::MAX, &model, &[]).unwrap();
    }
    println!("retention after an operation under the limit: {:?} a run", t0.elapsed() / rounds);
    let t0 = std::time::Instant::now();
    for _ in 0..rounds {
        for n in &names {
            assert!(parse_file_name(n).is_some());
        }
    }
    println!("parse_file_name: {:?} a name", t0.elapsed() / (rounds * names.len() as u32));
}

/// What listing costs, the first time and again: 200 finished entries
/// of 1,000 documents and one cut short.
/// `cargo test -p rongo-core --release --lib -- --ignored --nocapture report_listing_time`
#[test]
#[ignore = "timing; run explicitly in release mode"]
fn report_listing_time() {
    let t = TempDir::new();
    let s = store(&t);
    let p = Uuid::new_v4();
    let model = record_one(&s, p, &ns(), Kind::Delete, DateTime::now(), &orders(1000));
    let path = s.entry_path(&model).unwrap();
    let bytes = fs::read(&path).unwrap();
    let ms = parse_file_name(&model.file).unwrap().ms;
    for e in 1..200 {
        fs::write(path.with_file_name(file_name(ms - e * 1000, Kind::Delete, 0)), &bytes).unwrap();
    }
    let mut w = s.begin(p, "c", &ns(), Kind::Delete, doc! {}).unwrap();
    w.save(&orders(1000)).unwrap();
    drop(w);
    for round in ["first", "second", "third"] {
        let t0 = std::time::Instant::now();
        let list = s.list(p, &ns()).unwrap();
        assert_eq!(list.len(), 201);
        println!("listing 201 entries ({:.1} MB), {round} time: {:?}", 201.0 * bytes.len() as f64 / 1e6, t0.elapsed());
    }
}

#[test]
fn every_batch_reaches_the_disk_before_save_returns() {
    let t = TempDir::new();
    let s = store(&t);
    let mut w = s.begin(Uuid::new_v4(), "local", &ns(), Kind::Update, doc! {}).unwrap();
    assert_eq!(w.id().file, files(&t.root()).iter().find_map(|f| f.file_name()?.to_str().map(str::to_string).filter(|n| n.ends_with(".rhist"))).unwrap());
    SYNCS.with(|c| c.set(0));
    w.save(&orders(3)).unwrap();
    assert_eq!(SYNCS.with(|c| c.get()), 1, "one sync per saved batch");
    w.after(&[(Bson::Int32(0), 1)]).unwrap();
    w.after(&[]).unwrap();
    let id = w.id().clone();
    assert_eq!(w.finish(false).unwrap(), id);
    // Only before-images must survive a power cut: without the results or
    // the end record an entry reads as "changed since" or incomplete.
    assert_eq!(SYNCS.with(|c| c.get()), 1, "results and the end record are appended without a sync of their own");
    let h = s.header(&id).unwrap();
    assert_eq!((h.count, h.complete), (3, true));
    assert_eq!(s.read(&id).unwrap().map(|r| r.unwrap().after_hash).collect::<Vec<_>>(), [Some(1), None, None]);
}
