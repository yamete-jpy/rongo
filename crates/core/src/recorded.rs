//! Recorded deletes and updates (`Command::Recorded`): Collection History
//! saves the documents a write will delete or change before writing them.
//!
//! The matching documents are read in batches of `transfer::BATCH`. Each batch
//! is saved to the history store, then exactly those documents are written by
//! `_id`; an update also keeps its filter (`{ $and: [filter, { _id: { $in: ids
//! } }] }`), so a document that stopped matching meanwhile is left alone.
//! Nothing changes that was not saved first, and a document that starts
//! matching after it was read is not touched (this differs slightly from one
//! server-side `deleteMany`). An update then reads the batch back and records
//! the hash of each document as it left it.
//!
//! The history it needs is estimated before anything is read (`check_room`);
//! as it runs, each batch is checked again (`check_growth`), and it stops
//! before one that would take the entry past the size limit or the free disk
//! space.

use std::collections::HashSet;

use bson::raw::{RawBsonRef, cstr};
use bson::{Bson, Document, RawDocumentBuf, doc};
use ring::digest;
use tokio_util::sync::CancellationToken;

use crate::api::{Command, Namespace, Record, UpdateBody, WriteOutcome};
use crate::conn::{Conn, FindOpts, UpdateSpec};
use crate::engine::Progress;
use crate::error::{CoreError, CoreResult};
use crate::history::{EntryId, EntryWriter, Kind, Store, doc_hash, reason};
use crate::transfer::BATCH;

/// The write applied to each batch, by `_id`.
enum Write {
    Delete,
    Update(UpdateSpec),
    Replace(Document),
}

/// A recordable command, taken apart.
struct Plan {
    ns: Namespace,
    /// What its entry is: from `Command::recorded_target`.
    kind: Kind,
    filter: Document,
    /// Every match, or only the first.
    many: bool,
    /// Insert when nothing matches (not recorded).
    upsert: bool,
    write: Write,
    /// Named in a read-only refusal, as the unrecorded command names it.
    what: &'static str,
    /// Kept in the entry's header: the command and what it was given (for
    /// selected rows, how many: their `_id`s are in the before-images).
    how: Document,
}

impl Plan {
    /// `cmd` taken apart, if `Command::recorded_target` (the one list of
    /// what can be recorded) names it.
    fn new(cmd: Command) -> CoreResult<Plan> {
        let Some((_, kind)) = cmd.recorded_target() else {
            return Err(CoreError::InvalidInput("only deletes and updates can be kept in Collection History".into()));
        };
        Ok(match cmd {
            Command::Delete { ns, ids } => {
                let how = doc! { "command": "delete", "count": ids.len() as i64 };
                Plan { how, ns, kind, filter: doc! { "_id": { "$in": ids } }, many: true, upsert: false, write: Write::Delete, what: "delete" }
            }
            Command::DeleteWhere { ns, filter, many } => {
                let mut how = doc! { "command": "deleteWhere" };
                shown_filter(&mut how, &filter);
                how.insert("many", many);
                Plan { how, ns, kind, filter, many, upsert: false, write: Write::Delete, what: "delete" }
            }
            Command::Replace { ns, id, doc } => {
                let filter = doc! { "_id": id };
                Plan { how: doc! { "command": "replace", "filter": filter.clone() }, ns, kind, filter, many: false, upsert: false, write: Write::Replace(doc), what: "replace" }
            }
            Command::UpdateMany { ns, filter, update } => {
                let mut how = doc! { "command": "updateMany" };
                shown_filter(&mut how, &filter);
                how.insert("update", update.clone());
                Plan { how, ns, kind, filter, many: true, upsert: false, write: Write::Update(UpdateSpec::Doc(update)), what: "update" }
            }
            Command::Update { ns, filter, update, many, upsert } => {
                let (shown, spec) = match update {
                    UpdateBody::Doc(d) => (Bson::Document(d.clone()), UpdateSpec::Doc(d)),
                    UpdateBody::Pipeline(p) => (Bson::from(p.as_slice()), UpdateSpec::Pipeline(p)),
                };
                let mut how = doc! { "command": "update" };
                shown_filter(&mut how, &filter);
                how.extend(doc! { "update": shown, "many": many, "upsert": upsert });
                Plan { how, ns, kind, filter, many, upsert, write: Write::Update(spec), what: "update" }
            }
            _ => unreachable!("recorded_target names only the commands above"),
        })
    }
}

/// Adds the filter to an entry header's `how`: the Delete and Update dialogs'
/// "Selected Document(s)" query (`{ _id: { $in: [...] } }`) as its count, as
/// for selected rows (the `_id`s are in the before-images, and a header
/// holding many of them would be read at every listing); any other filter as
/// it is.
fn shown_filter(how: &mut Document, filter: &Document) {
    let ids = match filter.get_document("_id") {
        Ok(sel) if filter.len() == 1 && sel.len() == 1 => sel.get_array("$in").ok(),
        _ => None,
    };
    match ids {
        Some(ids) => how.insert("count", ids.len() as i64),
        None => how.insert("filter", filter.clone()),
    };
}

/// One Collection History entry written by a recorded write, a restore or a
/// copy: begun when its first before-images are saved, so an operation that
/// saves nothing leaves no entry. What becomes of it after a failure
/// (removed, or left cut short) is for the caller to say (`stop`): only the
/// caller knows whether anything it holds may have changed.
pub(crate) struct Recorder<'a> {
    store: Store,
    record: &'a Record,
    ns: &'a Namespace,
    kind: Kind,
    /// The header's `how`.
    how: Document,
    writer: Option<EntryWriter>,
    /// The entry's id; kept when the writer is dropped on an error, gone with
    /// the file when it is abandoned.
    id: Option<EntryId>,
}

impl<'a> Recorder<'a> {
    pub(crate) fn new(record: &'a Record, ns: &'a Namespace, kind: Kind, how: Document) -> Self {
        Recorder { store: record.store(), record, ns, kind, how, writer: None, id: None }
    }

    /// Bytes the entry holds so far.
    pub(crate) fn bytes(&self) -> u64 {
        self.writer.as_ref().map_or(0, EntryWriter::bytes)
    }

    /// Refuses saving `befores` when that would take the entry past the
    /// size limit or the free disk space (`check_growth`).
    pub(crate) fn check<'b>(&self, befores: impl IntoIterator<Item = &'b RawDocumentBuf>) -> CoreResult<()> {
        check_growth(&self.store, self.record.max_bytes, self.bytes(), befores)
    }

    /// The entry's writer, the entry begun first if it is not yet. Saving
    /// through it skips the size check: a caller calls `check` first (as
    /// `save` does), or the entry can outgrow the limit or the disk.
    pub(crate) fn writer(&mut self) -> CoreResult<&mut EntryWriter> {
        if self.writer.is_none() {
            let w = self.store.begin(self.record.profile, &self.record.conn_name, self.ns, self.kind, self.how.clone())?;
            self.id = Some(w.id().clone());
            self.writer = Some(w);
        }
        Ok(self.writer.as_mut().expect("begun"))
    }

    /// Saves documents, as they are, before they are changed: checked
    /// (`check`), the entry begun if need be, then written and synced.
    pub(crate) fn save<'b>(&mut self, befores: impl IntoIterator<Item = &'b RawDocumentBuf>) -> CoreResult<()> {
        let befores: Vec<&RawDocumentBuf> = befores.into_iter().collect();
        if befores.is_empty() {
            return Ok(());
        }
        self.check(befores.iter().copied())?;
        self.writer()?.save(befores)
    }

    /// Records the hash of each of these documents as the write left it,
    /// read back by `_id` (one `after` per `save`, as the store expects).
    pub(crate) async fn after(&mut self, conn: &Conn, ids: &[Bson]) -> CoreResult<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let (filter, _) = by_id(ids);
        let hashes = after_hashes(conn, self.ns, filter).await?;
        self.writer.as_mut().expect("begun").after(&hashes)
    }

    /// Begun, and not ended yet.
    pub(crate) fn is_open(&self) -> bool {
        self.writer.is_some()
    }

    /// The entry, once begun; kept by `stop(false)`, gone with `stop(true)`.
    pub(crate) fn id(&self) -> Option<&EntryId> {
        self.id.as_ref()
    }

    /// Ends writing after a failure. With `abandon` (nothing the entry
    /// holds can have changed) its file is removed; otherwise the file stays
    /// as it is: listed as incomplete, still restorable.
    pub(crate) fn stop(&mut self, abandon: bool) {
        if let Some(w) = self.writer.take()
            && abandon
        {
            w.abandon();
            self.id = None;
        }
    }

    /// Finishes the entry, if it is still open; a warning when it could not be.
    pub(crate) fn finish(&mut self, cancelled: bool) -> Option<String> {
        let w = self.writer.take()?;
        w.finish(cancelled).err().map(|e| format!("Collection History may be incomplete: {}", reason(e)))
    }

    /// Retention after the operation, which makes room by removing older
    /// entries, never this one nor those in `protect`; a warning when it
    /// left something it could not read or remove.
    pub(crate) fn trim(&self, protect: &[EntryId]) -> Option<String> {
        let id = self.id.as_ref()?;
        self.store.trim(self.record.keep_days, self.record.max_bytes, id, protect).err().map(trim_warning)
    }
}

