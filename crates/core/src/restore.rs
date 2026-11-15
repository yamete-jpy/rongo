//! Collection History: conflict checks and restore (`Command::HistoryConflicts`,
//! `Command::HistoryRestore`).
//!
//! Both stream the entry in batches of `transfer::BATCH` saved documents and
//! look each batch up in the collection with one `{ _id: { $in: … } }` query
//! (raw BSON), so memory is bounded by a batch. A deleted document conflicts
//! when its `_id` exists again; an updated one when it is gone, or when it is
//! no longer as rongo left it (its hash differs from the recorded one; with no
//! recorded hash, when it differs from the saved version at all). A document
//! that is already exactly the saved version needs nothing.
//!
//! A restore inserts deleted documents (unordered; inserting recreates a
//! dropped collection) and replaces updated ones by `_id` with the saved
//! version (`replace_one`, `WINDOW` at a time, which on a sharded collection
//! before MongoDB 7.1 needs the shard key in `_id`: a documented limitation). Every document it
//! replaces is first saved in a new update entry, with the hashes of what it
//! wrote, so the restore itself can be undone; what it inserts is not
//! recorded, like any insert. Each replace matches only the exact version
//! that was looked up and saved (`{ _id, $expr: { $eq: [{ $cmp: ["$$ROOT",
//! { $literal: … }] }, 0] } }`), so a change made in between is never
//! overwritten unsaved: it is
//! a conflict found late (skipped, or on Overwrite looked up, saved and
//! replaced again). A batch whose replaced documents would take the new
//! entry past the size limit or the free disk space is not begun: the
//! restore stops there.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};

use bson::oid::ObjectId;
use bson::raw::{RawBsonRef, cstr};
use bson::{Bson, Document, RawDocumentBuf, doc};
use futures_util::stream::{FuturesUnordered, StreamExt};
use tokio_util::sync::CancellationToken;

use crate::api::{Namespace, OnConflict, Record, WriteOutcome};
use crate::conn::{Conn, FindOpts};
use crate::engine::Progress;
use crate::error::{CoreError, CoreResult};
use crate::history::{EntryId, EntryReader, Kind, Saved, Store, doc_hash, reason};
use crate::recorded::{End, Recorder, changed_nothing, warn};
use crate::transfer::BATCH;

/// Conflicts named by `_id` in `Conflicts::sample`, at most.
const SAMPLE: usize = 20;

/// What `conflicts` found.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Conflicts {
    pub total: u64,
    pub conflicts: u64,
    pub sample: Vec<Bson>,
}

/// What `restore` did (see `Event::Restored`).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Restored {
    pub restored: u64,
    pub skipped: u64,
    pub overwritten: u64,
    pub inserted_new: u64,
    pub recorded: Option<EntryId>,
    pub cancelled: bool,
    pub warning: Option<String>,
    /// A write got no confirmation and may have been applied.
    pub unconfirmed: bool,
    /// Documents counted in `restored` or `inserted_new` whose insert met a
    /// write-concern error: in on the primary, not confirmed.
    pub unconfirmed_docs: u64,
}

/// Opens an entry and learns its kind; an entry whose header can't be read is
/// refused rather than restored as empty.
fn open(record: &Record, entry: &EntryId) -> CoreResult<(Store, EntryReader, Kind)> {
    let store = record.store();
    let reader = store.read(entry)?;
    let kind = reader.kind().ok_or_else(|| CoreError::History("this entry can't be read (damaged, or saved with another key)".into()))?;
    Ok((store, reader, kind))
}

/// Up to `BATCH` saved documents. A read error ends the chunk and is kept in
/// `err`, so what was read before it is still handled.
fn next_chunk(reader: &mut EntryReader, err: &mut Option<CoreError>) -> Vec<Saved> {
    let mut out = Vec::new();
    while out.len() < BATCH {
        match reader.next() {
            Some(Ok(s)) => out.push(s),
            Some(Err(e)) => {
                *err = Some(e);
                break;
            }
            None => break,
        }
    }
    out
}

/// A lookup key for an `_id` that matches the way the server compares them:
/// numbers by value (`1`, `NumberLong(1)` and `1.0` are the same `_id`),
/// anything else by type and bytes. Limits: a Decimal128 `_id` is not
/// matched with an equal int or double, numbers nested inside an embedded
/// `_id` compare by type and bytes, and a collection with a non-simple default
/// collation (e.g. case-insensitive) can match `_id`s this key keeps apart.
/// A miss only means the conflict is found late: the insert meets a duplicate
/// key, or the guarded replace matches nothing, and is handled there.
pub(crate) fn id_key(v: RawBsonRef<'_>) -> Vec<u8> {
    let int = match v {
        RawBsonRef::Int32(n) => Some(i64::from(n)),
        RawBsonRef::Int64(n) => Some(n),
        // Integral doubles within ±2^53 are exactly an integer.
        RawBsonRef::Double(f) if f.fract() == 0.0 && f.abs() <= 9_007_199_254_740_992.0 => Some(f as i64),
        _ => None,
    };
    let mut d = RawDocumentBuf::new();
    match int {
        Some(n) => d.append(cstr!("n"), n),
        None => d.append(cstr!("v"), v),
    }
    d.into_bytes()
}

/// The key `id_key` gives a document's `_id` that was read as BSON.
pub(crate) fn id_key_of_bson(id: &Bson) -> Option<Vec<u8>> {
    let d = RawDocumentBuf::try_from(&doc! { "_id": id.clone() }).ok()?;
    Some(id_key(d.get("_id").ok().flatten()?))
}

/// The saved document's `_id`, as a value to query with and as a key.
fn saved_id(s: &Saved) -> Option<(Bson, Vec<u8>)> {
    let raw = s.before.get("_id").ok().flatten()?;
    Some((Bson::try_from(raw).ok()?, id_key(raw)))
}

/// The documents with these `_id`s as they are now, by `id_key`.
pub(crate) async fn lookup(conn: &Conn, ns: &Namespace, ids: &[Bson]) -> CoreResult<HashMap<Vec<u8>, RawDocumentBuf>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let opts = FindOpts { batch_size: Some(BATCH as u32), ..Default::default() };
    let docs = conn.open_find_raw(ns, doc! { "_id": { "$in": ids } }, opts).await?.next_batch(usize::MAX).await?;
    Ok(docs.into_iter().filter_map(|d| Some((id_key(d.get("_id").ok().flatten()?), d))).collect())
}

/// A saved document against the collection as it is now.
enum State {
    /// Nothing in the way: a deleted document is absent; an updated one is as
    /// rongo left it (and is replaced).
    Free(Option<RawDocumentBuf>),
    /// The document is already exactly the saved version: nothing to do.
    Same,
    /// A deleted document whose `_id` exists again, or an updated one that is
    /// gone (`None`) or changed since.
    Conflict(Option<RawDocumentBuf>),
}

fn classify(kind: Kind, saved: &Saved, current: Option<RawDocumentBuf>) -> State {
    match (kind, current) {
        (_, Some(c)) if c.as_bytes() == saved.before.as_bytes() => State::Same,
        (Kind::Delete, None) => State::Free(None),
        (Kind::Delete, Some(c)) => State::Conflict(Some(c)),
        (Kind::Update, None) => State::Conflict(None),
        (Kind::Update, Some(c)) => match saved.after_hash {
            Some(h) if doc_hash(&c) == h => State::Free(Some(c)),
            _ => State::Conflict(Some(c)),
        },
    }
}