/// The entry being recorded, and what the writes did.
struct Entry<'a> {
    rec: Recorder<'a>,
    /// A write succeeded, or may have: what was saved may have changed on
    /// the server.
    wrote: bool,
    /// The last write failed without being refused (no answer came, or a
    /// write of many documents failed part way): it may have been applied.
    unsure: bool,
}

impl Entry<'_> {
    /// Ends the entry on an error. It is removed when nothing it holds can
    /// have been written (no earlier write, and this batch's write was
    /// `refused` before changing anything); otherwise the writer is dropped and
    /// the file stays as it is: listed as incomplete, still restorable.
    fn fail(&mut self, e: CoreError, refused: bool) -> End {
        self.rec.stop(refused && !self.wrote);
        End::Failed(e)
    }
}

/// How the batches of a recorded write or a restore ended.
pub(crate) enum End {
    /// Every document was handled.
    Done,
    /// The token stopped it between batches.
    Cancelled,
    /// Stopped between batches, for this reason: the connection was switched
    /// to read-only meanwhile, or the next batch would take the entry past
    /// the size limit or the free disk space.
    Stopped(CoreError),
    /// An error; what became of the entry was decided where it happened.
    Failed(CoreError),
}

/// Runs `cmd` (see `Command::Recorded`). Answers what was written, with the
/// entry that holds it. A failure before anything was written is an error;
/// after that, or after a write that may have been applied (not refused:
/// no answer came, or a write of many documents failed part way), the
/// answer is what is confirmed, with the reason as a warning and, for the
/// latter, `unconfirmed` (an error would invite a retry, which could apply
/// an update twice).
pub(crate) async fn run(conn: &Conn, record: &Record, cmd: Command, progress: &Progress, cancel: &CancellationToken) -> CoreResult<WriteOutcome> {
    let plan = Plan::new(cmd)?;
    let kind = plan.kind;
    // Refused before anything is read or saved.
    conn.writable(plan.what)?;
    if let Write::Update(UpdateSpec::Doc(update)) = &plan.write {
        check_update(update)?;
    }
    let store = record.store();

    // Only reads so far, so a cancel may drop them.
    let setup = async {
        // A count the server refuses (`countDocuments` allows no `$where` or
        // `$near`) leaves the estimate unknown, like a missing average: 0. A
        // filter that is wrong for the find as well fails just below.
        let total = if plan.many { conn.count(&plan.ns, plan.filter.clone()).await.unwrap_or(0) } else { 1 };
        check_room(&store, record.max_bytes, total.saturating_mul(avg_obj_size(conn, &plan.ns).await))?;
        let opts = FindOpts { batch_size: Some(BATCH as u32), limit: (!plan.many).then_some(1), ..Default::default() };
        let cursor = conn.open_find_raw(&plan.ns, plan.filter.clone(), opts).await?;
        CoreResult::Ok((total, cursor))
    };
    let (total, mut cursor) = tokio::select! {
        r = setup => r?,
        () = cancel.cancelled() => return Ok(WriteOutcome { cancelled: true, ..Default::default() }),
    };

    let note = plan.ns.to_string();
    let mut entry = Entry { rec: Recorder::new(record, &plan.ns, kind, plan.how.clone()), wrote: false, unsure: false };
    let mut out = WriteOutcome::default();
    let (mut done, mut reported_total) = (0, false);
    // An update can move a document ahead of the cursor (by changing a field
    // of the index it walks), which then returns it again: it was saved and
    // written already, so it is skipped. Kept as 128-bit digests of the `_id`s.
    let mut seen: HashSet<u128> = HashSet::new();
    let end = loop {
        if cancel.is_cancelled() {
            break End::Cancelled;
        }
        let batch = tokio::select! {
            b = cursor.next_batch(BATCH) => b,
            () = cancel.cancelled() => break End::Cancelled,
        };
        let batch = match batch {
            Ok(b) if b.is_empty() => break End::Done,
            Ok(b) => b,
            Err(e) => break entry.fail(e, false),
        };
        let mut docs = Vec::with_capacity(batch.len());
        let mut ids = Vec::with_capacity(batch.len());
        for d in batch {
            // A document whose `_id` is missing (or unreadable) can't be written by `_id`: left alone.
            let Some(raw_id) = d.get("_id").ok().flatten() else { continue };
            let Ok(id) = Bson::try_from(raw_id) else { continue };
            if kind == Kind::Update && plan.many && !seen.insert(id_digest(raw_id)) {
                continue;
            }
            ids.push(id);
            docs.push(d);
        }
        if docs.is_empty() {
            continue;
        }
        // Switched to read-only meanwhile: stop before saving what can't be
        // written; the entry holds exactly what was.
        if let Err(e) = conn.writable(plan.what) {
            break End::Stopped(e);
        }
        // The estimate can be low (a filter that selects unusually large
        // documents): stop before a batch that would take the entry past the
        // size limit or the free disk space.
        if let Err(e) = entry.rec.check(&docs) {
            break End::Stopped(e);
        }
        let writer = match entry.rec.writer() {
            Ok(w) => w,
            Err(e) => break End::Failed(e),
        };
        // A recording failure stops before the batch it could not save is written.
        if let Err(e) = writer.save(&docs) {
            break entry.fail(e, true);
        }
        drop(docs);

        let (by_id, many) = by_id(&ids);
        let written = match &plan.write {
            Write::Delete => conn.delete(&plan.ns, by_id.clone(), many).await.map(|deleted| WriteOutcome { deleted, ..Default::default() }),
            Write::Update(spec) => conn.update(&plan.ns, doc! { "$and": [plan.filter.clone(), by_id.clone()] }, spec.clone(), many, false).await,
            Write::Replace(doc) => conn.replace_one(&plan.ns, plan.filter.clone(), doc.clone(), false).await,
        };
        let written = match written {
            Ok(w) if matches!(plan.write, Write::Replace(_)) && w.matched == 0 => break entry.fail(gone(), true),
            Ok(w) => w,
            Err(e) => {
                let refused = changed_nothing(&e, !many);
                // Not refused: the write may have been applied, wholly or in
                // part, so what was saved for it stays and the answer says so
                // (unconfirmed) rather than failing.
                entry.unsure = !refused;
                entry.wrote |= entry.unsure;
                break entry.fail(e, refused);
            }
        };
        entry.wrote = true;
        out.deleted += written.deleted;
        out.matched += written.matched;
        out.modified += written.modified;

        // Hashes only when every saved document was written: if one stopped
        // matching, which ones were is unknown, and without a hash a restore
        // treats each document as changed since.
        if kind == Kind::Update
            && written.matched == ids.len() as u64
            && let Err(e) = entry.rec.after(conn, &ids).await
        {
            break entry.fail(e, false);
        }
        done += ids.len() as u64;
        progress.report(done, Some(total.max(done)), &note);
        reported_total = done >= total;
    };

    let mut warning = None;
    let cancelled = match end {
        End::Done => {
            // The count can be off (documents come and go meanwhile); the last
            // report says what was actually done, and nothing matching ends at 0 / 0.
            progress.finish(done, reported_total, &note);
            false
        }
        End::Cancelled => true,
        // Nothing was written: an error, as unrecorded (a history too large
        // is then offered to go on without it). An entry kept all the same (a
        // write that may have changed some) counts toward the limits.
        End::Stopped(e) | End::Failed(e) if !entry.wrote => {
            let _ = entry.rec.trim(&[]);
            return Err(e);
        }
        End::Stopped(e) => {
            warning = Some(e.to_string());
            true
        }
        End::Failed(e) if entry.unsure => {
            warning = Some(format!("{e}; the documents as they were are kept in Collection History"));
            false
        }
        End::Failed(e) => {
            warning = Some(e.to_string());
            false
        }
    };

    if entry.rec.id().is_none() && !cancelled {
        // Nothing matched: an upsert inserts, as it would unrecorded (an
        // inserted document is not history); a replaced document is gone.
        match &plan.write {
            Write::Update(spec) if plan.upsert => return conn.update(&plan.ns, plan.filter, spec.clone(), plan.many, true).await,
            Write::Replace(_) => return Err(gone()),
            _ => {}
        }
    }
    // Still open only when the batches stopped cleanly; a failure dropped it.
    if let Some(w) = entry.rec.finish(cancelled) {
        warn(&mut warning, w);
    }
    // The write has happened, so a retention problem is a warning, not a failure.
    if let Some(w) = entry.rec.trim(&[]) {
        warn(&mut warning, w);
    }
    out.recorded = entry.rec.id().cloned();
    out.cancelled = cancelled;
    out.warning = warning;
    out.unconfirmed = entry.unsure;
    Ok(out)
}