/// Counts the entry's conflicts with the collection as it is now. Only reads,
/// so a cancel may drop it anywhere.
pub(crate) async fn conflicts(conn: &Conn, record: &Record, entry: &EntryId, progress: &Progress) -> CoreResult<Conflicts> {
    let (_, mut reader, kind) = open(record, entry)?;
    let note = entry.ns.to_string();
    let mut out = Conflicts::default();
    loop {
        let mut err = None;
        let chunk = next_chunk(&mut reader, &mut err);
        let keyed: Vec<Option<(Bson, Vec<u8>)>> = chunk.iter().map(saved_id).collect();
        let ids: Vec<Bson> = keyed.iter().flatten().map(|(id, _)| id.clone()).collect();
        let mut current = lookup(conn, &entry.ns, &ids).await?;
        for (s, k) in chunk.iter().zip(keyed) {
            out.total += 1;
            // A document without a readable `_id` can't be put back.
            let Some((id, key)) = k else {
                out.conflicts += 1;
                continue;
            };
            if let State::Conflict(_) = classify(kind, s, current.remove(&key)) {
                out.conflicts += 1;
                if out.sample.len() < SAMPLE {
                    out.sample.push(id);
                }
            }
        }
        if let Some(e) = err {
            return Err(e);
        }
        if chunk.is_empty() {
            break;
        }
        progress.report(out.total, None, &note);
    }
    progress.report(out.total, Some(out.total), &note);
    Ok(out)
}

/// One document to replace by `_id`.
struct Replace {
    id: Bson,
    before: Document,
    /// The version it replaces, as looked up: saved first, and the replace
    /// matches only that exact version. `None` when it was not found (an
    /// Overwrite then inserts, see `Run::overwrite_late`).
    current: Option<RawDocumentBuf>,
    /// Replacing a conflicting document (counted as overwritten).
    conflict: bool,
}

/// Tries at putting a saved version over a document that keeps changing
/// under an Overwrite, before it is skipped with a warning.
pub(crate) const ATTEMPTS: usize = 3;

/// The filter that matches the document only as it was looked up (and
/// saved): a replace never overwrites a version that was not saved first.
/// `$cmp` rather than `$eq` on `$$ROOT`: MongoDB 4.4's optimizer refuses
/// `{ $eq: ["$$ROOT", …] }` inside `$expr` (error 16409, "FieldPath::tail()
/// called on single element path"); `$cmp` compares the same way.
/// It compares numbers by value (int, long and double alike) and strings by
/// the collection's default collation when that isn't simple, so a change
/// made meanwhile to a number's type only, or (under a case-insensitive
/// collation) to letter case only, is not detected.
pub(crate) fn guard(id: &Bson, current: &RawDocumentBuf) -> CoreResult<Document> {
    Ok(doc! { "_id": id.clone(), "$expr": { "$eq": [{ "$cmp": ["$$ROOT", { "$literal": to_doc(current)? }] }, 0] } })
}

/// Guarded replaces of one batch under way at once. Each is a distinct
/// `_id`, and every before-image of the batch is saved (and synced) before
/// the first is issued.
pub(crate) const WINDOW: usize = 8;

/// Runs `write(0)` … `write(n - 1)`, `WINDOW` at a time, and answers each
/// one's result by index. Once one fails no more are issued (`None` for
/// those); the ones already under way are waited for, never dropped, so the
/// results say exactly what was written.
pub(crate) async fn windowed<F, Fut>(n: usize, write: F) -> Vec<Option<CoreResult<WriteOutcome>>>
where
    F: Fn(usize) -> Fut,
    Fut: Future<Output = CoreResult<WriteOutcome>>,
{
    let stop = AtomicBool::new(false);
    let mut out: Vec<Option<CoreResult<WriteOutcome>>> = (0..n).map(|_| None).collect();
    let mut running = FuturesUnordered::new();
    let mut next = 0;
    loop {
        while running.len() < WINDOW && next < n && !stop.load(Ordering::SeqCst) {
            let (i, w, stop) = (next, write(next), &stop);
            next += 1;
            running.push(async move {
                // One failed since this was queued: it is not issued.
                if stop.load(Ordering::SeqCst) {
                    return (i, None);
                }
                let r = w.await;
                if r.is_err() {
                    stop.store(true, Ordering::SeqCst);
                }
                (i, Some(r))
            });
        }
        match running.next().await {
            Some((i, r)) => out[i] = r,
            None => break,
        }
    }
    out
}

#[cfg(test)]
type Hook = Box<dyn Fn(&Bson)>;

#[cfg(test)]
thread_local! {
    /// Called with the `_id` before every guarded replace, so tests can change
    /// the document between the lookup and the write.
    pub(crate) static BEFORE_REPLACE: std::cell::RefCell<Option<Hook>> = std::cell::RefCell::new(None);
}

/// Tests: runs the `BEFORE_REPLACE` hook (test builds call it; release builds have none).
#[cfg(test)]
pub(crate) fn before_replace(id: &Bson) {
    BEFORE_REPLACE.with(|h| {
        if let Some(f) = h.borrow().as_ref() {
            f(id);
        }
    });
}

/// A restore under way.
struct Run<'a> {
    conn: &'a Conn,
    ns: &'a Namespace,
    kind: Kind,
    on_conflict: OnConflict,
    out: Restored,
    /// The new update entry, begun when the first replaced document is saved.
    rec: Recorder<'a>,
    /// Something was written: an error after this answers with counts.
    wrote: bool,
    /// A replace succeeded: the new entry holds a document that changed.
    replaced: bool,
    /// Documents that kept changing under an Overwrite and were skipped.
    gave_up: u64,
}

/// A failure inside a batch.
struct Failure {
    e: CoreError,
    /// The write that failed certainly changed nothing.
    refused: bool,
    /// A write that failed may have been applied (it was not refused).
    unsure: bool,
}

impl Failure {
    /// A failed write: refused when it certainly changed nothing, otherwise
    /// it may have been applied.
    fn write(e: CoreError, single: bool) -> Self {
        let refused = changed_nothing(&e, single);
        Failure { e, refused, unsure: !refused }
    }

    /// Refused before anything was written (a history that can't be kept, a refused insert).
    fn refused(e: CoreError) -> Self {
        Failure { e, refused: true, unsure: false }
    }
}

/// A failure outside a write (a read, a document that won't convert).
impl From<CoreError> for Failure {
    fn from(e: CoreError) -> Self {
        Failure { e, refused: false, unsure: false }
    }
}

pub(crate) fn to_doc(raw: &RawDocumentBuf) -> CoreResult<Document> {
    Ok(Document::try_from(raw)?)
}

/// The saved document with a new ObjectId `_id` (in the same place) and every
/// other field kept.
fn with_new_id(raw: &RawDocumentBuf) -> CoreResult<Document> {
    let mut d = to_doc(raw)?;
    d.insert("_id", ObjectId::new());
    Ok(d)
}

/// One chunk sorted out against the collection as it is now: what a batch
/// will do, nothing written or counted yet.
#[derive(Default)]
struct Planned {
    /// Already exactly the saved version: restored with nothing to do.
    same: u64,
    /// Conflicts skipped, and documents without a readable `_id`.
    skipped: u64,
    inserts: Vec<RawDocumentBuf>,
    inserts_new: Vec<Document>,
    replaces: Vec<Replace>,
}

impl Planned {
    /// The documents saved in the new entry before they are replaced (a
    /// conflict found late saves more).
    fn befores(&self) -> impl Iterator<Item = &RawDocumentBuf> {
        self.replaces.iter().filter_map(|r| r.current.as_ref())
    }
}

impl Run<'_> {
    /// Sorts a chunk out against the documents as they are now (`current`).
    fn plan(&self, chunk: Vec<Saved>, mut current: HashMap<Vec<u8>, RawDocumentBuf>) -> CoreResult<Planned> {
        let mut p = Planned::default();
        for s in chunk {
            let Some((id, key)) = saved_id(&s) else {
                p.skipped += 1;
                continue;
            };
            match classify(self.kind, &s, current.remove(&key)) {
                State::Same => p.same += 1,
                State::Free(None) => p.inserts.push(s.before),
                State::Free(Some(c)) => p.replaces.push(Replace { id, before: to_doc(&s.before)?, current: Some(c), conflict: false }),
                State::Conflict(c) => match (self.on_conflict, c) {
                    (OnConflict::Skip, _) => p.skipped += 1,
                    (OnConflict::Overwrite, Some(c)) => p.replaces.push(Replace { id, before: to_doc(&s.before)?, current: Some(c), conflict: true }),
                    (OnConflict::Overwrite, None) => p.inserts.push(s.before),
                    (OnConflict::InsertNew, _) => p.inserts_new.push(with_new_id(&s.before)?),
                },
            }
        }
        Ok(p)
    }

    /// Runs one batch: inserts, then replaces (saved first), with exact counts.
    async fn apply(&mut self, planned: Planned) -> Result<(), Failure> {
        let Planned { same, skipped, mut inserts, mut inserts_new, mut replaces } = planned;
        self.out.restored += same;
        self.out.skipped += skipped;

        // Deleted documents (and updated ones that are gone, on Overwrite) go
        // back with their own `_id`. One whose `_id` appeared since the check
        // is a conflict like the others. Another refusal (a validator, say),
        // or a write-concern error (what went in is unconfirmed), ends the
        // restore after this batch, which is finished first so the counts
        // add up.
        let (mut refusal, mut unsure) = (None, false);
        if !inserts.is_empty() {
            let docs = inserts.iter().map(to_doc).collect::<CoreResult<Vec<_>>>()?;
            let r = self.conn.insert_many_unordered(self.ns, &docs).await.map_err(|e| Failure::write(e, false))?;
            self.out.restored += r.inserted;
            self.wrote |= r.inserted > 0;
            unsure |= r.unconfirmed;
            self.unconfirmed_inserts(&r);
            refusal = r.error;
            for i in r.duplicates {
                match self.on_conflict {
                    OnConflict::Skip => self.out.skipped += 1,
                    // Looked up and replaced below like any conflict.
                    OnConflict::Overwrite => {
                        let d = std::mem::take(&mut inserts[i]);
                        let Some(raw_id) = d.get("_id").ok().flatten() else { continue };
                        replaces.push(Replace { id: Bson::try_from(raw_id).map_err(CoreError::from)?, before: to_doc(&d)?, current: None, conflict: true });
                    }
                    OnConflict::InsertNew => inserts_new.push(with_new_id(&inserts[i])?),
                }
            }
        }
        drop(inserts);

        if !inserts_new.is_empty() {
            let r = self.conn.insert_many_unordered(self.ns, &inserts_new).await.map_err(|e| Failure::write(e, false))?;
            self.out.inserted_new += r.inserted;
            // A new ObjectId that already exists: left out, like a skip.
            self.out.skipped += r.duplicates.len() as u64;
            self.wrote |= r.inserted > 0;
            unsure |= r.unconfirmed;
            self.unconfirmed_inserts(&r);
            refusal = refusal.or(r.error);
        }

        if !replaces.is_empty() {
            self.replace(replaces).await?;
        }
        // A refused insert changed nothing; the inserts around it are not recorded.
        match refusal {
            Some(e) if unsure => Err(Failure { e, refused: false, unsure: true }),
            Some(e) => Err(Failure::refused(e)),
            None => Ok(()),
        }
    }

    /// Counts the documents an insert under a write-concern error put in:
    /// on the primary, not confirmed.
    fn unconfirmed_inserts(&mut self, r: &crate::conn::Inserted) {
        if r.unconfirmed {
            self.out.unconfirmed_docs += r.inserted;
        }
    }

    /// Saves before-images in the new update entry, beginning it if needed;
    /// refused (before they are replaced) when that would take the entry
    /// past the size limit or the free disk space.
    fn save<'b>(&mut self, befores: impl IntoIterator<Item = &'b RawDocumentBuf>) -> Result<(), Failure> {
        self.rec.save(befores).map_err(Failure::refused)
    }

    /// Ends the batches on a failure inside one. The new entry is removed
    /// when nothing it holds can have been replaced; otherwise it stays as
    /// it is (listed as incomplete). A write that may have been applied makes
    /// the answer unconfirmed, never a bare error.
    fn failed(&mut self, Failure { e, refused, unsure }: Failure) -> End {
        if unsure {
            self.out.unconfirmed = true;
            self.wrote = true;
        }
        self.rec.stop(refused && !self.replaced);
        End::Failed(e)
    }

    /// Records the hash of each document as the restore left it (one `after`
    /// per `save`, as the store expects).
    async fn record_after(&mut self, written: &[Bson]) -> Result<(), Failure> {
        self.rec.after(self.conn, written).await?;
        Ok(())
    }

    /// Replaces a batch's documents, each guarded by the exact version that
    /// was looked up and saved first, `WINDOW` at a time once all of them are
    /// saved. One that changed since matches nothing: a conflict found late,
    /// skipped, or on Overwrite handled one by one afterwards. A failure
    /// stops issuing more; what the others wrote is counted and hashed.
    async fn replace(&mut self, replaces: Vec<Replace>) -> Result<(), Failure> {
        self.save(replaces.iter().filter_map(|r| r.current.as_ref()))?;
        let (now, mut late): (Vec<Replace>, Vec<Replace>) = replaces.into_iter().partition(|r| r.current.is_some());
        let (conn, ns) = (self.conn, self.ns);
        let results = windowed(now.len(), |i| {
            let r = &now[i];
            async move {
                #[cfg(test)]
                before_replace(&r.id);
                conn.replace_one(ns, guard(&r.id, r.current.as_ref().expect("looked up"))?, r.before.clone(), false).await
            }
        })
        .await;
        let mut written = Vec::with_capacity(now.len());
        let mut failed = Vec::new();
        for (r, result) in now.into_iter().zip(results) {
            match result {
                // Not issued: a replace before it failed.
                None => {}
                Some(Ok(w)) if w.matched == 0 => {
                    if self.on_conflict == OnConflict::Overwrite { late.push(r) } else { self.out.skipped += 1 }
                }
                Some(Ok(_)) => {
                    if r.conflict { self.out.overwritten += 1 } else { self.out.restored += 1 }
                    self.wrote = true;
                    self.replaced = true;
                    written.push(r.id);
                }
                Some(Err(e)) => failed.push(e),
            }
        }
        // The hashes of exactly the documents written: each replace matched
        // the saved version, so they pair with it. A document not written has
        // none, and a restore of this entry treats any difference as a conflict.
        if !failed.is_empty() {
            let refused = failed.iter().all(|e| changed_nothing(e, true));
            let _ = self.record_after(&written).await;
            return Err(Failure { e: failed.swap_remove(0), refused, unsure: !refused });
        }
        self.record_after(&written).await?;
        for r in late {
            self.overwrite_late(r).await?;
        }
        Ok(())
    }

    /// Overwrite for a document that was not where the check left it: looks
    /// it up again, saves what is there and replaces exactly that, or inserts
    /// when it is gone (never upserting over a version that was not saved).
    /// After `ATTEMPTS` changes in between it is skipped with a warning.
    async fn overwrite_late(&mut self, r: Replace) -> Result<(), Failure> {
        for _ in 0..ATTEMPTS {
            let now = lookup(self.conn, self.ns, std::slice::from_ref(&r.id)).await?.into_values().next();
            match now {
                Some(current) => {
                    self.save(std::slice::from_ref(&current))?;
                    #[cfg(test)]
                    before_replace(&r.id);
                    let w = self.conn.replace_one(self.ns, guard(&r.id, &current)?, r.before.clone(), false).await.map_err(|e| Failure::write(e, true))?;
                    if w.matched == 1 {
                        self.out.overwritten += 1;
                        self.wrote = true;
                        self.replaced = true;
                        return self.record_after(std::slice::from_ref(&r.id)).await;
                    }
                }
                None => match self.conn.insert_one(self.ns, r.before.clone()).await {
                    Ok(_) => {
                        self.out.restored += 1;
                        self.wrote = true;
                        return Ok(());
                    }
                    // It appeared meanwhile: look again.
                    Err(CoreError::Server { code: 11000, .. }) => {}
                    Err(e) => return Err(Failure::write(e, true)),
                },
            }
        }
        self.out.skipped += 1;
        self.gave_up += 1;
        Ok(())
    }
}