/// Adds `more` to an outcome's warning.
pub(crate) fn warn(warning: &mut Option<String>, more: String) {
    *warning = Some(match warning.take() {
        Some(first) => format!("{first}; {more}"),
        None => more,
    });
}

/// The warning for retention that left something it could not read or
/// remove after an operation: the history may stay over its limits.
pub(crate) fn trim_warning(e: CoreError) -> String {
    format!("Collection History could not be trimmed: {}", reason(e))
}

/// What the unrecorded `Replace` answers for a document that is not there.
fn gone() -> CoreError {
    CoreError::Other("the document no longer exists".into())
}

/// The filter that addresses a batch's documents, and whether it is many. One
/// document is addressed by `_id` equality and written with `update_one` /
/// `delete_one`, which stay retryable writes (and are targeted by `_id` on a
/// sharded collection).
pub(crate) fn by_id(ids: &[Bson]) -> (Document, bool) {
    match ids {
        [id] => (doc! { "_id": id.clone() }, false),
        _ => (doc! { "_id": { "$in": ids } }, true),
    }
}

/// Server errors that mean a shard or the primary did not answer, or went
/// away during the write: mongos's HostUnreachable (6), HostNotFound (7),
/// NetworkTimeout (89) and SocketException (9001), and ShutdownInProgress
/// (91), InterruptedAtShutdown (11600), PrimarySteppedDown (189),
/// NotWritablePrimary (10107), NotPrimaryNoSecondaryOk (13435),
/// NotPrimaryOrSecondary (13436), InterruptedDueToReplStateChange (11602)
/// and ExceededTimeLimit (262) once the driver's retry failed too: the
/// retryable-writes list, the codes on which a driver can't tell whether a
/// write happened. The write may have been applied.
const NO_ANSWER: [i32; 12] = [6, 7, 89, 9001, 91, 11600, 189, 10107, 13435, 13436, 11602, 262];

/// Whether a failed write certainly changed nothing: refused by the guard or
/// before it ran (authentication, not authorized, a view, an update that
/// doesn't parse (FailedToParse, e.g. a misspelt operator) or whose operators
/// conflict on a path (ConflictingUpdateOperators)), or a refused `single`
/// document, which is written whole or not at all. Anything else may have
/// written: a write-concern error (applied on the primary, not confirmed),
/// no answer (a network error, a timeout, an error the driver can't place,
/// `NO_ANSWER`), or a write of many documents stopped part way.
pub(crate) fn changed_nothing(e: &CoreError, single: bool) -> bool {
    match e {
        CoreError::ReadOnly(_) | CoreError::Auth(_) | CoreError::Server { code: 9 | 13 | 40 | 166, .. } => true,
        CoreError::WriteConcern { .. } => false,
        CoreError::Server { code, .. } if NO_ANSWER.contains(code) => false,
        CoreError::Server { .. } | CoreError::InvalidInput(_) => single,
        _ => false,
    }
}

/// An update document that is not all update operators is refused whole, by
/// the driver or the server; refused here before anything is read or saved.
fn check_update(update: &Document) -> CoreResult<()> {
    if update.is_empty() || !update.keys().all(|k| k.starts_with('$')) {
        return Err(CoreError::InvalidInput("an update document needs update operators ($set, $unset, …)".into()));
    }
    Ok(())
}

/// The collection's average document size, from its statistics; 0 when they
/// can't be read (a view, a missing collection, no privilege), which makes
/// the estimate 0.
pub(crate) async fn avg_obj_size(conn: &Conn, ns: &Namespace) -> u64 {
    let Ok(stats) = conn.coll_stats(ns).await else { return 0 };
    let src = stats.get_document("storageStats").unwrap_or(&stats);
    match src.get("avgObjSize") {
        Some(Bson::Int32(n)) => (*n).max(0) as u64,
        Some(Bson::Int64(n)) => (*n).max(0) as u64,
        Some(Bson::Double(f)) if *f > 0.0 => f.ceil() as u64,
        _ => 0,
    }
}

/// Refuses an operation whose history would not fit: more than the size limit
/// or than the free disk space. Older entries make room for a new one, so
/// what is kept already does not count.
pub(crate) fn check_room(store: &Store, limit: u64, needed: u64) -> CoreResult<()> {
    if needed == 0 {
        return Ok(());
    }
    let free = store.free_bytes()?;
    if needed > limit || needed > free {
        return Err(CoreError::HistoryTooLarge { needed, limit, free });
    }
    Ok(())
}

/// What a saved document takes on disk beyond its own bytes: the record's
/// framing and encryption, and the hash recorded after it is changed.
pub(crate) const RECORD_OVERHEAD: u64 = 128;

/// The running check that backs up the estimate (`check_room`), which can
/// be low: refuses saving `befores` when that would take an entry of
/// `so_far` bytes past the size limit or the free disk space.
pub(crate) fn check_growth<'a>(store: &Store, limit: u64, so_far: u64, befores: impl IntoIterator<Item = &'a RawDocumentBuf>) -> CoreResult<()> {
    let adding: u64 = befores.into_iter().map(|d| d.as_bytes().len() as u64 + RECORD_OVERHEAD).sum();
    if adding == 0 {
        return Ok(());
    }
    let needed = so_far + adding;
    let free = store.free_bytes()?.saturating_add(so_far);
    if needed > limit || needed > free {
        return Err(CoreError::HistoryTooLarge { needed, limit, free });
    }
    Ok(())
}

/// The hash of each document of a batch as the write left it, read back by `_id`.
pub(crate) async fn after_hashes(conn: &Conn, ns: &Namespace, by_id: Document) -> CoreResult<Vec<(Bson, i64)>> {
    let opts = FindOpts { batch_size: Some(BATCH as u32), ..Default::default() };
    let docs = conn.open_find_raw(ns, by_id, opts).await?.next_batch(usize::MAX).await?;
    let mut out = Vec::with_capacity(docs.len());
    for d in &docs {
        if let Some(id) = d.get("_id").ok().flatten() {
            out.push((Bson::try_from(id)?, doc_hash(d)));
        }
    }
    Ok(out)
}

/// 128 bits of the SHA-256 of an `_id` (its type and bytes): tells documents
/// apart without keeping every `_id` of a large update in memory.
fn id_digest(id: RawBsonRef<'_>) -> u128 {
    let mut d = RawDocumentBuf::new();
    d.append(cstr!("_id"), id);
    let h = digest::digest(&digest::SHA256, d.as_bytes());
    u128::from_le_bytes(h.as_ref()[..16].try_into().expect("a SHA-256 has 32 bytes"))
}