/// Restores an entry (see `Command::HistoryRestore`). A failure before
/// anything was written is an error; after that, the answer is what was done,
/// with the reason as a warning. Only a restore that went through the whole
/// entry marks it restored.
pub(crate) async fn restore(conn: &Conn, record: &Record, entry: &EntryId, on_conflict: OnConflict, progress: &Progress, cancel: &CancellationToken) -> CoreResult<Restored> {
    // Refused before anything is read or written.
    conn.writable("restore")?;
    let (store, mut reader, kind) = open(record, entry)?;
    if kind == Kind::Update && on_conflict == OnConflict::InsertNew {
        return Err(CoreError::InvalidInput("only deleted documents can be inserted with new _ids".into()));
    }
    let ns = &entry.ns;
    // The entry's count, for progress (a finished entry's is read from its end).
    let total = store.header(entry).ok().map(|h| h.count);
    let note = ns.to_string();
    let strategy = match on_conflict {
        OnConflict::Skip => "skip",
        OnConflict::Overwrite => "overwrite",
        OnConflict::InsertNew => "insertNew",
    };
    let how = doc! { "command": "restore", "entry": &entry.file, "onConflict": strategy };
    let mut run = Run { conn, ns, kind, on_conflict, out: Restored::default(), rec: Recorder::new(record, ns, Kind::Update, how), wrote: false, replaced: false, gave_up: 0 };
    let mut done = 0u64;
    let mut reported_total = false;

    let end = loop {
        if cancel.is_cancelled() {
            break End::Cancelled;
        }
        let mut read_err = None;
        let chunk = next_chunk(&mut reader, &mut read_err);
        if chunk.is_empty() {
            break read_err.map_or(End::Done, End::Failed);
        }
        let ids: Vec<Bson> = chunk.iter().filter_map(|s| saved_id(s).map(|(id, _)| id)).collect();
        // Not raced with the token: a cancel takes effect at the top of the
        // loop, so the answer is always given and its counts are exact.
        let current = match lookup(conn, ns, &ids).await {
            Ok(c) => c,
            Err(e) => break End::Failed(e),
        };
        // Switched to read-only meanwhile: stop before this batch.
        if let Err(e) = conn.writable("restore") {
            break End::Stopped(e);
        }
        let n = chunk.len() as u64;
        let planned = match run.plan(chunk, current) {
            Ok(p) => p,
            Err(e) => break run.failed(e.into()),
        };
        // What the batch replaces is saved first: stop before it when that
        // would take the new entry past the size limit or the free disk space.
        if let Err(e) = run.rec.check(planned.befores()) {
            break End::Stopped(e);
        }
        if let Err(f) = run.apply(planned).await {
            break run.failed(f);
        }
        done += n;
        progress.report(done, total.map(|t| t.max(done)), &note);
        reported_total = total == Some(done);
        if let Some(e) = read_err {
            break End::Failed(e);
        }
    };

    let mut out = run.out;
    let finished = matches!(end, End::Done);
    let cancelled = match end {
        End::Done => {
            progress.finish(done, reported_total, &note);
            false
        }
        End::Cancelled => true,
        End::Stopped(e) | End::Failed(e) if !run.wrote => {
            // Saved in an earlier batch, but nothing was replaced.
            if run.rec.is_open() {
                run.rec.stop(true);
            } else {
                // Kept all the same (`Run::failed`): it counts toward the limits.
                let _ = run.rec.trim(std::slice::from_ref(entry));
            }
            return Err(e);
        }
        End::Stopped(e) => {
            out.warning = Some(e.to_string());
            true
        }
        End::Failed(e) => {
            out.warning = Some(e.to_string());
            false
        }
    };
    if run.gave_up > 0 {
        let n = run.gave_up;
        warn(&mut out.warning, format!("{n} document{} kept changing during the restore and {} skipped", if n == 1 { "" } else { "s" }, if n == 1 { "was" } else { "were" }));
    }
    // Still open only when the batches stopped cleanly; a failure dropped it.
    if let Some(w) = run.rec.finish(cancelled) {
        warn(&mut out.warning, w);
    }
    if finished
        && let Err(e) = store.mark_restored(entry)
    {
        warn(&mut out.warning, format!("the entry could not be marked restored: {}", reason(e)));
    }
    // Retention makes room for the new entry by removing older ones, never
    // it or the entry just restored.
    if let Some(w) = run.rec.trim(std::slice::from_ref(entry)) {
        warn(&mut out.warning, w);
    }
    out.recorded = run.rec.id().cloned();
    out.cancelled = cancelled;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use bson::{Bson, Document, RawDocumentBuf, doc};
    use tokio_util::sync::CancellationToken;
    // Only the Unix-only test that makes a folder unreadable.
    #[cfg(unix)]
    use uuid::Uuid;

    use super::*;
    use crate::api::{Command, Namespace};
    use crate::conn::FindOpts;
    use crate::history::{EntryHeader, Kind, doc_hash};
    use crate::testkit::{TempDir, entries, files, mock, ns, raw, record, saved, seed, store};

    /// Every document, ordered by its `i` field.
    async fn all(c: &Conn) -> Vec<Document> {
        let mut docs = c.open_find(&ns(), doc! {}, FindOpts::default()).await.unwrap().next_batch(usize::MAX).await.unwrap();
        docs.sort_by_key(|d| d.get_i32("i").unwrap_or(i32::MAX));
        docs
    }

    fn header(r: &Record, id: &EntryId) -> EntryHeader {
        entries(r).into_iter().find(|e| e.id == *id).expect("listed")
    }

    /// Runs `cmd` recorded; answers its entry.
    async fn recorded(c: &Conn, r: &Record, cmd: Command) -> EntryId {
        let (progress, _) = Progress::channel();
        crate::recorded::run(c, r, cmd, &progress, &CancellationToken::new()).await.unwrap().recorded.expect("recorded")
    }

    async fn check(c: &Conn, r: &Record, id: &EntryId) -> Conflicts {
        let (progress, _) = Progress::channel();
        conflicts(c, r, id, &progress).await.unwrap()
    }

    async fn restore_with(c: &Conn, r: &Record, id: &EntryId, on_conflict: OnConflict) -> CoreResult<Restored> {
        let (progress, _) = Progress::channel();
        restore(c, r, id, on_conflict, &progress, &CancellationToken::new()).await
    }

    fn row(i: i32, v: &str) -> Document {
        doc! { "_id": i, "i": i, "v": v }
    }

    #[tokio::test]
    async fn restoring_a_delete_puts_documents_back() {
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        let docs: Vec<Document> = (0..2500).map(|i| doc! { "_id": i, "i": i, "g": i % 2 }).collect();
        seed(&c, &docs).await;
        let id = recorded(&c, &r, Command::DeleteWhere { ns: ns(), filter: doc! { "g": 1 }, many: true }).await;
        assert_eq!(all(&c).await.len(), 1250);
        assert_eq!(check(&c, &r, &id).await, Conflicts { total: 1250, conflicts: 0, sample: vec![] });

        let (progress, rx) = Progress::channel();
        let out = restore(&c, &r, &id, OnConflict::Skip, &progress, &CancellationToken::new()).await.unwrap();
        assert_eq!(out, Restored { restored: 1250, ..Default::default() });
        assert_eq!(all(&c).await, docs);
        let reports = rx.try_iter().filter_map(|resp| match resp.event {
            crate::api::Event::Progress { done, total, .. } => Some((done, total)),
            _ => None,
        });
        assert_eq!(reports.last(), Some((1250, Some(1250))));
        let list = entries(&r);
        assert_eq!(list.len(), 1, "nothing was overwritten, so no new entry: {list:?}");
        assert!(list[0].restored);
    }

    /// Four rows deleted with history; rows 1 and 2 then come back with new
    /// content (row 2's `_id` as a long, which MongoDB matches with the int).
    async fn two_conflicts() -> (TempDir, Conn, Record, EntryId) {
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        let docs: Vec<Document> = (0..4).map(|i| row(i, "old")).collect();
        seed(&c, &docs).await;
        let id = recorded(&c, &r, Command::Delete { ns: ns(), ids: (0..4).map(Bson::Int32).collect() }).await;
        seed(&c, &[row(1, "new"), doc! { "_id": 2_i64, "i": 2, "v": "new" }]).await;
        (t, c, r, id)
    }

    fn versions(docs: &[Document]) -> Vec<(i32, String)> {
        docs.iter().map(|d| (d.get_i32("i").unwrap(), d.get_str("v").unwrap().to_string())).collect()
    }

    fn want(v: &[(i32, &str)]) -> Vec<(i32, String)> {
        v.iter().map(|(i, s)| (*i, s.to_string())).collect()
    }

    #[tokio::test]
    async fn conflicting_ids_follow_the_choice() {
        let (_t, c, r, id) = two_conflicts().await;
        assert_eq!(check(&c, &r, &id).await, Conflicts { total: 4, conflicts: 2, sample: vec![Bson::Int32(1), Bson::Int32(2)] });
        let out = restore_with(&c, &r, &id, OnConflict::Skip).await.unwrap();
        assert_eq!(out, Restored { restored: 2, skipped: 2, ..Default::default() });
        assert_eq!(versions(&all(&c).await), want(&[(0, "old"), (1, "new"), (2, "new"), (3, "old")]));
        assert!(header(&r, &id).restored);
        assert_eq!(entries(&r).len(), 1);

        let (_t, c, r, id) = two_conflicts().await;
        let out = restore_with(&c, &r, &id, OnConflict::Overwrite).await.unwrap();
        assert!(out.recorded.is_some(), "{out:?}");
        assert_eq!(out, Restored { restored: 2, overwritten: 2, recorded: out.recorded.clone(), ..Default::default() });
        assert_eq!(versions(&all(&c).await), want(&[(0, "old"), (1, "old"), (2, "old"), (3, "old")]));
        assert!(header(&r, &id).restored);

        let (_t, c, r, id) = two_conflicts().await;
        let out = restore_with(&c, &r, &id, OnConflict::InsertNew).await.unwrap();
        assert_eq!(out, Restored { restored: 2, inserted_new: 2, ..Default::default() });
        let now = all(&c).await;
        assert_eq!(now.len(), 6);
        let fresh: Vec<&Document> = now.iter().filter(|d| matches!(d.get("_id"), Some(Bson::ObjectId(_)))).collect();
        assert_eq!(fresh.len(), 2);
        for (d, i) in fresh.iter().zip([1, 2]) {
            let mut kept = (*d).clone();
            kept.insert("_id", i);
            assert_eq!(kept, row(i, "old"), "a new _id, every other field kept");
        }
        assert_eq!(now.iter().filter(|d| d.get_str("v").ok() == Some("new")).count(), 2, "the conflicting documents are left alone");
        assert!(header(&r, &id).restored);
        assert_eq!(entries(&r).len(), 1, "inserts are not recorded");
    }

    #[tokio::test]
    async fn restoring_an_update_brings_back_the_saved_version() {
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        let docs: Vec<Document> = (0..1500).map(|i| doc! { "_id": i, "i": i, "x": 0 }).collect();
        seed(&c, &docs).await;
        let id = recorded(&c, &r, Command::UpdateMany { ns: ns(), filter: doc! {}, update: doc! { "$set": { "x": 1 } } }).await;
        let updated = all(&c).await;
        assert_eq!(check(&c, &r, &id).await, Conflicts { total: 1500, conflicts: 0, sample: vec![] });

        let out = restore_with(&c, &r, &id, OnConflict::Skip).await.unwrap();
        let new = out.recorded.clone().expect("what the restore replaced is recorded");
        assert_eq!(out, Restored { restored: 1500, recorded: Some(new.clone()), ..Default::default() });
        assert_eq!(all(&c).await, docs);
        assert!(header(&r, &id).restored);
        let h = header(&r, &new);
        assert_eq!((h.kind, h.count, h.complete, h.restored), (Kind::Update, 1500, true, false));
        let now = all(&c).await;
        let s = saved(&r, &new);
        assert_eq!(s.iter().map(|s| s.before.clone()).collect::<Vec<_>>(), updated.iter().map(raw).collect::<Vec<_>>());
        assert_eq!(s.iter().map(|s| s.after_hash).collect::<Vec<_>>(), now.iter().map(|d| Some(doc_hash(&raw(d)))).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn a_document_changed_again_is_a_conflict() {
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        let docs: Vec<Document> = (0..3).map(|i| doc! { "_id": i, "i": i, "x": 0 }).collect();
        seed(&c, &docs).await;
        let id = recorded(&c, &r, Command::UpdateMany { ns: ns(), filter: doc! {}, update: doc! { "$set": { "x": 1 } } }).await;
        c.update(&ns(), doc! { "_id": 2 }, crate::conn::UpdateSpec::Doc(doc! { "$set": { "x": 2 } }), false, false).await.unwrap();
        assert_eq!(check(&c, &r, &id).await, Conflicts { total: 3, conflicts: 1, sample: vec![Bson::Int32(2)] });
        let out = restore_with(&c, &r, &id, OnConflict::Skip).await.unwrap();
        assert_eq!((out.restored, out.skipped, out.overwritten), (2, 1, 0));
        let x: Vec<i32> = all(&c).await.iter().map(|d| d.get_i32("x").unwrap()).collect();
        assert_eq!(x, [0, 0, 2]);

        // Without the hashes of what was written, anything that differs from
        // the saved version is a conflict; a document already as saved is not.
        let other = Namespace::new("shop", "other");
        let mut w = store(&r).begin(r.profile, "local", &other, Kind::Update, doc! {}).unwrap();
        w.save(&[raw(&doc! { "_id": 10, "i": 10, "x": 0 }), raw(&doc! { "_id": 11, "i": 11, "x": 0 }), raw(&doc! { "_id": 12, "i": 12, "x": 0 })]).unwrap();
        let bare = w.finish(false).unwrap();
        c.insert_many(&other, vec![doc! { "_id": 10, "i": 10, "x": 0 }, doc! { "_id": 11, "i": 11, "x": 5 }, doc! { "_id": 12, "i": 12, "x": 6 }]).await.unwrap();
        assert_eq!(check(&c, &r, &bare).await, Conflicts { total: 3, conflicts: 2, sample: vec![Bson::Int32(11), Bson::Int32(12)] });
        let out = restore_with(&c, &r, &bare, OnConflict::Overwrite).await.unwrap();
        assert_eq!((out.restored, out.skipped, out.overwritten), (1, 0, 2));
        let now = c.open_find(&other, doc! {}, FindOpts::default()).await.unwrap().next_batch(usize::MAX).await.unwrap();
        assert!(now.iter().all(|d| d.get_i32("x").ok() == Some(0)), "{now:?}");
    }

    #[tokio::test]
    async fn restoring_into_a_dropped_collection() {
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        let docs: Vec<Document> = (0..3).map(|i| row(i, "old")).collect();
        seed(&c, &docs).await;
        let deleted = recorded(&c, &r, Command::DeleteWhere { ns: ns(), filter: doc! {}, many: true }).await;
        c.drop_collection(&ns()).await.unwrap();
        let out = restore_with(&c, &r, &deleted, OnConflict::Skip).await.unwrap();
        assert_eq!(out, Restored { restored: 3, ..Default::default() });
        assert_eq!(all(&c).await, docs, "inserting recreates the collection");

        let updated = recorded(&c, &r, Command::UpdateMany { ns: ns(), filter: doc! {}, update: doc! { "$set": { "v": "new" } } }).await;
        c.drop_collection(&ns()).await.unwrap();
        assert_eq!(check(&c, &r, &updated).await, Conflicts { total: 3, conflicts: 3, sample: (0..3).map(Bson::Int32).collect() });
        let refused = restore_with(&c, &r, &updated, OnConflict::InsertNew).await;
        assert!(matches!(refused, Err(CoreError::InvalidInput(_))), "{refused:?}");
        assert!(all(&c).await.is_empty());
        let out = restore_with(&c, &r, &updated, OnConflict::Skip).await.unwrap();
        assert_eq!(out, Restored { skipped: 3, ..Default::default() });
        assert!(all(&c).await.is_empty());
        let out = restore_with(&c, &r, &updated, OnConflict::Overwrite).await.unwrap();
        assert_eq!(out, Restored { restored: 3, ..Default::default() }, "a missing document is inserted; nothing was replaced");
        assert_eq!(all(&c).await, docs);
    }

    #[tokio::test]
    async fn an_incomplete_entry_restores_what_it_holds() {
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        let mut w = store(&r).begin(r.profile, "local", &ns(), Kind::Delete, doc! {}).unwrap();
        w.save(&[raw(&row(0, "a")), raw(&row(1, "b"))]).unwrap();
        w.save(&[raw(&row(2, "c"))]).unwrap();
        let id = w.id().clone();
        drop(w);
        // Cut into the last record: the reader stops before it.
        let path = files(&r.root).into_iter().find(|p| p.extension().is_some_and(|e| e == "rhist")).unwrap();
        let len = fs::metadata(&path).unwrap().len();
        fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(len - 3).unwrap();
        assert!(!header(&r, &id).complete);

        assert_eq!(check(&c, &r, &id).await, Conflicts { total: 2, conflicts: 0, sample: vec![] });
        let out = restore_with(&c, &r, &id, OnConflict::Skip).await.unwrap();
        assert_eq!(out, Restored { restored: 2, ..Default::default() });
        assert_eq!(all(&c).await, vec![row(0, "a"), row(1, "b")]);
        assert!(header(&r, &id).restored);
    }

    #[tokio::test]
    async fn restore_records_what_it_overwrites() {
        let (_t, c, r, id) = two_conflicts().await;
        let before = all(&c).await;
        let conflicting: Vec<RawDocumentBuf> = before.iter().filter(|d| d.get_str("v").ok() == Some("new")).map(raw).collect();
        assert_eq!(conflicting.len(), 2);
        let out = restore_with(&c, &r, &id, OnConflict::Overwrite).await.unwrap();
        let new = out.recorded.clone().unwrap();
        let h = header(&r, &new);
        assert_eq!((h.kind, h.count, h.complete, h.restored), (Kind::Update, 2, true, false));
        assert!(header(&r, &id).restored, "the restored entry is marked");
        let s = saved(&r, &new);
        assert_eq!(s.iter().map(|s| s.before.clone()).collect::<Vec<_>>(), conflicting, "the two overwritten documents, as they were");
        let now = all(&c).await;
        for x in &s {
            let i = x.before.get_i32("i").unwrap();
            let d = now.iter().find(|d| d.get_i32("i").ok() == Some(i)).unwrap();
            assert_eq!(x.after_hash, Some(doc_hash(&raw(d))), "the hash of what the restore wrote");
        }

        // The overwrite can be undone.
        let out = restore_with(&c, &r, &new, OnConflict::Skip).await.unwrap();
        assert_eq!((out.restored, out.skipped, out.overwritten), (2, 0, 0));
        assert_eq!(versions(&all(&c).await), want(&[(0, "old"), (1, "new"), (2, "new"), (3, "old")]));
    }

    /// Retention after a restore that can't read part of the history says
    /// so on the answer; the restore went through.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_retention_problem_after_a_restore_is_a_warning() {
        let (_t, c, r, id) = two_conflicts().await;
        let closed = r.root.join(Uuid::from_u128(7).to_string());
        fs::create_dir_all(&closed).unwrap();
        fs::set_permissions(&closed, std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();
        // Due for its hourly purge.
        store(&r).aged_ago(std::time::Duration::from_secs(3601));
        let out = restore_with(&c, &r, &id, OnConflict::Overwrite).await;
        fs::set_permissions(&closed, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        let out = out.unwrap();
        assert_eq!((out.restored, out.overwritten, out.cancelled), (2, 2, false));
        assert!(out.warning.as_deref().is_some_and(|w| w.starts_with("Collection History could not be trimmed: ") && w.contains("ermission denied")), "{out:?}");
        assert!(out.recorded.is_some() && header(&r, &id).restored);
    }

    #[tokio::test]
    async fn read_only_refuses_restore() {
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        let docs: Vec<Document> = (0..3).map(|i| row(i, "old")).collect();
        seed(&c, &docs).await;
        let deleted = recorded(&c, &r, Command::DeleteWhere { ns: ns(), filter: doc! { "i": { "$lt": 2 } }, many: true }).await;
        seed(&c, &[row(0, "new")]).await;
        let files_before = files(&r.root);
        c.set_read_only(true);
        for on_conflict in [OnConflict::Skip, OnConflict::Overwrite, OnConflict::InsertNew] {
            let out = restore_with(&c, &r, &deleted, on_conflict).await;
            assert!(matches!(out, Err(CoreError::ReadOnly(_))), "{out:?}");
        }
        c.set_read_only(false);
        assert_eq!(all(&c).await, vec![row(0, "new"), row(2, "old")]);
        assert!(!header(&r, &deleted).restored);
        assert_eq!(files(&r.root), files_before, "nothing recorded or marked");
    }

    /// Runs `f` with `hook` called before every guarded replace.
    async fn with_hook<T>(hook: impl Fn(&Bson) + 'static, f: impl std::future::Future<Output = T>) -> T {
        BEFORE_REPLACE.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
        let out = f.await;
        BEFORE_REPLACE.with(|h| *h.borrow_mut() = None);
        out
    }

    /// A hook that sets `x` of document 1 to the next number, `times` times.
    fn change_doc_1(c: &Conn, times: usize) -> impl Fn(&Bson) + 'static {
        let (c, n) = (c.clone(), std::cell::Cell::new(0));
        move |id: &Bson| {
            if *id == Bson::Int32(1) && n.get() < times {
                n.set(n.get() + 1);
                let x = 100 + n.get() as i32;
                c.with_store(|s| s.update(&ns(), &doc! { "_id": 1 }, &doc! { "$set": { "x": x } }, false, false)).unwrap();
            }
        }
    }

    /// Three documents updated (`x` 0 → 1) with history.
    async fn three_updated() -> (TempDir, Conn, Record, EntryId) {
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        seed(&c, &(0..3).map(|i| doc! { "_id": i, "i": i, "x": 0 }).collect::<Vec<_>>()).await;
        let id = recorded(&c, &r, Command::UpdateMany { ns: ns(), filter: doc! {}, update: doc! { "$set": { "x": 1 } } }).await;
        (t, c, r, id)
    }

    fn xs(docs: &[Document]) -> Vec<i32> {
        docs.iter().map(|d| d.get_i32("x").unwrap()).collect()
    }

    #[tokio::test]
    async fn a_document_changed_before_its_replace_is_never_overwritten_unsaved() {
        // Skip: changed between the lookup and the replace, it is a conflict
        // found late and left as it is.
        let (_t, c, r, id) = three_updated().await;
        let out = with_hook(change_doc_1(&c, 1), restore_with(&c, &r, &id, OnConflict::Skip)).await.unwrap();
        assert_eq!((out.restored, out.skipped, out.overwritten, out.warning.as_deref()), (2, 1, 0, None));
        assert_eq!(xs(&all(&c).await), [0, 101, 0]);

        // Overwrite: its new version is saved, then replaced.
        let (_t, c, r, id) = three_updated().await;
        let out = with_hook(change_doc_1(&c, 1), restore_with(&c, &r, &id, OnConflict::Overwrite)).await.unwrap();
        assert_eq!((out.restored, out.skipped, out.overwritten, out.warning.as_deref()), (2, 0, 1, None));
        assert_eq!(xs(&all(&c).await), [0, 0, 0]);
        let s = saved(&r, &out.recorded.unwrap());
        let doc_1: Vec<&Saved> = s.iter().filter(|s| s.before.get_i32("i").ok() == Some(1)).collect();
        assert_eq!(doc_1.len(), 2, "the version looked up first, then the one replaced: {s:?}");
        assert_eq!(doc_1[0].before, raw(&doc! { "_id": 1, "i": 1, "x": 1 }));
        assert_eq!(doc_1[0].after_hash, None, "never written over");
        assert_eq!(doc_1[1].before, raw(&doc! { "_id": 1, "i": 1, "x": 101 }), "what was overwritten is saved");
        assert_eq!(doc_1[1].after_hash, Some(doc_hash(&raw(&doc! { "_id": 1, "i": 1, "x": 0 }))));
        assert!(header(&r, &id).restored);
    }

    /// Forty documents updated (`x` 0 → 1) with history; the documents as
    /// the update left them.
    async fn forty_updated() -> (TempDir, Conn, Record, EntryId, Vec<RawDocumentBuf>) {
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        seed(&c, &(0..40).map(|i| doc! { "_id": i, "i": i, "x": 0 }).collect::<Vec<_>>()).await;
        let id = recorded(&c, &r, Command::UpdateMany { ns: ns(), filter: doc! {}, update: doc! { "$set": { "x": 1 } } }).await;
        let updated = all(&c).await.iter().map(raw).collect();
        (t, c, r, id, updated)
    }

    /// A batch's guarded replaces run eight at a time, and none is issued
    /// before every before-image of the batch is saved and synced.
    #[tokio::test]
    async fn replaces_run_eight_at_a_time_once_their_batch_is_synced() {
        let (_t, c, r, id, updated) = forty_updated().await;
        c.set_mock_delay(std::time::Duration::from_millis(2));
        c.most_calls_at_once();
        crate::history::SYNCS.with(|n| n.set(0));
        let (rr, from, issued) = (r.clone(), id.clone(), std::rc::Rc::new(std::cell::Cell::new(0)));
        let count = issued.clone();
        let hook = move |rid: &Bson| {
            assert_eq!(crate::history::SYNCS.with(|n| n.get()), 1, "synced before replacing {rid}");
            let new = entries(&rr).into_iter().find(|e| e.id != from).expect("the restore's entry");
            let befores: Vec<RawDocumentBuf> = saved(&rr, &new.id).into_iter().map(|s| s.before).collect();
            assert_eq!(befores, updated, "the whole batch was saved before replacing {rid}");
            count.set(count.get() + 1);
        };
        let out = with_hook(hook, restore_with(&c, &r, &id, OnConflict::Skip)).await.unwrap();
        c.set_mock_delay(std::time::Duration::ZERO);
        assert_eq!((out.restored, out.skipped, out.warning.as_deref(), issued.get()), (40, 0, None, 40));
        assert_eq!(c.most_calls_at_once(), 8, "eight replaces under way at once");
        assert!(xs(&all(&c).await).iter().all(|x| *x == 0));
        let s = saved(&r, &out.recorded.unwrap());
        let now = all(&c).await;
        assert_eq!(s.iter().map(|s| s.after_hash).collect::<Vec<_>>(), now.iter().map(|d| Some(doc_hash(&raw(d)))).collect::<Vec<_>>());
    }

    /// A replace that fails mid-window stops it: no more are issued, those
    /// under way finish, and exactly they count, with their hashes recorded.
    #[tokio::test]
    async fn a_failure_mid_window_counts_exactly_what_was_replaced() {
        let (_t, c, r, id, updated) = forty_updated().await;
        c.set_mock_delay(std::time::Duration::from_millis(2));
        let lock = c.clone();
        let hook = move |rid: &Bson| {
            if *rid == Bson::Int32(5) {
                lock.set_read_only(true);
            }
        };
        let out = with_hook(hook, restore_with(&c, &r, &id, OnConflict::Skip)).await.unwrap();
        c.set_mock_delay(std::time::Duration::ZERO);
        c.set_read_only(false);
        assert!(out.warning.as_deref().is_some_and(|w| w.contains("read-only")), "{out:?}");
        // Replaces 0 to 4 were under way when 5 was refused; 6 on were never issued.
        assert_eq!((out.restored, out.skipped, out.cancelled, out.unconfirmed), (5, 0, false, false));
        let now = all(&c).await;
        assert_eq!(xs(&now), (0..40).map(|i| if i < 5 { 0 } else { 1 }).collect::<Vec<_>>());
        let s = saved(&r, out.recorded.as_ref().expect("what was replaced is kept"));
        assert_eq!(s.iter().map(|s| s.before.clone()).collect::<Vec<_>>(), updated);
        let want: Vec<Option<i64>> = (0..40).map(|i| (i < 5).then(|| doc_hash(&raw(&now[i])))).collect();
        assert_eq!(s.iter().map(|s| s.after_hash).collect::<Vec<_>>(), want, "hashes for exactly the documents replaced");
        assert!(!header(&r, &id).restored);
    }

    /// Documents put back under a write-concern error went in on the primary
    /// but were not confirmed: unconfirmed, with the reason, not a refusal.
    #[tokio::test]
    async fn a_write_concern_error_on_restored_documents_is_unconfirmed() {
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        seed(&c, &(0..3).map(|i| doc! { "_id": i, "i": i, "x": 0 }).collect::<Vec<_>>()).await;
        let id = recorded(&c, &r, Command::DeleteWhere { ns: ns(), filter: doc! {}, many: true }).await;
        c.concern_fails_next();
        let out = restore_with(&c, &r, &id, OnConflict::Skip).await.unwrap();
        assert_eq!((out.restored, out.cancelled, out.unconfirmed, out.unconfirmed_docs), (3, false, true, 3), "{out:?}");
        assert!(out.warning.as_deref().is_some_and(|w| w.contains("write concern error 64")), "{out:?}");
        assert_eq!(xs(&all(&c).await), [0, 0, 0], "it was applied");
        assert!(!header(&r, &id).restored);
    }

    /// A restore write that gets no answer may have been applied: the answer
    /// says so (unconfirmed, with the reason) instead of failing, the new
    /// entry holding what it may have overwritten is kept, and it counts
    /// toward the history's limits.
    #[tokio::test]
    async fn a_restore_write_with_no_answer_is_unconfirmed() {
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        seed(&c, &[doc! { "_id": 1, "i": 1, "x": 0 }]).await;
        let id = recorded(&c, &r, Command::UpdateMany { ns: ns(), filter: doc! {}, update: doc! { "$set": { "x": 1 } } }).await;
        c.lose_next_reply();
        let out = restore_with(&c, &r, &id, OnConflict::Skip).await.unwrap();
        assert_eq!((out.restored, out.skipped, out.cancelled, out.unconfirmed), (0, 0, false, true), "{out:?}");
        assert!(out.warning.as_deref().is_some_and(|w| w.contains("network error")), "{out:?}");
        let new = out.recorded.expect("what it may have overwritten is kept");
        assert_eq!(saved(&r, &new).iter().map(|s| s.before.clone()).collect::<Vec<_>>(), [raw(&doc! { "_id": 1, "i": 1, "x": 1 })]);
        assert_eq!(xs(&all(&c).await), [0], "it was applied");
        assert!(!header(&r, &id).restored);
        assert_eq!(store(&r).running_total(), Some(store(&r).usage().unwrap()), "the kept entry is counted");
    }

    #[tokio::test]
    async fn a_document_that_keeps_changing_is_skipped_with_a_warning() {
        let (_t, c, r, id) = three_updated().await;
        let out = with_hook(change_doc_1(&c, usize::MAX), restore_with(&c, &r, &id, OnConflict::Overwrite)).await.unwrap();
        assert_eq!((out.restored, out.skipped, out.overwritten), (2, 1, 0));
        assert!(out.warning.as_deref().is_some_and(|w| w.contains("1 document kept changing")), "{out:?}");
        assert_eq!(xs(&all(&c).await), [0, 100 + 1 + super::ATTEMPTS as i32, 0], "left as it is");
        let s = saved(&r, &out.recorded.unwrap());
        assert_eq!(s.iter().filter(|s| s.before.get_i32("i").ok() == Some(1)).count(), 1 + super::ATTEMPTS, "every version looked at is saved");
    }

    #[tokio::test]
    async fn a_deleted_document_back_as_saved_is_not_a_conflict() {
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        seed(&c, &(0..3).map(|i| row(i, "old")).collect::<Vec<_>>()).await;
        let id = recorded(&c, &r, Command::DeleteWhere { ns: ns(), filter: doc! {}, many: true }).await;
        seed(&c, &[row(1, "old")]).await;
        assert_eq!(check(&c, &r, &id).await, Conflicts { total: 3, conflicts: 0, sample: vec![] });
        let out = restore_with(&c, &r, &id, OnConflict::Overwrite).await.unwrap();
        assert_eq!(out, Restored { restored: 3, ..Default::default() }, "nothing replaced, nothing recorded");
        assert_eq!(all(&c).await, (0..3).map(|i| row(i, "old")).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn an_insert_refusal_still_counts_the_batch() {
        // Two saved versions of document 0 (the second meets the first) and
        // one document the collection's validator refuses.
        for (on_conflict, expected) in [(OnConflict::Skip, (1, 1, 0, 0)), (OnConflict::Overwrite, (1, 0, 1, 0)), (OnConflict::InsertNew, (1, 0, 0, 1))] {
            let (t, c) = (TempDir::new(), mock());
            let r = record(&t);
            c.create_collection(&ns()).await.unwrap();
            c.set_validator(&ns(), doc! { "v": { "$ne": "bad" } }, "strict", "error").await.unwrap();
            let mut w = store(&r).begin(r.profile, "local", &ns(), Kind::Delete, doc! {}).unwrap();
            w.save(&[raw(&row(0, "a")), raw(&row(0, "b")), raw(&row(5, "bad"))]).unwrap();
            let id = w.finish(false).unwrap();
            let out = restore_with(&c, &r, &id, on_conflict).await.unwrap();
            assert_eq!((out.restored, out.skipped, out.overwritten, out.inserted_new), expected, "{on_conflict:?}");
            assert!(out.warning.is_some(), "{out:?}");
            assert!(!header(&r, &id).restored);
        }
    }

    #[tokio::test]
    async fn a_duplicate_key_race_is_a_conflict() {
        // The same `_id` twice in one batch: the check finds neither in the
        // collection, so both are inserted and the second meets the first.
        for (on_conflict, expected) in [(OnConflict::Skip, (1, 1, 0, "a")), (OnConflict::Overwrite, (1, 0, 1, "b"))] {
            let (t, c) = (TempDir::new(), mock());
            let r = record(&t);
            let mut w = store(&r).begin(r.profile, "local", &ns(), Kind::Delete, doc! {}).unwrap();
            w.save(&[raw(&row(0, "a")), raw(&row(0, "b"))]).unwrap();
            let id = w.finish(false).unwrap();
            let out = restore_with(&c, &r, &id, on_conflict).await.unwrap();
            let now = all(&c).await;
            assert_eq!((out.restored, out.skipped, out.overwritten, now[0].get_str("v").unwrap()), expected, "{on_conflict:?}");
            if on_conflict == OnConflict::Overwrite {
                let s = saved(&r, &out.recorded.expect("the replaced document is recorded"));
                assert_eq!(s.iter().map(|s| s.before.clone()).collect::<Vec<_>>(), [raw(&row(0, "a"))]);
            }
        }
    }

    /// What a restore replaces is kept in a new entry first; there is no
    /// estimate before, so a running check stops it before a batch whose
    /// documents would take that entry past the size limit: before anything
    /// was written, as too large; after, with what was restored and why it
    /// stopped. The entry restored from is not marked.
    #[tokio::test]
    async fn a_restore_that_outgrows_the_limit_stops_before_the_batch() {
        let (t, c) = (TempDir::new(), mock());
        let mut r = record(&t);
        // 2,000 documents updated with history; the second thousand are large.
        let docs: Vec<Document> = (0..2000).map(|i| if i < 1000 { doc! { "_id": i, "i": i, "x": 0 } } else { doc! { "_id": i, "i": i, "x": 0, "pad": "x".repeat(2000) } }).collect();
        seed(&c, &docs).await;
        let id = recorded(&c, &r, Command::UpdateMany { ns: ns(), filter: doc! {}, update: doc! { "$set": { "x": 1 } } }).await;
        let updated = all(&c).await;

        r.max_bytes = 10_000;
        let err = restore_with(&c, &r, &id, OnConflict::Skip).await.unwrap_err();
        assert!(matches!(err, CoreError::HistoryTooLarge { needed, limit: 10_000, .. } if needed > 10_000), "{err:?}");
        assert_eq!(all(&c).await, updated, "nothing written");
        assert_eq!(entries(&r).len(), 1, "nothing kept");
        assert!(!header(&r, &id).restored);

        r.max_bytes = 1 << 20;
        let out = restore_with(&c, &r, &id, OnConflict::Skip).await.unwrap();
        assert_eq!((out.restored, out.skipped, out.cancelled), (1000, 0, true), "{out:?}");
        assert!(out.warning.as_deref().is_some_and(|w| w.starts_with("Collection History would need")), "{out:?}");
        let now = all(&c).await;
        assert_eq!((&now[..1000], &now[1000..]), (&docs[..1000], &updated[1000..]), "the first batch only");
        let h = header(&r, &out.recorded.expect("what it replaced is kept"));
        assert_eq!((h.count, h.complete, h.cancelled), (1000, true, true));
        assert!(!header(&r, &id).restored);
    }

    /// Restoring 400 updated documents from a demo server that waits 5 ms a
    /// call, as a round trip to a slow server would.
    /// `cargo test -p rongo-core --release --lib -- --ignored --nocapture report_restore_replace_time`
    #[tokio::test]
    #[ignore = "timing; run explicitly in release mode"]
    async fn report_restore_replace_time() {
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        let n = 400;
        seed(&c, &(0..n).map(|i| doc! { "_id": i, "i": i, "x": 0 }).collect::<Vec<_>>()).await;
        let id = recorded(&c, &r, Command::UpdateMany { ns: ns(), filter: doc! {}, update: doc! { "$set": { "x": 1 } } }).await;
        c.set_mock_delay(std::time::Duration::from_millis(5));
        let t0 = std::time::Instant::now();
        let out = restore_with(&c, &r, &id, OnConflict::Skip).await.unwrap();
        let took = t0.elapsed();
        c.set_mock_delay(std::time::Duration::ZERO);
        assert_eq!(out.restored, n as u64);
        println!("restore of {n} updated documents, 5 ms a call: {took:?}");
    }

    #[tokio::test]
    async fn restore_stops_between_batches() {
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        let docs: Vec<Document> = (0..2500).map(|i| doc! { "_id": i, "i": i }).collect();
        seed(&c, &docs).await;
        let id = recorded(&c, &r, Command::DeleteWhere { ns: ns(), filter: doc! {}, many: true }).await;
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let progress = Progress::with_hook(move |done| {
            if done >= 1000 {
                token.cancel();
            }
        });
        let out = restore(&c, &r, &id, OnConflict::Skip, &progress, &cancel).await.unwrap();
        assert_eq!(out, Restored { restored: 1000, cancelled: true, ..Default::default() });
        assert_eq!(all(&c).await.len(), 1000);
        assert!(!header(&r, &id).restored, "a cancelled restore does not mark the entry");

        // Switched to read-only after the first batch: stops there, with a warning.
        c.delete(&ns(), doc! {}, true).await.unwrap();
        let conn = c.clone();
        let progress = Progress::with_hook(move |done| {
            if done >= 1000 {
                conn.set_read_only(true);
            }
        });
        let out = restore(&c, &r, &id, OnConflict::Skip, &progress, &CancellationToken::new()).await.unwrap();
        assert_eq!((out.restored, out.cancelled, out.warning.is_some()), (1000, true, true), "{out:?}");
        c.set_read_only(false);
        assert_eq!(all(&c).await.len(), 1000);
        assert!(!header(&r, &id).restored);
    }
}