#[cfg(test)]
mod tests {
    // The tests that make files unwritable are Unix only.
    #[cfg(unix)]
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicBool, Ordering};

    use bson::oid::ObjectId;
    use bson::{Bson, Document, RawDocumentBuf, doc};
    use tokio_util::sync::CancellationToken;
    #[cfg(unix)]
    use uuid::Uuid;

    use super::*;
    use crate::api::{Event, UpdateBody};
    use crate::conn::FindOpts;
    use crate::history::{Kind, Saved, doc_hash};
    use crate::testkit::{TempDir, entries, files, mock, ns, raw, record, saved, seed, store};

    /// Every document, in the demo server's (insertion) order.
    async fn all(c: &Conn) -> Vec<Document> {
        c.open_find(&ns(), doc! {}, FindOpts::default()).await.unwrap().next_batch(usize::MAX).await.unwrap()
    }

    async fn recorded(c: &Conn, r: &Record, cmd: Command) -> CoreResult<WriteOutcome> {
        let (progress, _) = Progress::channel();
        run(c, r, cmd, &progress, &CancellationToken::new()).await
    }

    fn ids(docs: &[RawDocumentBuf]) -> Vec<Bson> {
        docs.iter().map(|d| Bson::try_from(d.get("_id").unwrap().unwrap()).unwrap()).collect()
    }

    #[tokio::test]
    async fn deleting_selected_rows_saves_them_first() {
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        let docs: Vec<Document> = (0..5).map(|i| doc! { "_id": i, "name": format!("row {i}") }).collect();
        seed(&c, &docs).await;
        let out = recorded(&c, &r, Command::Delete { ns: ns(), ids: vec![Bson::Int32(1), Bson::Int32(3)] }).await.unwrap();
        assert_eq!((out.deleted, out.cancelled, out.warning.as_deref()), (2, false, None));
        let list = entries(&r);
        assert_eq!(list.len(), 1, "{list:?}");
        let e = &list[0];
        assert_eq!((e.kind, e.count, e.complete, e.conn_name.as_str()), (Kind::Delete, 2, true, "local"));
        assert_eq!(out.recorded.as_ref(), Some(&e.id));
        let befores: Vec<RawDocumentBuf> = saved(&r, &e.id).into_iter().map(|s| s.before).collect();
        assert_eq!(befores, vec![raw(&docs[1]), raw(&docs[3])]);
        assert_eq!(all(&c).await, vec![docs[0].clone(), docs[2].clone(), docs[4].clone()]);
    }

    #[tokio::test]
    async fn filtered_delete_saves_exactly_what_it_deletes() {
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        // Every other one of the first 2,468 is in group 1: 1,234 of 2,500.
        let docs: Vec<Document> = (0..2500).map(|i| doc! { "_id": i, "g": if i % 2 == 0 && i < 2468 { 1 } else { 2 } }).collect();
        seed(&c, &docs).await;
        let (progress, rx) = Progress::channel();
        let out = run(&c, &r, Command::DeleteWhere { ns: ns(), filter: doc! { "g": 1 }, many: true }, &progress, &CancellationToken::new()).await.unwrap();
        assert_eq!((out.deleted, out.cancelled, out.warning.as_deref()), (1234, false, None));
        let list = entries(&r);
        assert_eq!(list.len(), 1);
        assert_eq!((list[0].kind, list[0].count, list[0].complete), (Kind::Delete, 1234, true));
        assert_eq!(out.recorded.as_ref(), Some(&list[0].id));
        let befores: Vec<RawDocumentBuf> = saved(&r, &list[0].id).into_iter().map(|s| s.before).collect();
        let matching: Vec<RawDocumentBuf> = docs.iter().filter(|d| d.get_i32("g").ok() == Some(1)).map(raw).collect();
        assert_eq!(befores, matching, "saved exactly the deleted documents, in order");
        let left = all(&c).await;
        assert_eq!(left.len(), 1266);
        assert!(left.iter().all(|d| d.get_i32("g").ok() == Some(2)));
        let last = rx.try_iter().filter_map(|r| match r.event {
            Event::Progress { done, total, .. } => Some((done, total)),
            _ => None,
        }).last();
        assert_eq!(last, Some((1234, Some(1234))));
    }

    #[tokio::test]
    async fn a_document_that_starts_matching_after_it_was_read_is_kept() {
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        let docs: Vec<Document> = (0..2500).map(|i| doc! { "_id": i, "g": 1 }).collect();
        seed(&c, &docs).await;
        let (late, once) = (c.clone(), AtomicBool::new(false));
        let progress = Progress::with_hook(move |done| {
            if done >= 1000 && !once.swap(true, Ordering::SeqCst) {
                late.with_store(|s| s.insert(&ns(), doc! { "_id": "late", "g": 1 })).unwrap();
            }
        });
        let out = run(&c, &r, Command::DeleteWhere { ns: ns(), filter: doc! { "g": 1 }, many: true }, &progress, &CancellationToken::new()).await.unwrap();
        assert_eq!(out.deleted, 2500);
        assert_eq!(all(&c).await, vec![doc! { "_id": "late", "g": 1 }]);
        let list = entries(&r);
        assert_eq!((list.len(), list[0].count), (1, 2500));
        assert!(saved(&r, &list[0].id).iter().all(|s| s.before.get_str("_id").is_err()), "the late document was not saved");
    }

    #[tokio::test]
    async fn updates_save_before_and_after() {
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        let docs: Vec<Document> = (0..1500).map(|i| doc! { "_id": i, "g": 1, "n": i }).collect();
        seed(&c, &docs).await;
        // After the first batch, document 1200 (read, not yet written) stops matching.
        let (other, once) = (c.clone(), AtomicBool::new(false));
        let progress = Progress::with_hook(move |done| {
            if done >= 1000 && !once.swap(true, Ordering::SeqCst) {
                other.with_store(|s| s.update(&ns(), &doc! { "_id": 1200 }, &doc! { "$set": { "g": 2 } }, false, false)).unwrap();
            }
        });
        let cmd = Command::Update { ns: ns(), filter: doc! { "g": 1 }, update: UpdateBody::Doc(doc! { "$set": { "done": true } }), many: true, upsert: false };
        let out = run(&c, &r, cmd, &progress, &CancellationToken::new()).await.unwrap();
        assert_eq!((out.matched, out.modified), (1499, 1499));
        let now = all(&c).await;
        assert_eq!(now[1200], doc! { "_id": 1200, "g": 2, "n": 1200 }, "stopped matching between read and write: not updated");
        assert!(now.iter().filter(|d| d.get_i32("_id").ok() != Some(1200)).all(|d| d.get_bool("done").ok() == Some(true)));

        let list = entries(&r);
        assert_eq!(list.len(), 1);
        assert_eq!((list[0].kind, list[0].count, list[0].complete), (Kind::Update, 1500, true));
        assert_eq!(out.recorded.as_ref(), Some(&list[0].id));
        let saved = saved(&r, &list[0].id);
        let befores: Vec<RawDocumentBuf> = saved.iter().map(|s| s.before.clone()).collect();
        assert_eq!(befores, docs.iter().map(raw).collect::<Vec<_>>(), "before-images are the documents as they were read");
        // The first batch: each hash is of the document as the update left it.
        for (s, d) in saved.iter().zip(&now).take(1000) {
            assert_eq!(s.after_hash, Some(doc_hash(&raw(d))));
        }
        // The second batch had a document that was not written, so which were
        // is unknown: no hashes, and a restore treats each as changed since.
        assert!(saved[1000..].iter().all(|s| s.after_hash.is_none()));
    }

    #[tokio::test]
    async fn edit_document_and_field_actions_record() {
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        let oid = ObjectId::new();
        let docs = [doc! { "_id": oid, "name": "a", "n": 1 }, doc! { "_id": "x", "name": "b", "n": 2 }];
        seed(&c, &docs).await;

        let out = recorded(&c, &r, Command::Replace { ns: ns(), id: Bson::ObjectId(oid), doc: doc! { "name": "edited" } }).await.unwrap();
        assert_eq!((out.matched, out.modified), (1, 1));
        let replaced = out.recorded.expect("recorded");
        let out = recorded(&c, &r, Command::UpdateMany { ns: ns(), filter: doc! { "_id": "x" }, update: doc! { "$unset": { "n": "" } } }).await.unwrap();
        assert_eq!((out.matched, out.modified), (1, 1));
        let unset = out.recorded.expect("recorded");

        let now = all(&c).await;
        assert_eq!(now, vec![doc! { "_id": oid, "name": "edited" }, doc! { "_id": "x", "name": "b" }]);
        assert_eq!(entries(&r).len(), 2);
        for (id, before, after) in [(&replaced, &docs[0], &now[0]), (&unset, &docs[1], &now[1])] {
            assert_eq!(saved(&r, id), vec![Saved { before: raw(before), after_hash: Some(doc_hash(&raw(after))) }]);
        }
        assert!(entries(&r).iter().all(|e| e.kind == Kind::Update && e.count == 1 && e.complete));

        // A document that no longer exists: refused as before, nothing recorded.
        let gone = recorded(&c, &r, Command::Replace { ns: ns(), id: Bson::Int32(404), doc: doc! { "name": "z" } }).await;
        assert!(matches!(gone, Err(CoreError::Other(ref m)) if m.contains("no longer exists")), "{gone:?}");
        assert_eq!(entries(&r).len(), 2);
    }

    #[tokio::test]
    async fn mixed_id_types() {
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        let kinds: Vec<Bson> = vec![
            Bson::ObjectId(ObjectId::new()),
            Bson::String("seven".into()),
            Bson::Int32(7),
            Bson::Int64(70_000_000_000),
            Bson::Double(7.5),
            Bson::Document(doc! { "region": "eu", "seq": 7 }),
        ];
        let mut docs = Vec::new();
        for (i, id) in kinds.iter().enumerate() {
            docs.push(doc! { "_id": id.clone(), "gone": true, "i": i as i32 });
        }
        let keep: Vec<Document> = vec![
            doc! { "_id": ObjectId::new(), "gone": false },
            doc! { "_id": "eight", "gone": false },
            doc! { "_id": 8, "gone": false },
            doc! { "_id": { "region": "eu", "seq": 8 }, "gone": false },
        ];
        docs.extend(keep.iter().cloned());
        seed(&c, &docs).await;

        let out = recorded(&c, &r, Command::DeleteWhere { ns: ns(), filter: doc! { "gone": true }, many: true }).await.unwrap();
        assert_eq!(out.deleted, 6);
        let befores: Vec<RawDocumentBuf> = saved(&r, &out.recorded.unwrap()).into_iter().map(|s| s.before).collect();
        assert_eq!(ids(&befores), kinds);
        assert_eq!(all(&c).await, keep);

        // An update by `_id` batches pairs each result with its document, whatever its type.
        let out = recorded(&c, &r, Command::UpdateMany { ns: ns(), filter: doc! {}, update: doc! { "$set": { "seen": true } } }).await.unwrap();
        assert_eq!(out.modified, 4);
        let now = all(&c).await;
        let saved = saved(&r, &out.recorded.unwrap());
        assert_eq!(saved.iter().map(|s| s.before.clone()).collect::<Vec<_>>(), keep.iter().map(raw).collect::<Vec<_>>());
        for (s, d) in saved.iter().zip(&now) {
            assert_eq!(s.after_hash, Some(doc_hash(&raw(d))), "{d}");
        }
    }

    #[tokio::test]
    async fn matching_nothing_records_nothing() {
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        seed(&c, &[doc! { "_id": 1, "g": 1 }]).await;
        let out = recorded(&c, &r, Command::DeleteWhere { ns: ns(), filter: doc! { "g": 99 }, many: true }).await.unwrap();
        assert_eq!((out.deleted, out.recorded), (0, None));
        let out = recorded(&c, &r, Command::Delete { ns: ns(), ids: vec![] }).await.unwrap();
        assert_eq!((out.deleted, out.recorded), (0, None));
        let cmd = Command::Update { ns: ns(), filter: doc! { "g": 99 }, update: UpdateBody::Doc(doc! { "$set": { "x": 1 } }), many: true, upsert: false };
        let out = recorded(&c, &r, cmd).await.unwrap();
        assert_eq!((out.matched, out.recorded), (0, None));
        assert!(files(&r.root).is_empty(), "{:?}", files(&r.root));

        // An upsert with no match runs as it would unrecorded; the document it creates is not history.
        let cmd = Command::Update { ns: ns(), filter: doc! { "_id": 2 }, update: UpdateBody::Doc(doc! { "$set": { "g": 2 } }), many: false, upsert: true };
        let out = recorded(&c, &r, cmd).await.unwrap();
        assert_eq!((out.inserted_id, out.recorded), (Some(Bson::Int32(2)), None));
        assert_eq!(all(&c).await, vec![doc! { "_id": 1, "g": 1 }, doc! { "_id": 2, "g": 2 }]);
        assert!(files(&r.root).is_empty());

        // An upsert that matches is recorded like any update.
        let cmd = Command::Update { ns: ns(), filter: doc! { "_id": 2 }, update: UpdateBody::Doc(doc! { "$set": { "g": 3 } }), many: false, upsert: true };
        let out = recorded(&c, &r, cmd).await.unwrap();
        assert_eq!((out.matched, out.inserted_id), (1, None));
        assert_eq!(saved(&r, &out.recorded.unwrap()).iter().map(|s| s.before.clone()).collect::<Vec<_>>(), vec![raw(&doc! { "_id": 2, "g": 2 })]);
    }

    #[tokio::test]
    async fn too_large_is_refused_before_writing() {
        let (t, c) = (TempDir::new(), mock());
        let mut r = record(&t);
        let docs: Vec<Document> = (0..100).map(|i| doc! { "_id": i, "pad": "x".repeat(100) }).collect();
        seed(&c, &docs).await;
        let avg = c.coll_stats(&ns()).await.unwrap().get_i64("avgObjSize").unwrap() as u64;
        r.max_bytes = 100 * avg - 1;
        let err = recorded(&c, &r, Command::DeleteWhere { ns: ns(), filter: doc! {}, many: true }).await.unwrap_err();
        assert!(matches!(err, CoreError::HistoryTooLarge { needed, limit, free } if needed == 100 * avg && limit == 100 * avg - 1 && free > 0), "{err:?}");
        let err = recorded(&c, &r, Command::UpdateMany { ns: ns(), filter: doc! {}, update: doc! { "$set": { "x": 1 } } }).await.unwrap_err();
        assert!(matches!(err, CoreError::HistoryTooLarge { .. }), "{err:?}");
        assert_eq!(all(&c).await, docs, "nothing written");
        assert!(files(&r.root).is_empty());
        // The estimate fits, but not what the documents take saved (their
        // records' framing too): the running check refuses the first batch.
        r.max_bytes = 100 * avg;
        let err = recorded(&c, &r, Command::DeleteWhere { ns: ns(), filter: doc! {}, many: true }).await.unwrap_err();
        assert!(matches!(err, CoreError::HistoryTooLarge { needed, .. } if needed >= 100 * (avg + RECORD_OVERHEAD)), "{err:?}");
        assert_eq!(all(&c).await, docs, "nothing written");
        assert!(files(&r.root).is_empty());
        // With room for that it goes ahead.
        r.max_bytes = 100 * (avg + RECORD_OVERHEAD) + 1024;
        let out = recorded(&c, &r, Command::DeleteWhere { ns: ns(), filter: doc! {}, many: true }).await.unwrap();
        assert_eq!(out.deleted, 100);
    }

    /// The estimate is the matching count times the collection's average
    /// size, which a filter selecting unusually large documents outgrows. A
    /// running check stops before a batch that would take the entry past the
    /// size limit: before anything was written, refused as too large (the
    /// app then offers to go on without history); after, the answer is what
    /// was written, stopped there with the reason, and the entry holds
    /// exactly that.
    #[tokio::test]
    async fn an_entry_that_outgrows_its_estimate_stops_before_the_batch() {
        let (t, c) = (TempDir::new(), mock());
        let mut r = record(&t);
        // 1,000 small and then 1,000 large matching documents among 1,000
        // small others: the average, and so the estimate, is too small.
        let docs: Vec<Document> = (0..3000)
            .map(|i| match i {
                0..1000 => doc! { "_id": i, "g": 1 },
                1000..2000 => doc! { "_id": i, "g": 1, "big": true, "pad": "x".repeat(3000) },
                _ => doc! { "_id": i, "g": 2 },
            })
            .collect();
        seed(&c, &docs).await;
        r.max_bytes = 5 << 19;
        let avg = c.coll_stats(&ns()).await.unwrap().get_i64("avgObjSize").unwrap() as u64;
        assert!(2000 * avg < r.max_bytes && 1000 * 3000 > r.max_bytes, "the estimate fits, the large documents don't");
        let big = |d: &Document| d.get_bool("big").is_ok();

        // Only large ones: the first batch would pass the limit; nothing is written.
        let err = recorded(&c, &r, Command::DeleteWhere { ns: ns(), filter: doc! { "big": true }, many: true }).await.unwrap_err();
        assert!(matches!(err, CoreError::HistoryTooLarge { needed, limit, .. } if limit == 5 << 19 && needed > limit), "{err:?}");
        assert_eq!(all(&c).await, docs);
        assert!(files(&r.root).is_empty(), "{:?}", files(&r.root));

        // Small ones first: the first batch is written, then it stops.
        let out = recorded(&c, &r, Command::UpdateMany { ns: ns(), filter: doc! { "g": 1 }, update: doc! { "$set": { "done": true } } }).await.unwrap();
        assert_eq!((out.matched, out.modified, out.cancelled), (1000, 1000, true), "{out:?}");
        assert!(out.warning.as_deref().is_some_and(|w| w.starts_with("Collection History would need")), "{out:?}");
        let now = all(&c).await;
        assert!(now.iter().all(|d| d.get_bool("done").is_ok() == (d.get_i32("g").unwrap() == 1 && !big(d))), "only the first batch was updated");
        let update = entries(&r).into_iter().find(|e| Some(&e.id) == out.recorded.as_ref()).unwrap();
        assert_eq!((update.kind, update.count, update.complete, update.cancelled), (Kind::Update, 1000, true, true));

        let out = recorded(&c, &r, Command::DeleteWhere { ns: ns(), filter: doc! { "g": 1 }, many: true }).await.unwrap();
        assert_eq!((out.deleted, out.cancelled), (1000, true), "{out:?}");
        assert!(out.warning.as_deref().is_some_and(|w| w.starts_with("Collection History would need")), "{out:?}");
        let left = all(&c).await;
        assert_eq!(left.len(), 2000);
        assert!(left.iter().filter(|d| d.get_i32("g").unwrap() == 1).all(big), "the large ones are still there");
        let delete = entries(&r).into_iter().find(|e| Some(&e.id) == out.recorded.as_ref()).unwrap();
        assert_eq!((delete.kind, delete.count, delete.complete, delete.cancelled), (Kind::Delete, 1000, true, true));
        assert!(saved(&r, &delete.id).iter().all(|s| s.before.get_bool("big").is_err()), "exactly the deleted documents");
    }

    /// Two operations at once: the one that ends first trims the history to
    /// the limit, protecting only its own entry, and must not remove the
    /// other's while that is still being written.
    #[tokio::test]
    async fn another_operations_retention_leaves_an_entry_being_written() {
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        let docs: Vec<Document> = (0..2500).map(|i| doc! { "_id": i }).collect();
        seed(&c, &docs).await;
        let (other, once) = (store(&r), AtomicBool::new(false));
        let progress = Progress::with_hook(move |done| {
            if done >= 1000 && !once.swap(true, Ordering::SeqCst) {
                other.purge(0, 0, &[]).unwrap();
            }
        });
        let out = run(&c, &r, Command::DeleteWhere { ns: ns(), filter: doc! {}, many: true }, &progress, &CancellationToken::new()).await.unwrap();
        assert_eq!((out.deleted, out.warning.as_deref()), (2500, None));
        let list = entries(&r);
        assert_eq!((list.len(), list[0].count, list[0].complete), (1, 2500, true), "{list:?}");
        assert_eq!(saved(&r, &list[0].id).len(), 2500, "every before-image is still there");
    }

    #[tokio::test]
    async fn cancel_and_read_only_stop_cleanly() {
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        let docs: Vec<Document> = (0..2500).map(|i| doc! { "_id": i }).collect();
        seed(&c, &docs).await;
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let progress = Progress::with_hook(move |_| token.cancel());
        let out = run(&c, &r, Command::DeleteWhere { ns: ns(), filter: doc! {}, many: true }, &progress, &cancel).await.unwrap();
        assert_eq!((out.deleted, out.cancelled, out.warning.as_deref()), (1000, true, None));
        assert_eq!(all(&c).await, docs[1000..]);
        let list = entries(&r);
        assert_eq!((list.len(), list[0].count, list[0].complete), (1, 1000, true));
        assert_eq!(out.recorded.as_ref(), Some(&list[0].id));
        let befores: Vec<RawDocumentBuf> = saved(&r, &list[0].id).into_iter().map(|s| s.before).collect();
        assert_eq!(befores, docs[..1000].iter().map(raw).collect::<Vec<_>>());

        // Read-only: refused before anything is read or saved.
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        seed(&c, &docs[..10]).await;
        c.set_read_only(true);
        for cmd in [
            Command::DeleteWhere { ns: ns(), filter: doc! {}, many: true },
            Command::Delete { ns: ns(), ids: vec![Bson::Int32(1)] },
            Command::Replace { ns: ns(), id: Bson::Int32(1), doc: doc! { "a": 1 } },
            Command::UpdateMany { ns: ns(), filter: doc! {}, update: doc! { "$set": { "a": 1 } } },
        ] {
            let err = recorded(&c, &r, cmd).await.unwrap_err();
            assert!(matches!(err, CoreError::ReadOnly(_)), "{err:?}");
        }
        c.set_read_only(false);
        assert_eq!(all(&c).await, docs[..10]);
        assert!(files(&t.0).is_empty(), "{:?}", files(&t.0));

        // Switched to read-only after the first batch: stops there like a
        // cancel, says why, and the entry holds exactly what was deleted.
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        seed(&c, &docs).await;
        let lock = c.clone();
        let progress = Progress::with_hook(move |_| lock.set_read_only(true));
        let out = run(&c, &r, Command::DeleteWhere { ns: ns(), filter: doc! {}, many: true }, &progress, &CancellationToken::new()).await.unwrap();
        assert_eq!((out.deleted, out.cancelled), (1000, true));
        assert!(out.warning.as_deref().is_some_and(|w| w.contains("read-only")), "{out:?}");
        c.set_read_only(false);
        assert_eq!(all(&c).await, docs[1000..]);
        let list = entries(&r);
        assert_eq!((list.len(), list[0].count, list[0].complete), (1, 1000, true));
        assert_eq!(out.recorded.as_ref(), Some(&list[0].id));
    }

    #[tokio::test]
    async fn a_refused_first_write_leaves_no_entry() {
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        let docs = [doc! { "_id": 1, "n": 1 }, doc! { "_id": 2, "n": 2 }];
        seed(&c, &docs).await;
        // An update document without operators: refused before anything is read.
        for update in [doc! { "n": 5 }, doc! { "$set": { "n": 5 }, "m": 1 }, doc! {}] {
            let err = recorded(&c, &r, Command::UpdateMany { ns: ns(), filter: doc! {}, update }).await.unwrap_err();
            assert!(matches!(err, CoreError::InvalidInput(_)), "{err:?}");
        }
        assert!(files(&t.0).is_empty(), "{:?}", files(&t.0));
        // A misspelt operator: the server refuses the whole update before it runs
        // (FailedToParse), so the batch saved for it is removed again.
        let err = recorded(&c, &r, Command::UpdateMany { ns: ns(), filter: doc! {}, update: doc! { "$sett": { "n": 5 } } }).await.unwrap_err();
        assert!(matches!(err, CoreError::Server { code: 9, .. }), "{err:?}");
        assert!(files(&r.root).iter().all(|f| f.extension().is_none_or(|e| e != "rhist")), "{:?}", files(&r.root));
        // The demo server refuses pipeline updates; a single document is written
        // whole or not at all, so its saved before-image is removed again.
        let cmd = Command::Update { ns: ns(), filter: doc! { "_id": 1 }, update: UpdateBody::Pipeline(vec![doc! { "$set": { "n": 2 } }]), many: true, upsert: false };
        let err = recorded(&c, &r, cmd).await.unwrap_err();
        assert!(matches!(err, CoreError::InvalidInput(_)), "{err:?}");
        assert_eq!(all(&c).await, docs);
        assert!(files(&r.root).iter().all(|f| f.extension().is_none_or(|e| e != "rhist")), "{:?}", files(&r.root));
        assert!(entries(&r).is_empty());
    }

    #[tokio::test]
    async fn a_write_that_fails_part_way_keeps_its_entry() {
        // `$inc` stops at the first non-number; the documents before it were
        // changed (as on a real server), so their before-images must stay, and
        // the answer is unconfirmed rather than an error: a retry would
        // apply the `$inc` twice to those.
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        let docs = [doc! { "_id": 1, "n": 1 }, doc! { "_id": 2, "n": 2 }, doc! { "_id": 3, "n": "three" }, doc! { "_id": 4, "n": 4 }];
        seed(&c, &docs).await;
        let out = recorded(&c, &r, Command::UpdateMany { ns: ns(), filter: doc! {}, update: doc! { "$inc": { "n": 10 } } }).await.unwrap();
        assert_eq!((out.matched, out.modified, out.cancelled, out.unconfirmed), (0, 0, false, true), "{out:?}");
        assert!(out.warning.as_deref().is_some_and(|w| w.contains("non-numeric")), "{out:?}");
        assert_eq!(all(&c).await[..2], [doc! { "_id": 1, "n": 11 }, doc! { "_id": 2, "n": 12 }]);
        let list = entries(&r);
        assert_eq!((list.len(), list[0].count, list[0].complete), (1, 4, false), "{list:?}");
        assert_eq!(out.recorded.as_ref(), Some(&list[0].id));
        let befores: Vec<RawDocumentBuf> = saved(&r, &list[0].id).into_iter().map(|s| s.before).collect();
        assert_eq!(befores, docs.iter().map(raw).collect::<Vec<_>>());

        // The same in a later batch: the batches before it were written, so the
        // answer is what they did, with the error as a warning.
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        let docs: Vec<Document> = (0..1500).map(|i| if i == 1200 { doc! { "_id": i, "n": "x" } } else { doc! { "_id": i, "n": i } }).collect();
        seed(&c, &docs).await;
        let out = recorded(&c, &r, Command::UpdateMany { ns: ns(), filter: doc! {}, update: doc! { "$inc": { "n": 10 } } }).await.unwrap();
        assert_eq!((out.matched, out.modified, out.cancelled, out.unconfirmed), (1000, 1000, false, true));
        assert!(out.warning.as_deref().is_some_and(|w| w.contains("non-numeric")), "{out:?}");
        let list = entries(&r);
        assert_eq!((list.len(), list[0].count, list[0].complete), (1, 1500, false), "{list:?}");
        assert_eq!(out.recorded.as_ref(), Some(&list[0].id));
    }

    /// The codes a driver retries a write on (the retryable-writes list)
    /// leave its outcome unknown, and a write-concern error comes after the
    /// primary applied it: never "changed nothing", even for one document.
    #[test]
    fn no_answer_and_write_concern_errors_are_never_refusals() {
        for code in [6, 7, 89, 9001, 91, 189, 10107, 11602, 11600, 13435, 13436, 262] {
            assert!(!changed_nothing(&CoreError::Server { code, msg: String::new() }, true), "{code}");
        }
        assert!(!changed_nothing(&CoreError::WriteConcern { code: 64, msg: String::new() }, true));
        assert!(changed_nothing(&CoreError::Server { code: 121, msg: String::new() }, true));
    }

    /// A recorded write answered with a write-concern error was applied on
    /// the primary: its entry is kept and the answer is unconfirmed.
    #[tokio::test]
    async fn a_write_concern_error_keeps_the_entry() {
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        seed(&c, &[doc! { "_id": 1, "n": 1 }]).await;
        c.lose_next_reply_with(CoreError::WriteConcern { code: 64, msg: "waiting for replication timed out".into() });
        let out = recorded(&c, &r, Command::Replace { ns: ns(), id: Bson::Int32(1), doc: doc! { "n": 2 } }).await.unwrap();
        assert!(out.unconfirmed && out.warning.as_deref().is_some_and(|w| w.contains("write concern error 64")), "{out:?}");
        assert_eq!(entries(&r).len(), 1);
    }

    /// A write whose reply is lost (the connection dropped after the server
    /// applied it) may have changed what was saved for it: the entry is kept
    /// and the answer is what is known, with the reason as a warning, not an
    /// error that invites a retry (which would apply an `$inc` twice).
    #[tokio::test]
    async fn a_write_whose_outcome_is_unknown_keeps_its_entry_and_warns() {
        let docs: Vec<Document> = (0..3).map(|i| doc! { "_id": i, "n": i }).collect();
        for (cmd, saved_docs) in [
            (Command::UpdateMany { ns: ns(), filter: doc! {}, update: doc! { "$inc": { "n": 10 } } }, &docs[..]),
            (Command::DeleteWhere { ns: ns(), filter: doc! {}, many: true }, &docs[..]),
            (Command::Delete { ns: ns(), ids: vec![Bson::Int32(1)] }, &docs[1..2]),
            (Command::Replace { ns: ns(), id: Bson::Int32(1), doc: doc! { "n": "replaced" } }, &docs[1..2]),
        ] {
            let (t, c) = (TempDir::new(), mock());
            let r = record(&t);
            seed(&c, &docs).await;
            c.lose_next_reply();
            let what = format!("{cmd:?}");
            let out = recorded(&c, &r, cmd).await.unwrap_or_else(|e| panic!("{what}: {e:?}"));
            assert!(out.warning.as_deref().is_some_and(|w| w.contains("network error")), "{what}: {out:?}");
            assert!(out.unconfirmed && !out.cancelled, "{what}: {out:?}");
            let list = entries(&r);
            assert_eq!(list.len(), 1, "{what}");
            assert_eq!(out.recorded.as_ref(), Some(&list[0].id), "{what}");
            let befores: Vec<RawDocumentBuf> = saved(&r, &list[0].id).into_iter().map(|s| s.before).collect();
            assert_eq!(befores, saved_docs.iter().map(raw).collect::<Vec<_>>(), "{what}");
        }
        // A server error that means no answer from the primary (a step-down,
        // say), even on one document: unconfirmed, not a refusal.
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        seed(&c, &docs).await;
        c.lose_next_reply_with(CoreError::Server { code: 91, msg: "ShutdownInProgress".into() });
        let out = recorded(&c, &r, Command::Replace { ns: ns(), id: Bson::Int32(1), doc: doc! { "n": "replaced" } }).await.unwrap();
        assert!(out.unconfirmed && out.warning.as_deref().is_some_and(|w| w.contains("ShutdownInProgress")), "{out:?}");
        assert_eq!(entries(&r).len(), 1);
        // A refusal is still an error, and leaves no entry.
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        seed(&c, &docs).await;
        let err = recorded(&c, &r, Command::UpdateMany { ns: ns(), filter: doc! {}, update: doc! { "$sett": { "n": 1 } } }).await.unwrap_err();
        assert!(matches!(err, CoreError::Server { code: 9, .. }), "{err:?}");
        assert!(entries(&r).is_empty());
    }

    #[test]
    fn what_a_failed_write_may_have_changed() {
        let server = |code| CoreError::Server { code, msg: String::new() };
        // Refused before anything ran: the guard, not authorized, a view, a
        // misspelt operator (FailedToParse), operators on one path (ConflictingUpdateOperators).
        for e in [CoreError::ReadOnly("delete".into()), server(13), server(166), server(9), server(40)] {
            assert!(changed_nothing(&e, false), "{e:?}");
        }
        // Per-document failures: only a single document is known to be untouched.
        for e in [server(14), server(121), CoreError::InvalidInput("x".into())] {
            assert!(!changed_nothing(&e, false) && changed_nothing(&e, true), "{e:?}");
        }
        assert!(!changed_nothing(&CoreError::Network("reset".into()), true));
        // No answer: it may have been applied, even to one document. That is
        // a network error, a timeout, an error the driver can't place (a
        // write concern's), and the server errors that mean a shard or the
        // primary did not answer (mongos 6, 7, 89, 9001; a step-down or
        // shutdown 91, 189, 10107, 11602).
        for e in [CoreError::Network("reset".into()), CoreError::Timeout("x".into()), CoreError::Other("write concern".into())] {
            assert!(!changed_nothing(&e, true), "{e:?}");
        }
        for code in [6, 7, 89, 9001, 91, 189, 10107, 11602] {
            assert!(!changed_nothing(&server(code), true), "{code}");
        }
        // An authentication failure refuses before anything runs.
        assert!(changed_nothing(&CoreError::Auth("x".into()), false));
    }

    /// The header names a selected-rows delete by its command and count: the
    /// `_id`s are in the before-images already, and a header holding 100,000
    /// of them would be decrypted and parsed at every listing.
    #[test]
    fn a_selected_rows_delete_keeps_its_count_not_its_ids() {
        let plan = Plan::new(Command::Delete { ns: ns(), ids: (0..100_000).map(Bson::Int32).collect() }).unwrap();
        assert_eq!(plan.how, doc! { "command": "delete", "count": 100_000_i64 });
        assert_eq!(plan.filter.get_document("_id").unwrap().get_array("$in").unwrap().len(), 100_000, "still deletes them all");
        // Filters are kept: they are small.
        let plan = Plan::new(Command::DeleteWhere { ns: ns(), filter: doc! { "g": 1 }, many: true }).unwrap();
        assert_eq!(plan.how, doc! { "command": "deleteWhere", "filter": { "g": 1 }, "many": true });
        // The Delete and Update dialogs' "Selected Document(s)" query is the
        // same list of `_id`s: kept as its count.
        let selected = doc! { "_id": { "$in": (0..100_000).map(Bson::Int32).collect::<Vec<_>>() } };
        let plan = Plan::new(Command::DeleteWhere { ns: ns(), filter: selected.clone(), many: true }).unwrap();
        assert_eq!(plan.how, doc! { "command": "deleteWhere", "count": 100_000_i64, "many": true });
        assert_eq!(plan.filter, selected, "still deletes exactly those");
        let plan = Plan::new(Command::UpdateMany { ns: ns(), filter: selected.clone(), update: doc! { "$set": { "x": 1 } } }).unwrap();
        assert_eq!(plan.how, doc! { "command": "updateMany", "count": 100_000_i64, "update": { "$set": { "x": 1 } } });
        let cmd = Command::Update { ns: ns(), filter: selected.clone(), update: UpdateBody::Doc(doc! { "$set": { "x": 1 } }), many: true, upsert: false };
        assert_eq!(Plan::new(cmd).unwrap().how, doc! { "command": "update", "count": 100_000_i64, "update": { "$set": { "x": 1 } }, "many": true, "upsert": false });
        // A filter that says more than the `_id`s is kept as it is.
        let narrower = doc! { "_id": { "$in": [1, 2] }, "g": 1 };
        assert_eq!(Plan::new(Command::DeleteWhere { ns: ns(), filter: narrower.clone(), many: true }).unwrap().how, doc! { "command": "deleteWhere", "filter": narrower, "many": true });
    }

    #[test]
    fn one_document_is_written_as_one() {
        // `update_one` / `delete_one` stay retryable writes and target by `_id` equality.
        assert_eq!(by_id(&[Bson::Int32(7)]), (doc! { "_id": 7 }, false));
        assert_eq!(by_id(&[Bson::Int32(7), Bson::String("x".into())]), (doc! { "_id": { "$in": [7, "x"] } }, true));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_full_disk_stops_before_the_unsaved_batch() {
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        let docs: Vec<Document> = (0..2500).map(|i| doc! { "_id": i }).collect();
        seed(&c, &docs).await;
        // After the first batch the entry's file can no longer be written.
        let (root, once) = (r.root.clone(), AtomicBool::new(false));
        let progress = Progress::with_hook(move |done| {
            if done >= 1000 && !once.swap(true, Ordering::SeqCst) {
                for f in files(&root).into_iter().filter(|f| f.extension().is_some_and(|e| e == "rhist")) {
                    fs::set_permissions(&f, fs::Permissions::from_mode(0o400)).unwrap();
                }
            }
        });
        // Something was deleted, so the answer is what was, with the reason it stopped.
        let out = run(&c, &r, Command::DeleteWhere { ns: ns(), filter: doc! {}, many: true }, &progress, &CancellationToken::new()).await.unwrap();
        assert_eq!((out.deleted, out.cancelled), (1000, false));
        assert!(out.warning.as_deref().is_some_and(|w| w.starts_with("Collection History:")), "{out:?}");
        assert_eq!(all(&c).await, docs[1000..], "exactly the saved batch was deleted");
        let list = entries(&r);
        assert_eq!((list.len(), list[0].count, list[0].complete), (1, 1000, false), "what was deleted stays in history");
        assert_eq!(out.recorded.as_ref(), Some(&list[0].id));
        let befores: Vec<RawDocumentBuf> = saved(&r, &list[0].id).into_iter().map(|s| s.before).collect();
        assert_eq!(befores, docs[..1000].iter().map(raw).collect::<Vec<_>>());

        // A full disk before anything was written is an error, and leaves no entry.
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        seed(&c, &docs).await;
        fs::create_dir_all(&r.root).unwrap();
        fs::set_permissions(&r.root, fs::Permissions::from_mode(0o500)).unwrap();
        let err = recorded(&c, &r, Command::DeleteWhere { ns: ns(), filter: doc! {}, many: true }).await.unwrap_err();
        fs::set_permissions(&r.root, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(err, CoreError::History(_)), "{err:?}");
        assert_eq!(all(&c).await, docs);
        assert!(files(&r.root).is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_finish_still_answers_written() {
        // Every batch was saved and written; only the end record can't be. The
        // answer is the write (retrying could apply an update twice), with a
        // warning, and retention still runs.
        let (t, c) = (TempDir::new(), mock());
        let mut r = record(&t);
        let docs: Vec<Document> = (0..1500).map(|i| doc! { "_id": i, "n": i }).collect();
        seed(&c, &docs).await;
        seed(&c, &[doc! { "_id": "old", "n": -1 }]).await;
        let old = recorded(&c, &r, Command::Delete { ns: ns(), ids: vec![Bson::String("old".into())] }).await.unwrap().recorded.unwrap();
        let cmd = Command::UpdateMany { ns: ns(), filter: doc! {}, update: doc! { "$inc": { "n": 1 } } };
        // Room for this update's entry, not for both entries: the older one
        // goes. Measured by the same update elsewhere.
        let (t2, c2) = (TempDir::new(), mock());
        let r2 = record(&t2);
        seed(&c2, &docs).await;
        recorded(&c2, &r2, cmd.clone()).await.unwrap();
        r.max_bytes = entries(&r2)[0].bytes;
        let (root, keep) = (r.root.clone(), old.file.clone());
        let progress = Progress::with_hook(move |done| {
            if done == 1500 {
                for f in files(&root).into_iter().filter(|f| f.extension().is_some_and(|e| e == "rhist") && !f.ends_with(&keep)) {
                    fs::set_permissions(&f, fs::Permissions::from_mode(0o400)).unwrap();
                }
            }
        });
        let out = run(&c, &r, cmd, &progress, &CancellationToken::new()).await.unwrap();
        assert_eq!((out.matched, out.modified, out.cancelled), (1500, 1500, false));
        assert!(out.warning.as_deref().is_some_and(|w| w.starts_with("Collection History may be incomplete: ")), "{out:?}");
        let list = entries(&r);
        assert_eq!(list.len(), 1, "the older entry was purged: {list:?}");
        assert_eq!((Some(&list[0].id), list[0].count, list[0].complete), (out.recorded.as_ref(), 1500, false));
        assert!(all(&c).await.iter().zip(&docs).all(|(now, was)| now.get_i32("n").unwrap() == was.get_i32("n").unwrap() + 1));
    }

    /// What a one-document update (Edit Value) takes, history included.
    /// `cargo test -p rongo-core --release --lib -- --ignored --nocapture report_single_update_time`
    #[tokio::test]
    #[ignore = "timing; run explicitly in release mode"]
    async fn report_single_update_time() {
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        let n = 50;
        seed(&c, &(0..n).map(|i| doc! { "_id": i, "n": i }).collect::<Vec<_>>()).await;
        let t0 = std::time::Instant::now();
        for i in 0..n {
            recorded(&c, &r, Command::UpdateMany { ns: ns(), filter: doc! { "_id": i }, update: doc! { "$set": { "n": -1 } } }).await.unwrap();
        }
        println!("one-document recorded update: {:?} each ({n} runs)", t0.elapsed() / n as u32);
    }

    /// Retention that can't read part of the history says so on the answer:
    /// the write went through, and the history may stay over its limits.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_retention_problem_is_a_warning_on_the_answer() {
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        seed(&c, &[doc! { "_id": 1 }, doc! { "_id": 2 }]).await;
        // Another profile's history that can't be read.
        let closed = r.root.join(Uuid::from_u128(7).to_string());
        fs::create_dir_all(&closed).unwrap();
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).unwrap();
        let out = recorded(&c, &r, Command::Delete { ns: ns(), ids: vec![Bson::Int32(1)] }).await;
        fs::set_permissions(&closed, fs::Permissions::from_mode(0o700)).unwrap();
        let out = out.unwrap();
        assert_eq!((out.deleted, out.cancelled), (1, false));
        assert!(out.warning.as_deref().is_some_and(|w| w.starts_with("Collection History could not be trimmed: ") && w.contains("ermission denied")), "{out:?}");
        assert_eq!(out.recorded.as_ref(), Some(&entries(&r)[0].id));
    }

    /// `Command::recorded_target`, which the app asks before sending a
    /// write as `Recorded`, names exactly the commands a plan is made for,
    /// and the kind of entry its write keeps (from the plan's write, not
    /// its `kind`, which is `recorded_target`'s own).
    #[test]
    fn recorded_target_names_what_can_be_recorded() {
        let update = UpdateBody::Doc(doc! { "$set": { "x": 1 } });
        let cmds = [
            Command::Delete { ns: ns(), ids: vec![Bson::Int32(1)] },
            Command::DeleteWhere { ns: ns(), filter: doc! {}, many: true },
            Command::Replace { ns: ns(), id: Bson::Int32(1), doc: doc! {} },
            Command::UpdateMany { ns: ns(), filter: doc! {}, update: doc! { "$set": { "x": 1 } } },
            Command::Update { ns: ns(), filter: doc! {}, update, many: true, upsert: false },
            Command::Insert { ns: ns(), doc: doc! {} },
            Command::InsertMany { ns: ns(), docs: vec![doc! {}] },
            Command::ClearCollection { ns: ns() },
            Command::DropCollection { ns: ns() },
        ];
        for cmd in cmds {
            let target = cmd.recorded_target().map(|(ns, kind)| (ns.clone(), kind));
            let what = format!("{cmd:?}");
            let written = |w: &Write| match w {
                Write::Delete => Kind::Delete,
                Write::Update(_) | Write::Replace(_) => Kind::Update,
            };
            let plan = Plan::new(cmd).ok().map(|p| (p.ns.clone(), written(&p.write)));
            assert_eq!(target, plan, "{what}");
        }
    }

    #[tokio::test]
    async fn only_deletes_and_updates_are_recorded() {
        let (t, c) = (TempDir::new(), mock());
        let r = record(&t);
        for cmd in [Command::Insert { ns: ns(), doc: doc! {} }, Command::ClearCollection { ns: ns() }, Command::Recorded { record: r.clone(), cmd: Box::new(Command::Insert { ns: ns(), doc: doc! {} }) }] {
            let err = recorded(&c, &r, cmd).await.unwrap_err();
            assert!(matches!(err, CoreError::InvalidInput(_)), "{err:?}");
        }
        assert!(all(&c).await.is_empty());
    }
}
