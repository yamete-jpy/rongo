//! Copy collections between databases and connections (F2): documents are
//! streamed in batches, a missing target is created from the source's
//! `listCollections` options, views are recreated as views, and indexes
//! follow the documents. Documents travel as BSON, never as shell text.
//!
//! With `CopyOptions::history`, Append + Overwrite saves the target documents
//! it replaces in Collection History (one update entry per item, under the
//! target namespace): each batch's colliding documents are looked up, saved,
//! and replaced by the exact version saved (`restore::guard`), so a target
//! that changed meanwhile is saved again rather than overwritten unsaved.

use std::collections::HashMap;

use bson::{Bson, Document, RawDocumentBuf, doc};
use tokio_util::sync::CancellationToken;

use crate::api::{CopiedItem, CopyItem, CopyOptions, CopyReport, IfExists, Namespace, OnDuplicateId, Record};
use crate::conn::{Conn, FindOpts, copyable_indexes};
use crate::engine::{Progress, check_collection_name};
use crate::error::{CoreError, CoreResult};
use crate::format::{Style, to_shell};
use crate::history::{EntryId, Kind};
use crate::recorded::{Recorder, avg_obj_size, changed_nothing, check_room, warn};
use crate::restore::{ATTEMPTS, guard, id_key_of_bson, lookup, windowed};
use crate::transfer::BATCH;

/// Copy `items` from `src` into `dst` (`same_connection` when they are one
/// connection). Each item that fails records its error and the rest still
/// run. The token is checked between batches, so a cancelled copy reports
/// exactly what it wrote; items after it are not started.
pub(crate) async fn copy_collections(
    src: &Conn,
    dst: &Conn,
    same_connection: bool,
    items: &[CopyItem],
    options: CopyOptions,
    progress: &Progress,
    cancel: &CancellationToken,
) -> CoreResult<CopyReport> {
    // Every source as it was before anything is written: a later item's
    // source may be an earlier item's target.
    let sources = Sources::read(src, items).await;
    // A history that would not fit is refused before anything is written.
    if let Some(record) = &options.history {
        check_history_room(src, dst, items, record).await?;
    }
    let copier = Copier { src, dst, same_connection, options, progress, cancel, items, sources: &sources };
    let mut report = CopyReport::default();
    for (k, item) in items.iter().enumerate() {
        if cancel.is_cancelled() {
            report.cancelled = true;
            break;
        }
        let mut out = CopiedItem { src: item.src.clone(), dst: item.dst.clone(), inserted: 0, overwritten: 0, skipped: 0, recorded: None, warning: None, error: None, unconfirmed: false };
        let note = format!("{} ({}/{})", item.dst, k + 1, items.len());
        match copier.item(item, &note, &mut out).await {
            Ok(Outcome::Complete) => {}
            Ok(Outcome::Cancelled) => report.cancelled = true,
            Err(e) => out.error = Some(e.to_string()),
        }
        report.items.push(out);
        if report.cancelled {
            break;
        }
    }
    Ok(report)
}

/// What an Append + Overwrite item can save at most: the documents it can
/// replace (the fewer of the two collections') at the target's average size,
/// 0 when the counts or statistics can't be read.
async fn history_estimate(src: &Conn, dst: &Conn, item: &CopyItem) -> u64 {
    let (Ok(from), Ok(to)) = (src.estimated_count(&item.src).await, dst.estimated_count(&item.dst).await) else { return 0 };
    from.min(to).saturating_mul(avg_obj_size(dst, &item.dst).await)
}

/// Refuses the whole copy when the Overwrite items' history would not fit
/// the size limit or the free disk space (`CoreError::HistoryTooLarge`); the
/// app then offers to paste without history. Older entries make room for a
/// new one, so what is kept already does not count.
async fn check_history_room(src: &Conn, dst: &Conn, items: &[CopyItem], record: &Record) -> CoreResult<()> {
    let mut needed = 0u64;
    for item in items {
        if item.if_exists == IfExists::Append(OnDuplicateId::Overwrite) {
            needed = needed.saturating_add(history_estimate(src, dst, item).await);
        }
    }
    check_room(&record.store(), record.max_bytes, needed)
}

fn also_pasted(item: &CopyItem) -> CoreError {
    CoreError::InvalidInput(format!("{} is also being pasted; paste under a new name", item.dst))
}

/// How an item ended when it did not fail.
enum Outcome {
    Complete,
    /// The token stopped it between batches.
    Cancelled,
}

struct Copier<'a> {
    src: &'a Conn,
    dst: &'a Conn,
    same_connection: bool,
    options: CopyOptions,
    progress: &'a Progress,
    cancel: &'a CancellationToken,
    /// Every item of this copy, for the cross-item checks.
    items: &'a [CopyItem],
    sources: &'a Sources,
}

impl Copier<'_> {
    /// Every refusal of an item, before anything is written: answers its
    /// source and its target (if there is one) as they are now.
    async fn preflight(&self, item: &CopyItem) -> CoreResult<(Found, Option<Found>)> {
        // Writing into another item's source would change what that item
        // then copies: refused before anything is written. Within one
        // connection the names say so (views have no uuid); across two
        // connections to one server, the uuids below say so.
        if self.same_connection && self.items.iter().any(|other| other.src == item.dst && other.src != item.src) {
            return Err(also_pasted(item));
        }
        if item.src.coll.starts_with("system.") {
            return Err(CoreError::InvalidInput("system collections are not copied".into()));
        }
        check_collection_name(&item.dst.coll)?;
        if self.same_connection && item.src == item.dst {
            return Err(CoreError::InvalidInput("a collection can't be pasted into itself".into()));
        }
        if self.dst.is_read_only() {
            return Err(CoreError::ReadOnly(format!("copy into {}", item.dst)));
        }
        let source = self.sources.find(&item.src)?.ok_or_else(|| CoreError::Other("not found".into()))?;
        let target = find(self.dst, &item.dst).await?;
        if let Some(target) = &target {
            // Two profiles can point at one server: the connections differ,
            // the uuid says it is the same collection. Checked before any drop.
            if source.uuid.is_some() && target.uuid == source.uuid {
                return Err(CoreError::InvalidInput("a collection can't be pasted into itself".into()));
            }
            // The same through another item: the target is what that item
            // copies (as it was before this paste wrote anything).
            if target.uuid.is_some() && self.items.iter().any(|o| o.src != item.src && self.sources.uuid(&o.src) == target.uuid) {
                return Err(also_pasted(item));
            }
            // A view pasted over what it reads would drop that, then fail to
            // be a view on itself.
            if let Some(on) = source.view_on() {
                let read = Namespace::new(item.src.db.clone(), on);
                let by_name = self.same_connection && item.dst == read;
                if by_name || (target.uuid.is_some() && self.sources.uuid(&read) == target.uuid) {
                    return Err(CoreError::InvalidInput(format!("{} is what view {} reads from; paste the view under a new name", item.dst, item.src.coll)));
                }
            }
            // Replace drops the target first. Without both uuids there is no
            // telling it from the source (two profiles can share a server), so
            // drop nothing. A view has no uuid and nothing to lose: its
            // definition was read before the drop.
            let views = source.kind() == Some("view") || target.kind() == Some("view");
            if item.if_exists == IfExists::Replace && !views && (source.uuid.is_none() || target.uuid.is_none()) {
                return Err(CoreError::InvalidInput("can't confirm the target isn't the source (a collection id couldn't be read); paste under a new name or drop the target first".into()));
            }
            // Nothing unique to skip or overwrite by, so appending would
            // take every document again.
            if target.kind() == Some("timeseries") && matches!(item.if_exists, IfExists::Append(_)) {
                return Err(CoreError::InvalidInput("can't append to a time series: it has no unique _id to skip or overwrite by; replace it or paste under a new name".into()));
            }
            // A view holds no documents to append and takes none.
            if matches!(item.if_exists, IfExists::Append(_)) {
                if source.kind() == Some("view") {
                    return Err(CoreError::InvalidInput(format!("a view can't be appended to {}; replace it or paste under a new name", item.dst)));
                }
                if target.kind() == Some("view") {
                    return Err(CoreError::InvalidInput(format!("{} is a view, which can't be appended to; replace it or paste under a new name", item.dst)));
                }
            }
        }
        Ok((source, target))
    }

    /// One collection or view, counting into `out` as it goes so the counts
    /// stay exact when it stops with an error.
    async fn item(&self, item: &CopyItem, note: &str, out: &mut CopiedItem) -> CoreResult<Outcome> {
        let (source, target) = self.preflight(item).await?;
        let source_options = source.spec.get_document("options").cloned().unwrap_or_default();
        let mut exists = target.is_some();
        if exists && item.if_exists == IfExists::Replace {
            self.dst.drop_collection(&item.dst).await?;
            exists = false;
        }

        if source.kind() == Some("view") {
            if !exists {
                let definition: Document = source_options.into_iter().filter(|(k, _)| matches!(k.as_str(), "viewOn" | "pipeline" | "collation")).collect();
                self.dst.create_collection_with(&item.dst, definition).await?;
            }
            return Ok(Outcome::Complete);
        }
        // A target created with the source's validator refuses source
        // documents written before that validator existed.
        let gets_validator = !exists && self.options.collection_options && source_options.contains_key("validator");
        if !exists {
            let options = if self.options.collection_options { source_options } else { Document::new() };
            self.dst.create_collection_with(&item.dst, options).await?;
        }
        let documents = self.documents(item, note, out).await.map_err(|e| match e {
            CoreError::Server { code: 121, msg } if gets_validator => {
                CoreError::Server { code: 121, msg: format!("{msg} (the target got the source's validator; paste without collection options to skip it)") }
            }
            e => e,
        });
        if let Outcome::Cancelled = documents? {
            return Ok(Outcome::Cancelled);
        }
        if self.options.indexes {
            let specs = copyable_indexes(self.src.list_indexes(&item.src).await?);
            self.dst.create_indexes(&item.dst, specs).await?;
        }
        Ok(Outcome::Complete)
    }

    /// Stream the documents across in batches, checking the token before each.
    /// An Append + Overwrite item with `options.history` keeps the target
    /// documents it replaces in one Collection History entry, finished when
    /// the item ends.
    async fn documents(&self, item: &CopyItem, note: &str, out: &mut CopiedItem) -> CoreResult<Outcome> {
        let mut recording = match (&self.options.history, item.if_exists) {
            (Some(record), IfExists::Append(OnDuplicateId::Overwrite)) => Some(Recording::new(record, &item.dst, doc! { "command": "copy", "from": item.src.to_string(), "onDuplicateId": "overwrite" })),
            _ => None,
        };
        let result = self.batches(item, note, out, &mut recording).await;
        let Some(recording) = recording else { return result };
        // Only the history may be short or over its limits: a warning, never an error of its own.
        (out.recorded, out.warning) = recording.end(matches!(result, Ok(Outcome::Cancelled)));
        result
    }

    async fn batches(&self, item: &CopyItem, note: &str, out: &mut CopiedItem, recording: &mut Option<Recording<'_>>) -> CoreResult<Outcome> {
        // A dropped-and-recreated target can only meet a duplicate if something
        // else writes to it meanwhile; that ends the item rather than hiding it.
        let on_duplicate = match item.if_exists {
            IfExists::Append(d) => d,
            IfExists::Replace => OnDuplicateId::Stop,
        };
        let total = self.src.estimated_count(&item.src).await.ok();
        let opts = FindOpts { batch_size: Some(BATCH as u32), ..Default::default() };
        let mut cursor = self.src.open_find(&item.src, Document::new(), opts).await?;
        let mut done = 0;
        let mut reported_total = false;
        loop {
            if self.cancel.is_cancelled() {
                return Ok(Outcome::Cancelled);
            }
            let batch = cursor.next_batch(BATCH).await?;
            if batch.is_empty() {
                break;
            }
            // Borrowed by the insert: a duplicate is then taken from the batch, not from a copy of it.
            let mut batch = batch;
            let result = match self.dst.insert_many_unordered(&item.dst, &batch).await {
                Ok(r) => r,
                Err(e) => {
                    note_unconfirmed(out, &e, false);
                    return Err(e);
                }
            };
            out.inserted += result.inserted;
            // A write-concern error: what went in is not confirmed.
            out.unconfirmed |= result.unconfirmed;
            if on_duplicate == OnDuplicateId::Skip {
                out.skipped += result.duplicates.len() as u64;
            }
            // Any other refusal (or a write-concern error) ends the item; the
            // rest of the batch went in and is counted.
            if let Some(e) = result.error {
                return Err(e);
            }
            match on_duplicate {
                OnDuplicateId::Skip => {}
                OnDuplicateId::Overwrite => {
                    if let Some(rec) = recording.as_mut() {
                        let replaces = result.duplicates.into_iter().map(|i| std::mem::take(&mut batch[i])).collect();
                        self.overwrite_recorded(rec, &item.dst, replaces, out).await?;
                    } else {
                        // `WINDOW` at a time; a failure stops issuing more, and
                        // the ones under way finish and count.
                        let docs: Vec<Document> = result.duplicates.into_iter().map(|i| std::mem::take(&mut batch[i])).collect();
                        let (dst, ns) = (self.dst, &item.dst);
                        let results = windowed(docs.len(), |i| {
                            let d = &docs[i];
                            async move { dst.replace_one(ns, doc! { "_id": d.get("_id").cloned().unwrap_or(Bson::Null) }, d.clone(), true).await }
                        })
                        .await;
                        let mut failed = None;
                        for w in results.into_iter().flatten() {
                            match w {
                                // Deleted from the target meanwhile: the upsert inserted it.
                                Ok(w) => if w.inserted_id.is_some() { out.inserted += 1 } else { out.overwritten += 1 },
                                Err(e) => {
                                    note_unconfirmed(out, &e, true);
                                    failed.get_or_insert(e);
                                }
                            }
                        }
                        if let Some(e) = failed {
                            return Err(e);
                        }
                    }
                }
                OnDuplicateId::Stop => {
                    if let Some(&i) = result.duplicates.first() {
                        let id = batch[i].get("_id").cloned().unwrap_or(Bson::Null);
                        return Err(CoreError::Other(format!("duplicate _id {}", to_shell(&id, Style::Display))));
                    }
                }
            }
            done = out.inserted + out.overwritten + out.skipped;
            self.progress.report(done, total, note);
            reported_total = Some(done) == total;
        }
        // The estimate can be off (or missing); the item's last report says
        // what was actually done, and an empty source ends at 0 / 0.
        self.progress.finish(done, reported_total, note);
        Ok(Outcome::Complete)
    }
}

/// The Collection History entry of one item being overwritten into its target.
struct Recording<'a> {
    /// Begun when the first target document is saved: an item that replaces
    /// nothing leaves no entry. Saves stop (before anything is replaced)
    /// when the entry would pass the size limit or the free disk space.
    entry: Recorder<'a>,
    /// A replace went through (or may have): the entry holds a document that changed.
    replaced: bool,
}

impl<'a> Recording<'a> {
    fn new(record: &'a Record, ns: &'a Namespace, how: Document) -> Self {
        Recording { entry: Recorder::new(record, ns, Kind::Update, how), replaced: false }
    }

    /// Ends the entry: finished (`cancelled` or not) when something was
    /// replaced (it stays restorable, whether the item ended cleanly or with
    /// an error); removed when nothing was. Retention then makes room by
    /// removing older entries, never this one. Answers the entry and a
    /// warning when it could not be finished or retention left something.
    fn end(mut self, cancelled: bool) -> (Option<EntryId>, Option<String>) {
        // Never dropped before this: open when begun.
        if !self.entry.is_open() {
            return (None, None);
        }
        if !self.replaced {
            self.entry.stop(true);
            return (None, None);
        }
        let mut warning = self.entry.finish(cancelled);
        if let Some(w) = self.entry.trim(&[]) {
            warn(&mut warning, w);
        }
        (self.entry.id().cloned(), warning)
    }
}

/// Marks the item unconfirmed when a write that failed may have been applied.
fn note_unconfirmed(out: &mut CopiedItem, e: &CoreError, single: bool) {
    out.unconfirmed |= !changed_nothing(e, single);
}

impl Copier<'_> {
    /// Replaces the target documents whose `_id`s collided, keeping each in
    /// the item's entry first. The documents are looked up in one query, saved,
    /// and replaced (`WINDOW` at a time) by the exact version that was saved,
    /// so a target that changed in between is never overwritten unsaved: it is
    /// looked up, saved and replaced again (`ATTEMPTS` times, then the item
    /// stops). A failure stops issuing more; the hashes are recorded for the
    /// documents the replaces went through for.
    async fn overwrite_recorded(&self, rec: &mut Recording<'_>, ns: &Namespace, replaces: Vec<Document>, out: &mut CopiedItem) -> CoreResult<()> {
        let ids: Vec<Bson> = replaces.iter().map(|d| d.get("_id").cloned().unwrap_or(Bson::Null)).collect();
        let mut current = lookup(self.dst, ns, &ids).await?;
        let present: Vec<Option<RawDocumentBuf>> = ids.iter().map(|id| id_key_of_bson(id).and_then(|k| current.remove(&k))).collect();
        // A history that can't be written stops here, before the overwrite.
        rec.entry.save(present.iter().flatten())?;
        let mut late = Vec::new();
        let mut now = Vec::with_capacity(replaces.len());
        for ((doc, id), found) in replaces.into_iter().zip(ids).zip(present) {
            match found {
                Some(found) => now.push((doc, id, found)),
                None => late.push((doc, id)),
            }
        }
        let dst = self.dst;
        let results = windowed(now.len(), |i| {
            let (doc, id, found) = &now[i];
            async move {
                #[cfg(test)]
                crate::restore::before_replace(id);
                dst.replace_one(ns, guard(id, found)?, doc.clone(), false).await
            }
        })
        .await;
        let mut written = Vec::with_capacity(now.len());
        let mut failed = None;
        for ((doc, id, _), result) in now.into_iter().zip(results) {
            match result {
                // Not issued: a replace before it failed.
                None => {}
                Some(Ok(w)) if w.matched > 0 => {
                    rec.replaced = true;
                    out.overwritten += 1;
                    written.push(id);
                }
                Some(Ok(_)) => late.push((doc, id)),
                Some(Err(e)) => {
                    // What may have been written must stay in the entry.
                    rec.replaced |= !changed_nothing(&e, true);
                    note_unconfirmed(out, &e, true);
                    failed.get_or_insert(e);
                }
            }
        }
        if let Some(e) = failed {
            // The ones replaced keep their hashes (best effort).
            let _ = rec.entry.after(self.dst, &written).await;
            return Err(e);
        }
        rec.entry.after(self.dst, &written).await?;
        for (doc, id) in late {
            self.overwrite_late(rec, ns, id, doc, out).await?;
        }
        Ok(())
    }

    /// One replace that matches only `now`, the version that was saved.
    async fn replace_guarded(&self, rec: &mut Recording<'_>, ns: &Namespace, id: &Bson, now: &RawDocumentBuf, doc: Document) -> CoreResult<bool> {
        #[cfg(test)]
        crate::restore::before_replace(id);
        match self.dst.replace_one(ns, guard(id, now)?, doc, false).await {
            Ok(w) => {
                rec.replaced |= w.matched > 0;
                Ok(w.matched > 0)
            }
            Err(e) => {
                // What may have been written must stay in the entry.
                rec.replaced |= !changed_nothing(&e, true);
                Err(e)
            }
        }
    }

    /// A target document that was not as it was looked up: looks it up again,
    /// saves what is there and replaces exactly that, or inserts the source's
    /// when it is gone (never upserting over a version that was not saved).
    async fn overwrite_late(&self, rec: &mut Recording<'_>, ns: &Namespace, id: Bson, doc: Document, out: &mut CopiedItem) -> CoreResult<()> {
        for _ in 0..ATTEMPTS {
            match lookup(self.dst, ns, std::slice::from_ref(&id)).await?.into_values().next() {
                Some(now) => {
                    rec.entry.save(std::slice::from_ref(&now))?;
                    match self.replace_guarded(rec, ns, &id, &now, doc.clone()).await {
                        Ok(true) => {
                            out.overwritten += 1;
                            return rec.entry.after(self.dst, std::slice::from_ref(&id)).await;
                        }
                        Ok(false) => {}
                        Err(e) => {
                            note_unconfirmed(out, &e, true);
                            return Err(e);
                        }
                    }
                }
                None => match self.dst.insert_one(ns, doc.clone()).await {
                    Ok(_) => {
                        out.inserted += 1;
                        return Ok(());
                    }
                    // It appeared meanwhile: look again.
                    Err(CoreError::Server { code: 11000, .. }) => {}
                    Err(e) => {
                        note_unconfirmed(out, &e, true);
                        return Err(e);
                    }
                },
            }
        }
        Err(CoreError::Other(format!("document {} kept changing while it was replaced; it was left as it is", to_shell(&id, Style::Display))))
    }
}

/// A collection's `listCollections` entry and its uuid, which says which
/// collection it is whichever connection found it.
struct Found {
    spec: Document,
    /// None for a view. A time series has none of its own; its buckets
    /// collection's stands in.
    uuid: Option<Bson>,
}

impl Found {
    /// "collection", "view" or "timeseries".
    fn kind(&self) -> Option<&str> {
        self.spec.get_str("type").ok()
    }

    /// For a view, the collection (or view) it reads from, in its database.
    fn view_on(&self) -> Option<&str> {
        if self.kind() != Some("view") {
            return None;
        }
        self.spec.get_document("options").ok()?.get_str("viewOn").ok()
    }
}

/// The collection's `listCollections` entry, if it exists.
async fn find(conn: &Conn, ns: &Namespace) -> CoreResult<Option<Found>> {
    Ok(found_in(&conn.collection_specs(&ns.db).await?, &ns.coll))
}

/// `coll`'s entry among one database's `listCollections` entries.
fn found_in(specs: &[Document], coll: &str) -> Option<Found> {
    let named = |name: &str| specs.iter().find(|s| s.get_str("name").ok() == Some(name));
    let uuid_of = |s: &Document| s.get_document("info").ok().and_then(|i| i.get("uuid")).cloned();
    let spec = named(coll)?;
    let uuid = match uuid_of(spec) {
        None if spec.get_str("type").ok() == Some("timeseries") => named(&format!("system.buckets.{coll}")).and_then(uuid_of),
        uuid => uuid,
    };
    Some(Found { spec: spec.clone(), uuid })
}

/// The source databases' `listCollections` entries, read once before the
/// first item runs (one listing per database). Items copy what was there
/// then: a view is recreated from its definition even if an earlier item
/// replaced it, and every target is checked against every source.
struct Sources(HashMap<String, CoreResult<Vec<Document>>>);

impl Sources {
    async fn read(conn: &Conn, items: &[CopyItem]) -> Self {
        let mut dbs = HashMap::new();
        for item in items {
            if !dbs.contains_key(&item.src.db) {
                dbs.insert(item.src.db.clone(), conn.collection_specs(&item.src.db).await);
            }
        }
        Self(dbs)
    }

    fn find(&self, ns: &Namespace) -> CoreResult<Option<Found>> {
        match self.0.get(&ns.db) {
            Some(Ok(specs)) => Ok(found_in(specs, &ns.coll)),
            Some(Err(e)) => Err(e.clone()),
            None => Ok(None),
        }
    }

    /// The uuid `ns` had, when it existed and one could be read.
    fn uuid(&self, ns: &Namespace) -> Option<Bson> {
        self.find(ns).ok().flatten().and_then(|f| f.uuid)
    }
}

#[cfg(test)]
mod tests {
    use bson::{Document, doc};
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::api::{CopiedItem, Event, IfExists, Namespace, OnConflict, OnDuplicateId, Record};
    use crate::conn::FindOpts;
    use crate::history::{EntryHeader, Kind, Saved, doc_hash};
    use crate::testkit::{TempDir, mock, raw, saved, store};

    const SKIP: IfExists = IfExists::Append(OnDuplicateId::Skip);

    fn ns(db: &str, coll: &str) -> Namespace {
        Namespace::new(db, coll)
    }

    fn item(src: Namespace, dst: Namespace, if_exists: IfExists) -> CopyItem {
        CopyItem { src, dst, if_exists }
    }

    /// `n` documents `{ _id: i, a: i, s: "doc i" }`.
    async fn seed(c: &Conn, ns: &Namespace, n: i32) {
        c.insert_many(ns, (0..n).map(|i| doc! { "_id": i, "a": i, "s": format!("doc {i}") }).collect()).await.unwrap();
    }

    /// Every document, by `_id`.
    async fn all(c: &Conn, ns: &Namespace) -> Vec<Document> {
        let opts = FindOpts { sort: Some(doc! { "_id": 1 }), ..Default::default() };
        c.open_find(ns, doc! {}, opts).await.unwrap().next_batch(usize::MAX).await.unwrap()
    }

    async fn spec(c: &Conn, ns: &Namespace) -> Option<Document> {
        c.collection_specs(&ns.db).await.unwrap().into_iter().find(|s| s.get_str("name").ok() == Some(ns.coll.as_str()))
    }

    async fn index_names(c: &Conn, ns: &Namespace) -> Vec<String> {
        c.list_indexes(ns).await.unwrap().iter().map(|i| i.get_str("name").unwrap().to_string()).collect()
    }

    async fn copy(src: &Conn, dst: &Conn, items: &[CopyItem]) -> CopyReport {
        let (progress, _) = Progress::channel();
        copy_collections(src, dst, false, items, CopyOptions::default(), &progress, &CancellationToken::new()).await.unwrap()
    }

    fn progress_of(rx: &crossbeam_channel::Receiver<crate::api::Response>) -> Vec<(u64, Option<u64>, String)> {
        rx.try_iter()
            .filter_map(|r| match r.event {
                Event::Progress { done, total, note } => Some((done, total, note)),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn copies_documents_options_and_indexes() {
        let (src, dst) = (mock(), mock());
        let (from, to) = (ns("db1", "orders"), ns("db2", "orders"));
        let validator = doc! { "$jsonSchema": { "required": ["a"] } };
        src.create_collection_with(&from, doc! { "validator": validator.clone() }).await.unwrap();
        seed(&src, &from, 2500).await;
        src.create_index(&from, doc! { "a": 1 }, Some(doc! { "name": "a_1" })).await.unwrap();

        let (progress, rx) = Progress::channel();
        let items = [item(from.clone(), to.clone(), SKIP)];
        let report = copy_collections(&src, &dst, false, &items, CopyOptions::default(), &progress, &CancellationToken::new()).await.unwrap();

        let copied = CopiedItem { src: from.clone(), dst: to.clone(), inserted: 2500, overwritten: 0, skipped: 0, recorded: None, warning: None, error: None, unconfirmed: false };
        assert_eq!(report, CopyReport { items: vec![copied], cancelled: false });
        assert_eq!(all(&dst, &to).await, all(&src, &from).await);
        let options = spec(&dst, &to).await.expect("target created").get_document("options").unwrap().clone();
        assert_eq!(options.get_document("validator").unwrap(), &validator);
        assert_eq!(index_names(&dst, &to).await, ["_id_", "a_1"]);
        let events = progress_of(&rx);
        assert_eq!(events.last(), Some(&(2500, Some(2500), "db2.orders (1/1)".to_string())), "{events:?}");
        assert!(events.windows(2).all(|w| w[0].0 <= w[1].0), "{events:?}");
    }

    /// Source `db1.orders` with 2,500 documents; target `db2.orders` already
    /// holding `_id` 0..n with other content.
    async fn with_existing(n: i32) -> (Conn, Conn, Namespace, Namespace) {
        let (src, dst) = (mock(), mock());
        let (from, to) = (ns("db1", "orders"), ns("db2", "orders"));
        seed(&src, &from, 2500).await;
        dst.insert_many(&to, (0..n).map(|i| doc! { "_id": i, "other": true }).collect()).await.unwrap();
        (src, dst, from, to)
    }

    #[tokio::test]
    async fn append_skip_counts_existing_ids() {
        let (src, dst, from, to) = with_existing(10).await;
        let report = copy(&src, &dst, &[item(from.clone(), to.clone(), SKIP)]).await;
        let r = &report.items[0];
        assert_eq!((r.inserted, r.skipped, r.overwritten, r.error.as_deref()), (2490, 10, 0, None));
        let docs = all(&dst, &to).await;
        assert_eq!(docs.len(), 2500);
        assert!(docs[..10].iter().all(|d| d.get_bool("other").ok() == Some(true)), "existing documents unchanged");
        assert_eq!(docs[10..], all(&src, &from).await[10..]);
    }

    #[tokio::test]
    async fn append_overwrite_replaces_by_id() {
        let (src, dst, from, to) = with_existing(10).await;
        let report = copy(&src, &dst, &[item(from.clone(), to.clone(), IfExists::Append(OnDuplicateId::Overwrite))]).await;
        let r = &report.items[0];
        assert_eq!((r.inserted, r.skipped, r.overwritten, r.error.as_deref()), (2490, 0, 10, None));
        assert_eq!(all(&dst, &to).await, all(&src, &from).await);
    }

    #[tokio::test]
    async fn append_stop_ends_that_item_only() {
        let (src, dst) = (mock(), mock());
        let (orders, users) = (ns("db1", "orders"), ns("db1", "users"));
        let (to_orders, to_users) = (ns("db2", "orders"), ns("db2", "users"));
        seed(&src, &orders, 2500).await;
        seed(&src, &users, 300).await;
        dst.insert_one(&to_orders, doc! { "_id": 1500, "other": true }).await.unwrap();

        let stop = IfExists::Append(OnDuplicateId::Stop);
        let report = copy(&src, &dst, &[item(orders, to_orders.clone(), stop), item(users.clone(), to_users.clone(), stop)]).await;
        let (first, second) = (&report.items[0], &report.items[1]);
        assert_eq!(first.error.as_deref(), Some("duplicate _id 1500"));
        // The first batch, and the second one apart from the duplicate (the
        // insert is unordered); the count says exactly what went in.
        assert_eq!((first.inserted, first.skipped, first.overwritten), (1999, 0, 0));
        assert_eq!(dst.count(&to_orders, doc! {}).await.unwrap(), first.inserted + 1);
        assert_eq!(dst.count(&to_orders, doc! { "other": true }).await.unwrap(), 1);
        assert_eq!((second.inserted, second.error.as_deref()), (300, None));
        assert_eq!(all(&dst, &to_users).await, all(&src, &users).await);
        assert!(!report.cancelled);
    }

    #[tokio::test]
    async fn replace_drops_the_target_first() {
        let (src, dst) = (mock(), mock());
        let (from, to) = (ns("db1", "orders"), ns("db2", "orders"));
        seed(&src, &from, 2500).await;
        dst.insert_many(&to, (0..5).map(|i| doc! { "_id": format!("x{i}") }).collect()).await.unwrap();
        dst.create_index(&to, doc! { "old": 1 }, None).await.unwrap();

        let report = copy(&src, &dst, &[item(from.clone(), to.clone(), IfExists::Replace)]).await;
        assert_eq!((report.items[0].inserted, report.items[0].error.as_deref()), (2500, None));
        assert_eq!(all(&dst, &to).await, all(&src, &from).await);
        assert_eq!(index_names(&dst, &to).await, ["_id_"]);
    }

    #[tokio::test]
    async fn views_are_recreated_as_views() {
        let (src, dst) = (mock(), mock());
        seed(&src, &ns("db1", "orders"), 10).await;
        let definition = doc! { "viewOn": "orders", "pipeline": [{ "$match": { "a": 1 } }] };
        src.create_collection_with(&ns("db1", "v"), definition.clone()).await.unwrap();

        let report = copy(&src, &dst, &[item(ns("db1", "v"), ns("db2", "v"), SKIP)]).await;
        assert_eq!((report.items[0].inserted, report.items[0].error.as_deref()), (0, None));
        let specs: Vec<(String, String, Document)> = dst
            .collection_specs("db2")
            .await
            .unwrap()
            .iter()
            .map(|s| (s.get_str("name").unwrap().to_string(), s.get_str("type").unwrap().to_string(), s.get_document("options").unwrap().clone()))
            .collect();
        assert_eq!(specs, [("v".to_string(), "view".to_string(), definition)]);
    }

    /// A view holds no documents to append, and takes none: appending a view
    /// onto anything, or anything onto a view, is an item error (it used to
    /// report success and do nothing). Replace and a new name still work.
    #[tokio::test]
    async fn appending_to_or_from_a_view_is_refused() {
        let (src, dst) = (mock(), mock());
        seed(&src, &ns("db1", "orders"), 5).await;
        let definition = doc! { "viewOn": "orders", "pipeline": [{ "$match": { "a": 1 } }] };
        src.create_collection_with(&ns("db1", "v"), definition.clone()).await.unwrap();
        let theirs = doc! { "viewOn": "other", "pipeline": [] };
        dst.create_collection_with(&ns("db2", "v"), theirs.clone()).await.unwrap();
        seed(&dst, &ns("db2", "plain"), 2).await;
        dst.create_collection_with(&ns("db2", "w"), theirs.clone()).await.unwrap();

        let items = [
            item(ns("db1", "v"), ns("db2", "v"), SKIP),
            item(ns("db1", "v"), ns("db2", "plain"), IfExists::Append(OnDuplicateId::Overwrite)),
            item(ns("db1", "orders"), ns("db2", "w"), SKIP),
        ];
        let report = copy(&src, &dst, &items).await;
        for r in &report.items {
            assert!(r.error.as_deref().is_some_and(|e| e.contains("view") && e.contains("replace it or paste under a new name")), "{r:?}");
            assert_eq!((r.inserted, r.overwritten, r.skipped), (0, 0, 0));
        }
        assert_eq!(spec(&dst, &ns("db2", "v")).await.unwrap().get_document("options").unwrap(), &theirs, "the target view is unchanged");
        assert_eq!(all(&dst, &ns("db2", "plain")).await.len(), 2);

        let report = copy(&src, &dst, &[item(ns("db1", "v"), ns("db2", "v"), IfExists::Replace), item(ns("db1", "v"), ns("db2", "v2"), SKIP)]).await;
        assert!(report.items.iter().all(|r| r.error.is_none()), "{report:?}");
        for name in ["v", "v2"] {
            assert_eq!(spec(&dst, &ns("db2", name)).await.unwrap().get_document("options").unwrap(), &definition);
        }
    }

    /// `v` reads from `orders`: pasting v over orders would drop orders, then
    /// fail to make a view on itself. Refused before anything is dropped,
    /// within one connection by name and through another by orders' uuid.
    #[tokio::test]
    async fn a_view_never_replaces_what_it_reads() {
        let c = mock();
        let orders = ns("db1", "orders");
        seed(&c, &orders, 5).await;
        c.create_collection_with(&ns("db1", "v"), doc! { "viewOn": "orders", "pipeline": [] }).await.unwrap();
        for (same, dst) in [(true, c.clone()), (false, c.clone())] {
            for if_exists in [IfExists::Replace, SKIP] {
                let (progress, _) = Progress::channel();
                let items = [item(ns("db1", "v"), orders.clone(), if_exists)];
                let report = copy_collections(&c, &dst, same, &items, CopyOptions::default(), &progress, &CancellationToken::new()).await.unwrap();
                let r = &report.items[0];
                assert!(r.error.as_deref().is_some_and(|e| e.contains("view v reads from")), "same connection {same}, {if_exists:?}: {r:?}");
                assert_eq!(all(&c, &orders).await.len(), 5, "orders keeps its documents");
                assert_eq!(spec(&c, &orders).await.unwrap().get_str("type").unwrap(), "collection");
            }
        }
        // Another collection of that name elsewhere is fair game.
        let other = mock();
        seed(&other, &orders, 2).await;
        let report = copy(&c, &other, &[item(ns("db1", "v"), orders.clone(), IfExists::Replace)]).await;
        assert_eq!(report.items[0].error, None);
    }

    #[tokio::test]
    async fn empty_source_still_creates_the_target() {
        let (src, dst) = (mock(), mock());
        let (from, to) = (ns("db1", "empty"), ns("db2", "empty"));
        let options = doc! { "capped": true, "size": 4096 };
        src.create_collection_with(&from, options.clone()).await.unwrap();
        src.create_index(&from, doc! { "b": 1 }, None).await.unwrap();

        let (progress, rx) = Progress::channel();
        let items = [item(from, to.clone(), SKIP)];
        let report = copy_collections(&src, &dst, false, &items, CopyOptions::default(), &progress, &CancellationToken::new()).await.unwrap();
        assert_eq!((report.items[0].inserted, report.items[0].error.as_deref()), (0, None));
        assert_eq!(spec(&dst, &to).await.expect("target created").get_document("options").unwrap(), &options);
        assert_eq!(index_names(&dst, &to).await, ["_id_", "b_1"]);
        let events = progress_of(&rx);
        assert_eq!(events.last().map(|e| (e.0, e.1)), Some((0, Some(0))), "{events:?}");
    }

    #[tokio::test]
    async fn options_and_indexes_can_be_left_behind() {
        let (src, dst) = (mock(), mock());
        let (from, to) = (ns("db1", "orders"), ns("db2", "orders"));
        src.create_collection_with(&from, doc! { "validator": { "a": { "$exists": true } } }).await.unwrap();
        seed(&src, &from, 5).await;
        src.create_index(&from, doc! { "a": 1 }, None).await.unwrap();

        let (progress, _) = Progress::channel();
        let options = CopyOptions { indexes: false, collection_options: false, history: None };
        let report = copy_collections(&src, &dst, false, &[item(from, to.clone(), SKIP)], options, &progress, &CancellationToken::new()).await.unwrap();
        assert_eq!((report.items[0].inserted, report.items[0].error.as_deref()), (5, None));
        assert_eq!(spec(&dst, &to).await.unwrap().get_document("options").unwrap(), &doc! {});
        assert_eq!(index_names(&dst, &to).await, ["_id_"]);
    }

    #[tokio::test]
    async fn pasting_into_itself_is_refused() {
        let c = mock();
        let orders = ns("db1", "orders");
        seed(&c, &orders, 10).await;
        c.create_index(&orders, doc! { "a": 1 }, None).await.unwrap();
        for if_exists in [IfExists::Replace, IfExists::Append(OnDuplicateId::Overwrite)] {
            let (progress, _) = Progress::channel();
            let items = [item(orders.clone(), orders.clone(), if_exists)];
            let report = copy_collections(&c, &c, true, &items, CopyOptions::default(), &progress, &CancellationToken::new()).await.unwrap();
            let r = &report.items[0];
            assert_eq!(r.error.as_deref(), Some("a collection can't be pasted into itself"), "{if_exists:?}");
            assert_eq!((r.inserted, r.overwritten, r.skipped), (0, 0, 0));
            assert_eq!(all(&c, &orders).await.len(), 10);
            assert_eq!(index_names(&c, &orders).await, ["_id_", "a_1"]);
        }
        // The same name on another connection is another collection.
        let other = mock();
        let report = copy(&c, &other, &[item(orders.clone(), orders.clone(), IfExists::Replace)]).await;
        assert_eq!((report.items[0].inserted, report.items[0].error.as_deref()), (10, None));
    }

    /// `a → b` with `b → b2` in one database: writing `b` first would hand
    /// `b2` a's documents (and drop b under Replace). The clashing item is
    /// refused, the rest run, and b keeps what it had.
    #[tokio::test]
    async fn writing_into_another_items_source_is_refused() {
        for if_exists in [IfExists::Replace, IfExists::Append(OnDuplicateId::Overwrite), SKIP] {
            let c = mock();
            let (a, b, b2) = (ns("db1", "a"), ns("db1", "b"), ns("db1", "b2"));
            seed(&c, &a, 3).await;
            c.insert_many(&b, vec![doc! { "_id": 100, "own": true }]).await.unwrap();
            let (progress, _) = Progress::channel();
            let items = [item(a.clone(), b.clone(), if_exists), item(b.clone(), b2.clone(), SKIP)];
            let report = copy_collections(&c, &c, true, &items, CopyOptions::default(), &progress, &CancellationToken::new()).await.unwrap();
            assert!(report.items[0].error.as_deref().is_some_and(|e| e.contains("db1.b is also being pasted")), "{if_exists:?}: {:?}", report.items[0].error);
            assert_eq!((report.items[0].inserted, report.items[0].overwritten, report.items[0].skipped), (0, 0, 0));
            assert_eq!(report.items[1].error, None, "the other item still runs");
            assert_eq!(all(&c, &b).await, [doc! { "_id": 100, "own": true }], "b keeps its documents ({if_exists:?})");
            assert_eq!(all(&c, &b2).await, [doc! { "_id": 100, "own": true }], "b2 is a copy of the original b");
            // Two connections: the names are other collections, nothing clashes.
            let other = mock();
            let report = copy(&c, &other, &items).await;
            assert!(report.items.iter().all(|r| r.error.is_none()), "{:?}", report.items);
        }
    }

    /// Two profiles on one server (mock clones share the store): `a → b`
    /// would drop or change b before `b → b2` copies it. The connections
    /// differ, so names say nothing; b's uuid says it is another item's
    /// source. That item is refused, b keeps its documents, and b2 is a copy
    /// of the original b.
    #[tokio::test]
    async fn writing_into_another_items_source_through_two_connections_is_refused() {
        for if_exists in [IfExists::Replace, IfExists::Append(OnDuplicateId::Overwrite), SKIP] {
            let c = mock();
            let (a, b, b2) = (ns("db1", "a"), ns("db1", "b"), ns("db1", "b2"));
            seed(&c, &a, 3).await;
            c.insert_many(&b, vec![doc! { "_id": 0, "own": true }, doc! { "_id": 100, "own": true }]).await.unwrap();
            let original = all(&c, &b).await;
            let same_server = c.clone();
            let items = [item(a.clone(), b.clone(), if_exists), item(b.clone(), b2.clone(), SKIP)];
            let report = copy(&c, &same_server, &items).await;
            let r = &report.items[0];
            assert!(r.error.as_deref().is_some_and(|e| e.contains("db1.b is also being pasted")), "{if_exists:?}: {r:?}");
            assert_eq!((r.inserted, r.overwritten, r.skipped), (0, 0, 0), "{if_exists:?}");
            assert_eq!((report.items[1].inserted, report.items[1].error.as_deref()), (2, None), "{if_exists:?}: the other item still runs");
            assert_eq!(all(&c, &b).await, original, "{if_exists:?}: b keeps its documents");
            assert_eq!(all(&c, &b2).await, original, "{if_exists:?}: b2 is a copy of the original b");
            // The other order: b is copied first, then would be written. Still
            // refused, as within one connection.
            let items = [item(b.clone(), ns("db1", "b3"), SKIP), item(a.clone(), b.clone(), if_exists)];
            let report = copy(&c, &same_server, &items).await;
            assert!(report.items[1].error.as_deref().is_some_and(|e| e.contains("db1.b is also being pasted")), "{if_exists:?}: {report:?}");
            assert_eq!(all(&c, &b).await, original, "{if_exists:?}");
        }

        // A view has no uuid to tell it by, and nothing but its definition,
        // which was read before the paste: replacing v2 loses nothing, and
        // v3 is still made from v2 as it was.
        let c = mock();
        let (d1, d2) = (doc! { "viewOn": "orders", "pipeline": [{ "$match": { "a": 1 } }] }, doc! { "viewOn": "orders", "pipeline": [{ "$match": { "a": 2 } }] });
        c.create_collection_with(&ns("db1", "v1"), d1.clone()).await.unwrap();
        c.create_collection_with(&ns("db1", "v2"), d2.clone()).await.unwrap();
        let items = [item(ns("db1", "v1"), ns("db1", "v2"), IfExists::Replace), item(ns("db1", "v2"), ns("db1", "v3"), SKIP)];
        let report = copy(&c, &c.clone(), &items).await;
        assert!(report.items.iter().all(|r| r.error.is_none()), "{report:?}");
        assert_eq!(spec(&c, &ns("db1", "v2")).await.unwrap().get_document("options").unwrap(), &d1);
        assert_eq!(spec(&c, &ns("db1", "v3")).await.unwrap().get_document("options").unwrap(), &d2, "from the definition read before the paste");
    }

    #[tokio::test]
    async fn pasting_into_itself_through_two_connections_is_refused() {
        // Two profiles can point at one server: the connections differ, the
        // collection (its uuid) is the same. Mock clones share the store.
        let c = mock();
        let orders = ns("db1", "orders");
        seed(&c, &orders, 10).await;
        c.create_index(&orders, doc! { "a": 1 }, None).await.unwrap();
        let same_server = c.clone();
        for if_exists in [IfExists::Replace, SKIP, IfExists::Append(OnDuplicateId::Overwrite)] {
            let (progress, _) = Progress::channel();
            let items = [item(orders.clone(), orders.clone(), if_exists)];
            let report = copy_collections(&c, &same_server, false, &items, CopyOptions::default(), &progress, &CancellationToken::new()).await.unwrap();
            let r = &report.items[0];
            assert_eq!(r.error.as_deref(), Some("a collection can't be pasted into itself"), "{if_exists:?}");
            assert_eq!((r.inserted, r.overwritten, r.skipped), (0, 0, 0));
            assert_eq!(all(&c, &orders).await.len(), 10, "{if_exists:?}: the source keeps its documents");
            assert_eq!(index_names(&c, &orders).await, ["_id_", "a_1"]);
        }
        // Another collection on that server is another collection.
        let report = copy(&c, &same_server, &[item(orders.clone(), ns("db1", "orders_copy"), IfExists::Replace)]).await;
        assert_eq!((report.items[0].inserted, report.items[0].error.as_deref()), (10, None));
    }

    #[tokio::test]
    async fn replace_needs_both_uuids() {
        // R8: when either uuid can't be read (a time series whose buckets
        // entry isn't visible, an older server), Replace can't tell the
        // target from the source, so it drops nothing.
        for hidden in ["source", "target"] {
            let (src, dst) = (mock(), mock());
            let (from, to) = (ns("db1", "orders"), ns("db2", "orders"));
            seed(&src, &from, 10).await;
            dst.insert_many(&to, (0..5).map(|i| doc! { "_id": format!("x{i}") }).collect()).await.unwrap();
            let (conn, hide) = if hidden == "source" { (&src, &from) } else { (&dst, &to) };
            conn.with_store(|s| s.forget_uuid(hide));
            let before = all(&dst, &to).await;

            let report = copy(&src, &dst, &[item(from.clone(), to.clone(), IfExists::Replace)]).await;
            let r = &report.items[0];
            assert!(r.error.as_deref().is_some_and(|e| e.contains("can't confirm the target isn't the source")), "{hidden}: {r:?}");
            assert_eq!((r.inserted, r.overwritten, r.skipped), (0, 0, 0));
            assert_eq!(all(&dst, &to).await, before, "{hidden}: the target is untouched");
            assert_eq!(all(&src, &from).await.len(), 10, "{hidden}: the source is untouched");

            // Append and a new name drop nothing, so they still copy.
            let report = copy(&src, &dst, &[item(from.clone(), to.clone(), SKIP), item(from.clone(), ns("db2", "fresh"), IfExists::Replace)]).await;
            assert!(report.items.iter().all(|r| r.inserted == 10 && r.error.is_none()), "{hidden}: {report:?}");
        }

        // A view has no uuid by nature and nothing to lose (its definition is
        // read before the drop), so Replace still swaps one view for another.
        let (src, dst) = (mock(), mock());
        let definition = doc! { "viewOn": "orders", "pipeline": [{ "$match": { "a": 1 } }] };
        src.create_collection_with(&ns("db1", "v"), definition.clone()).await.unwrap();
        dst.create_collection_with(&ns("db2", "v"), doc! { "viewOn": "other", "pipeline": [] }).await.unwrap();
        let report = copy(&src, &dst, &[item(ns("db1", "v"), ns("db2", "v"), IfExists::Replace)]).await;
        assert_eq!(report.items[0].error, None);
        assert_eq!(spec(&dst, &ns("db2", "v")).await.unwrap().get_document("options").unwrap(), &definition);
    }

    /// A time series on `timeField: "at"` holding `{ _id: "{tag}{i}", at, v: i }`.
    async fn seed_series(c: &Conn, ns: &Namespace, tag: &str, n: i64) {
        c.create_collection_with(ns, doc! { "timeseries": { "timeField": "at" } }).await.unwrap();
        let at = |i: i64| bson::DateTime::from_millis(1_760_000_000_000 + i * 60_000);
        c.insert_many(ns, (0..n).map(|i| doc! { "_id": format!("{tag}{i}"), "at": at(i), "v": i }).collect()).await.unwrap();
    }

    #[tokio::test]
    async fn appending_to_a_time_series_is_refused() {
        // A time series has no unique _id, so Skip/Overwrite/Stop can't tell
        // which documents are already there. The target's kind decides it,
        // not the demo server (which does keep _id unique there).
        let (src, dst) = (mock(), mock());
        let (from, to) = (ns("db1", "ts"), ns("db2", "ts"));
        seed_series(&src, &from, "s", 5).await;
        seed_series(&dst, &to, "d", 3).await;
        let before = all(&dst, &to).await;
        let items = [OnDuplicateId::Skip, OnDuplicateId::Overwrite, OnDuplicateId::Stop].map(|d| item(from.clone(), to.clone(), IfExists::Append(d)));
        let report = copy(&src, &dst, &items).await;
        assert_eq!(report.items.len(), 3);
        for r in &report.items {
            assert!(r.error.as_deref().is_some_and(|e| e.contains("time series")), "{r:?}");
            assert_eq!((r.inserted, r.overwritten, r.skipped), (0, 0, 0));
        }
        assert_eq!(all(&dst, &to).await, before, "the target is untouched");

        // Replace and a new name still work.
        let fresh = ns("db2", "ts2");
        let report = copy(&src, &dst, &[item(from.clone(), to.clone(), IfExists::Replace), item(from.clone(), fresh.clone(), SKIP)]).await;
        assert!(report.items.iter().all(|r| r.inserted == 5 && r.error.is_none()), "{report:?}");
        for target in [to, fresh] {
            assert_eq!(all(&dst, &target).await, all(&src, &from).await);
            assert_eq!(spec(&dst, &target).await.unwrap().get_str("type").unwrap(), "timeseries");
        }
    }

    #[tokio::test]
    async fn validation_failures_keep_exact_counts() {
        // Documents written before the source got its validator (every 500th
        // from 7 lacks `a`) are refused by a target that got the validator;
        // the rest of the unordered batch still goes in, and the count says so.
        let (src, dst) = (mock(), mock());
        let from = ns("db1", "orders");
        let docs = (0..2500).map(|i| if i % 500 == 7 { doc! { "_id": i } } else { doc! { "_id": i, "a": i } }).collect();
        src.insert_many(&from, docs).await.unwrap();
        src.set_validator(&from, doc! { "$jsonSchema": { "required": ["a"] } }, "strict", "error").await.unwrap();
        src.create_index(&from, doc! { "a": 1 }, None).await.unwrap();

        let to = ns("db2", "orders");
        let report = copy(&src, &dst, &[item(from.clone(), to.clone(), SKIP)]).await;
        let r = &report.items[0];
        let error = r.error.as_deref().unwrap_or_default();
        assert!(error.contains("121") && error.contains("paste without collection options"), "{r:?}");
        assert_eq!(r.inserted, dst.count(&to, doc! {}).await.unwrap());
        assert_eq!((r.inserted, r.skipped, r.overwritten), (998, 0, 0), "the first batch apart from _id 7 and 507");
        assert_eq!(index_names(&dst, &to).await, ["_id_"], "no indexes after a failed copy");

        // Without the options the target takes them all.
        let (progress, _) = Progress::channel();
        let options = CopyOptions { collection_options: false, ..Default::default() };
        let report = copy_collections(&src, &dst, false, &[item(from.clone(), ns("db2", "plain"), SKIP)], options, &progress, &CancellationToken::new()).await.unwrap();
        assert_eq!((report.items[0].inserted, report.items[0].error.as_deref()), (2500, None));

        // The target's own validator: unticking the options would not help,
        // so the error says nothing about them.
        let own = ns("db2", "own");
        dst.create_collection_with(&own, doc! { "validator": { "a": { "$exists": true } } }).await.unwrap();
        let report = copy(&src, &dst, &[item(from, own.clone(), SKIP)]).await;
        let r = &report.items[0];
        let error = r.error.as_deref().unwrap_or_default();
        assert!(error.contains("121") && !error.contains("collection options"), "{r:?}");
        assert_eq!((r.inserted, dst.count(&own, doc! {}).await.unwrap()), (998, 998));
    }

    #[tokio::test]
    async fn missing_source_fails_alone() {
        let (src, dst) = (mock(), mock());
        seed(&src, &ns("db1", "orders"), 20).await;
        let items = [item(ns("db1", "gone"), ns("db2", "gone"), SKIP), item(ns("db1", "orders"), ns("db2", "orders"), SKIP)];
        let report = copy(&src, &dst, &items).await;
        assert_eq!(report.items[0].error.as_deref(), Some("not found"));
        assert_eq!((report.items[1].inserted, report.items[1].error.as_deref()), (20, None));
        let names: Vec<String> = dst.list_collections("db2").await.unwrap().into_iter().map(|c| c.name).collect();
        assert_eq!(names, ["orders"]);
    }

    #[tokio::test]
    async fn names_with_dots_and_thai_copy_as_named() {
        let (src, dst) = (mock(), mock());
        let names = ["fs.files", "ลูกค้า"];
        for name in names {
            seed(&src, &ns("db1", name), 3).await;
        }
        let items: Vec<CopyItem> = names.iter().map(|n| item(ns("db1", n), ns("db2", n), SKIP)).collect();
        let report = copy(&src, &dst, &items).await;
        assert!(report.items.iter().all(|i| i.inserted == 3 && i.error.is_none()), "{report:?}");
        let copied: Vec<String> = dst.list_collections("db2").await.unwrap().into_iter().map(|c| c.name).collect();
        assert_eq!(copied, names);
        for name in names {
            assert_eq!(all(&dst, &ns("db2", name)).await, all(&src, &ns("db1", name)).await);
        }
    }

    #[tokio::test]
    async fn system_collections_and_bad_names_are_refused() {
        let (src, dst) = (mock(), mock());
        seed(&src, &ns("db1", "system.profile"), 2).await;
        seed(&src, &ns("db1", "orders"), 2).await;
        let items = [item(ns("db1", "system.profile"), ns("db2", "profile"), SKIP), item(ns("db1", "orders"), ns("db2", "system.orders"), SKIP), item(ns("db1", "orders"), ns("db2", "a$b"), SKIP)];
        let report = copy(&src, &dst, &items).await;
        assert_eq!(report.items.len(), 3);
        assert!(report.items.iter().all(|i| i.error.is_some() && i.inserted == 0), "{report:?}");
        assert!(dst.list_databases().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn read_only_mid_copy_stops_cleanly() {
        let (src, dst) = (mock(), mock());
        let (from, to) = (ns("db1", "orders"), ns("db2", "orders"));
        seed(&src, &from, 2500).await;
        src.create_index(&from, doc! { "a": 1 }, None).await.unwrap();
        let lock = dst.clone();
        let progress = Progress::with_hook(move |done| {
            if done >= 1000 {
                lock.set_read_only(true);
            }
        });
        let items = [item(from, to.clone(), SKIP)];
        let report = copy_collections(&src, &dst, false, &items, CopyOptions::default(), &progress, &CancellationToken::new()).await.unwrap();
        let r = &report.items[0];
        assert!(r.error.as_deref().is_some_and(|e| e.contains("read-only")), "{r:?}");
        assert_eq!(r.inserted, 1000);
        assert_eq!(dst.count(&to, doc! {}).await.unwrap(), 1000);
        assert_eq!(index_names(&dst, &to).await, ["_id_"], "no indexes after a failed copy");
        assert!(!report.cancelled);
    }

    #[tokio::test]
    async fn cancel_stops_between_batches() {
        let (src, dst) = (mock(), mock());
        seed(&src, &ns("db1", "orders"), 2500).await;
        seed(&src, &ns("db1", "users"), 10).await;
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let progress = Progress::with_hook(move |_| token.cancel());
        let items = [item(ns("db1", "orders"), ns("db2", "orders"), SKIP), item(ns("db1", "users"), ns("db2", "users"), SKIP)];
        let report = copy_collections(&src, &dst, false, &items, CopyOptions::default(), &progress, &cancel).await.unwrap();
        assert!(report.cancelled);
        assert_eq!(report.items.len(), 1, "the second collection was not started: {report:?}");
        assert_eq!(report.items[0].inserted, 1000);
        assert_eq!(dst.count(&ns("db2", "orders"), doc! {}).await.unwrap(), report.items[0].inserted);
        assert!(spec(&dst, &ns("db2", "users")).await.is_none());
    }

    #[tokio::test]
    async fn read_only_target_refuses() {
        let (src, dst) = (mock(), mock());
        let (from, to) = (ns("db1", "orders"), ns("db2", "orders"));
        seed(&src, &from, 10).await;
        dst.insert_many(&to, (0..5).map(|i| doc! { "_id": i, "other": true }).collect()).await.unwrap();
        dst.set_read_only(true);

        let items = [item(from.clone(), to.clone(), IfExists::Replace), item(from.clone(), to.clone(), SKIP), item(from, ns("db2", "fresh"), SKIP)];
        let report = copy(&src, &dst, &items).await;
        assert_eq!(report.items.len(), 3);
        for r in &report.items {
            assert!(r.error.as_deref().is_some_and(|e| e.contains("read-only")), "{r:?}");
            assert_eq!((r.inserted, r.overwritten, r.skipped), (0, 0, 0));
        }
        let docs = all(&dst, &to).await;
        assert_eq!(docs.len(), 5);
        assert!(docs.iter().all(|d| d.get_bool("other").ok() == Some(true)));
        assert!(spec(&dst, &ns("db2", "fresh")).await.is_none());
    }

    // ---- Collection History for Append + Overwrite ----

    /// History in `t`, for the target connection.
    fn record(t: &TempDir) -> Record {
        Record { conn_name: "target".into(), ..crate::testkit::record(t) }
    }

    fn entries(r: &Record, ns: &Namespace) -> Vec<EntryHeader> {
        store(r).list(r.profile, ns).unwrap()
    }

    const OVERWRITE: IfExists = IfExists::Append(OnDuplicateId::Overwrite);

    async fn copy_recorded(src: &Conn, dst: &Conn, items: &[CopyItem], record: &Record) -> CopyReport {
        let (progress, _) = Progress::channel();
        let options = CopyOptions { history: Some(record.clone()), ..Default::default() };
        copy_collections(src, dst, false, items, options, &progress, &CancellationToken::new()).await.unwrap()
    }

    /// The target as it was before: `{ _id: i, other: true }` for 0..n.
    fn originals(n: i32) -> Vec<Document> {
        (0..n).map(|i| doc! { "_id": i, "other": true }).collect()
    }

    #[tokio::test]
    async fn overwrite_records_the_replaced_documents() {
        let (t, (src, dst, from, to)) = (TempDir::new(), with_existing(10).await);
        let r = record(&t);
        let report = copy_recorded(&src, &dst, &[item(from.clone(), to.clone(), OVERWRITE)], &r).await;
        let out = &report.items[0];
        assert_eq!((out.inserted, out.overwritten, out.skipped, out.error.as_deref()), (2490, 10, 0, None));
        assert_eq!(all(&dst, &to).await, all(&src, &from).await);

        // One update entry under the target, holding the 10 documents as they were.
        let listed = entries(&r, &to);
        assert_eq!(listed.len(), 1, "{listed:?}");
        let h = &listed[0];
        assert_eq!((h.kind, h.count, h.complete, h.conn_name.as_str()), (Kind::Update, 10, true, "target"));
        assert_eq!(out.recorded.as_ref(), Some(&h.id));
        assert_eq!(h.id.profile, r.profile);
        assert!(entries(&r, &from).is_empty(), "nothing under the source");
        let s = saved(&r, &h.id);
        let mut want: Vec<Saved> = Vec::new();
        for (i, d) in all(&src, &from).await.iter().take(10).enumerate() {
            want.push(Saved { before: raw(&doc! { "_id": i as i32, "other": true }), after_hash: Some(doc_hash(&raw(d))) });
        }
        assert_eq!(s, want);

        // Restoring it (Collection History) brings the target's originals back.
        let (progress, _) = Progress::channel();
        let restored = crate::restore::restore(&dst, &r, &h.id, OnConflict::Skip, &progress, &CancellationToken::new()).await.unwrap();
        assert_eq!((restored.restored, restored.skipped, restored.overwritten), (10, 0, 0), "{restored:?}");
        assert_eq!(all(&dst, &to).await[..10], originals(10));
        assert_eq!(all(&dst, &to).await[10..], all(&src, &from).await[10..]);
    }

    /// Only Append + Overwrite records, and only when something was replaced.
    #[tokio::test]
    async fn only_replaced_documents_leave_an_entry() {
        let t = TempDir::new();
        let r = record(&t);
        // Nothing collides: an Overwrite that replaces nothing leaves no entry.
        let (src, dst) = (mock(), mock());
        let (from, to) = (ns("db1", "orders"), ns("db2", "orders"));
        seed(&src, &from, 50).await;
        let report = copy_recorded(&src, &dst, &[item(from.clone(), to.clone(), OVERWRITE)], &r).await;
        assert_eq!((report.items[0].inserted, report.items[0].overwritten, report.items[0].recorded.clone()), (50, 0, None));
        assert!(entries(&r, &to).is_empty());

        // Skip and Replace never record.
        let (src, dst, from, to) = with_existing(10).await;
        let report = copy_recorded(&src, &dst, &[item(from.clone(), to.clone(), SKIP)], &r).await;
        assert_eq!((report.items[0].skipped, report.items[0].recorded.clone()), (10, None));
        let report = copy_recorded(&src, &dst, &[item(from.clone(), to.clone(), IfExists::Replace)], &r).await;
        assert_eq!(report.items[0].recorded, None);
        assert!(entries(&r, &to).is_empty());

        // Without a record nothing is kept either.
        let (src, dst, from, to) = with_existing(10).await;
        let report = copy(&src, &dst, &[item(from, to, OVERWRITE)]).await;
        assert_eq!((report.items[0].overwritten, report.items[0].recorded.clone()), (10, None));
    }

    /// One entry per item, each under its own target.
    #[tokio::test]
    async fn each_item_gets_its_own_entry() {
        let (t, (src, dst, from, to)) = (TempDir::new(), with_existing(5).await);
        let r = record(&t);
        let (users, to_users) = (ns("db1", "users"), ns("db2", "users"));
        seed(&src, &users, 3).await;
        dst.insert_many(&to_users, (0..3).map(|i| doc! { "_id": i, "u": true }).collect()).await.unwrap();
        let items = [item(from, to.clone(), OVERWRITE), item(users, to_users.clone(), OVERWRITE)];
        let report = copy_recorded(&src, &dst, &items, &r).await;
        assert!(report.items.iter().all(|i| i.error.is_none()), "{report:?}");
        let (a, b) = (entries(&r, &to), entries(&r, &to_users));
        assert_eq!((a.len(), a[0].count, b.len(), b[0].count), (1, 5, 1, 3));
        assert_eq!(report.items[0].recorded.as_ref(), Some(&a[0].id));
        assert_eq!(report.items[1].recorded.as_ref(), Some(&b[0].id));
    }

    /// Runs `f` with `hook` called with the `_id` before every guarded replace.
    async fn with_hook<T>(hook: impl Fn(&Bson) + 'static, f: impl std::future::Future<Output = T>) -> T {
        crate::restore::BEFORE_REPLACE.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
        let out = f.await;
        crate::restore::BEFORE_REPLACE.with(|h| *h.borrow_mut() = None);
        out
    }

    /// A hook that sets `x` of target document 3 to the next number, `times` times.
    fn change_doc_3(c: &Conn, ns: &Namespace, times: usize) -> impl Fn(&Bson) + 'static {
        let (c, ns, n) = (c.clone(), ns.clone(), std::cell::Cell::new(0));
        move |id: &Bson| {
            if *id == Bson::Int32(3) && n.get() < times {
                n.set(n.get() + 1);
                let x = 100 + n.get() as i32;
                c.with_store(|s| s.update(&ns, &doc! { "_id": 3 }, &doc! { "$set": { "x": x } }, false, false)).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn a_target_changed_before_its_replace_is_saved_first() {
        let (t, (src, dst, from, to)) = (TempDir::new(), with_existing(10).await);
        let r = record(&t);
        let report = with_hook(change_doc_3(&dst, &to, 1), copy_recorded(&src, &dst, &[item(from.clone(), to.clone(), OVERWRITE)], &r)).await;
        let out = &report.items[0];
        assert_eq!((out.inserted, out.overwritten, out.error.as_deref()), (2490, 10, None));
        assert_eq!(all(&dst, &to).await, all(&src, &from).await);
        // Both versions of document 3 were saved: the one looked up first
        // (never written over), then the one that was replaced.
        let s = saved(&r, out.recorded.as_ref().unwrap());
        let doc_3: Vec<&Saved> = s.iter().filter(|s| s.before.get_i32("_id").ok() == Some(3)).collect();
        assert_eq!(doc_3.len(), 2, "{s:?}");
        assert_eq!(doc_3[0].before, raw(&doc! { "_id": 3, "other": true }));
        assert_eq!(doc_3[0].after_hash, None);
        assert_eq!(doc_3[1].before, raw(&doc! { "_id": 3, "other": true, "x": 101 }));
        let new = all(&src, &from).await.remove(3);
        assert_eq!(doc_3[1].after_hash, Some(doc_hash(&raw(&new))));
        assert_eq!(s.len(), 11);
    }

    #[tokio::test]
    async fn a_target_that_keeps_changing_stops_the_item_unwritten() {
        let (t, (src, dst, from, to)) = (TempDir::new(), with_existing(10).await);
        let r = record(&t);
        let report = with_hook(change_doc_3(&dst, &to, usize::MAX), copy_recorded(&src, &dst, &[item(from, to.clone(), OVERWRITE)], &r)).await;
        let out = &report.items[0];
        assert!(out.error.as_deref().is_some_and(|e| e.contains("kept changing")), "{out:?}");
        // The documents replaced before it are counted; document 3's last
        // version was never written over.
        assert_eq!(out.overwritten, 9);
        let d3 = all(&dst, &to).await.remove(3);
        assert_eq!(d3.get_bool("other").ok(), Some(true));
        assert!(d3.get_i32("x").is_ok());
        let h = &entries(&r, &to)[0];
        assert_eq!(out.recorded.as_ref(), Some(&h.id), "what was replaced stays recoverable");
    }

    /// A history that can't be written stops the item before the unsaved
    /// overwrite; what was inserted before is counted.
    #[tokio::test]
    async fn a_history_failure_stops_the_item_before_overwriting() {
        let (t, (src, dst, from, to)) = (TempDir::new(), with_existing(10).await);
        let mut r = record(&t);
        // A file where the history directory should be.
        std::fs::create_dir_all(&t.0).unwrap();
        r.root = t.0.join("blocked");
        std::fs::write(&r.root, b"not a directory").unwrap();
        let report = copy_recorded(&src, &dst, &[item(from, to.clone(), OVERWRITE)], &r).await;
        let out = &report.items[0];
        assert!(out.error.is_some(), "{out:?}");
        assert_eq!((out.overwritten, out.recorded.clone()), (0, None));
        assert_eq!(out.inserted, 990, "the first batch's other documents went in and are counted");
        assert_eq!(all(&dst, &to).await[..10], originals(10), "nothing was overwritten unsaved");
    }

    /// Cancelled between batches: the entry is finished as cancelled.
    #[tokio::test]
    async fn a_cancelled_copy_finishes_its_entry() {
        let (t, (src, dst, from, to)) = (TempDir::new(), with_existing(10).await);
        let r = record(&t);
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let progress = Progress::with_hook(move |_| token.cancel());
        let options = CopyOptions { history: Some(r.clone()), ..Default::default() };
        let report = copy_collections(&src, &dst, false, &[item(from, to.clone(), OVERWRITE)], options, &progress, &cancel).await.unwrap();
        assert!(report.cancelled);
        let out = &report.items[0];
        assert_eq!((out.inserted, out.overwritten), (990, 10));
        let h = &entries(&r, &to)[0];
        assert_eq!((h.count, h.complete), (10, true));
        assert_eq!(out.recorded.as_ref(), Some(&h.id));
    }

    /// A read-only target is refused before anything is recorded.
    #[tokio::test]
    async fn a_read_only_target_records_nothing() {
        let (t, (src, dst, from, to)) = (TempDir::new(), with_existing(10).await);
        let r = record(&t);
        dst.set_read_only(true);
        let report = copy_recorded(&src, &dst, &[item(from, to.clone(), OVERWRITE)], &r).await;
        assert!(report.items[0].error.as_deref().is_some_and(|e| e.contains("read-only")));
        assert!(entries(&r, &to).is_empty());
    }

    /// Overwrite items whose history would not fit refuse the whole copy
    /// before anything is written.
    #[tokio::test]
    async fn a_history_that_would_not_fit_refuses_the_whole_copy() {
        let (t, (src, dst, from, to)) = (TempDir::new(), with_existing(10).await);
        let mut r = record(&t);
        let avg = dst.coll_stats(&to).await.unwrap().get_i64("avgObjSize").unwrap() as u64;
        r.max_bytes = 10 * avg - 1;
        let (progress, _) = Progress::channel();
        let options = CopyOptions { history: Some(r.clone()), ..Default::default() };
        let items = [item(from.clone(), ns("db2", "fresh"), SKIP), item(from.clone(), to.clone(), OVERWRITE)];
        let err = copy_collections(&src, &dst, false, &items, options, &progress, &CancellationToken::new()).await.unwrap_err();
        assert!(matches!(err, CoreError::HistoryTooLarge { needed, limit, free } if needed == 10 * avg && limit == 10 * avg - 1 && free > 0), "{err:?}");
        assert_eq!(all(&dst, &to).await, originals(10), "nothing was written");
        assert!(spec(&dst, &ns("db2", "fresh")).await.is_none(), "not even the other item's target");
        assert!(entries(&r, &to).is_empty());

        // Items that never record are not counted: Skip passes the same limit.
        let report = copy_recorded(&src, &dst, &[item(from, to.clone(), SKIP)], &r).await;
        assert_eq!((report.items[0].skipped, report.items[0].error.as_deref()), (10, None));
    }

    /// The estimate can be low; the entry is watched as it grows and the item
    /// stops before the overwrite that would pass the limit.
    #[tokio::test]
    async fn an_entry_that_outgrows_the_limit_stops_the_item_before_overwriting() {
        let (src, dst) = (mock(), mock());
        let t = TempDir::new();
        let (from, to) = (ns("db1", "orders"), ns("db2", "orders"));
        seed(&src, &from, 2500).await;
        // Nine small documents and one large one: the average understates the sum of what is saved plus its framing.
        let mut docs: Vec<Document> = (0..9).map(|i| doc! { "_id": i }).collect();
        docs.push(doc! { "_id": 9, "pad": "x".repeat(5000) });
        dst.insert_many(&to, docs.clone()).await.unwrap();
        let avg = dst.coll_stats(&to).await.unwrap().get_i64("avgObjSize").unwrap() as u64;
        let mut r = record(&t);
        r.max_bytes = 10 * avg;
        let report = copy_recorded(&src, &dst, &[item(from, to.clone(), OVERWRITE)], &r).await;
        let out = &report.items[0];
        assert!(out.error.as_deref().is_some_and(|e| e.contains("Collection History would need")), "{out:?}");
        assert_eq!((out.overwritten, out.recorded.clone()), (0, None));
        assert_eq!(out.inserted, 990, "the first batch's other documents went in and are counted");
        assert_eq!(all(&dst, &to).await[..10], docs[..], "nothing was overwritten unsaved");
        assert!(entries(&r, &to).is_empty());
    }

    /// A replace that fails part way: the documents already replaced keep
    /// their hashes; what was saved stays restorable.
    #[tokio::test]
    async fn a_replace_that_fails_part_way_records_what_it_wrote() {
        let (t, (src, dst, from, to)) = (TempDir::new(), with_existing(10).await);
        let r = record(&t);
        let target = dst.clone();
        let hook = move |id: &Bson| {
            if *id == Bson::Int32(5) {
                target.set_read_only(true);
            }
        };
        let report = with_hook(hook, copy_recorded(&src, &dst, &[item(from.clone(), to.clone(), OVERWRITE)], &r)).await;
        let out = &report.items[0];
        assert!(out.error.as_deref().is_some_and(|e| e.contains("read-only")), "{out:?}");
        assert_eq!(out.overwritten, 5);
        assert_eq!(out.warning, None);
        let id = out.recorded.as_ref().expect("what was replaced is kept");
        let s = saved(&r, id);
        assert_eq!(s.len(), 10);
        let source = all(&src, &from).await;
        for (i, saved) in s.iter().enumerate() {
            assert_eq!(saved.before, raw(&doc! { "_id": i as i32, "other": true }));
            let want = (i < 5).then(|| doc_hash(&raw(&source[i])));
            assert_eq!(saved.after_hash, want, "document {i}");
        }
    }

    /// The overwrites of a batch run eight at a time; with history, none is
    /// issued before every target document of the batch is saved and synced.
    #[tokio::test]
    async fn overwrites_run_eight_at_a_time() {
        // With history.
        let (t, (src, dst, from, to)) = (TempDir::new(), with_existing(40).await);
        let r = record(&t);
        dst.set_mock_delay(std::time::Duration::from_millis(2));
        dst.most_calls_at_once();
        crate::history::SYNCS.with(|n| n.set(0));
        let (rr, target) = (r.clone(), to.clone());
        let hook = move |id: &Bson| {
            assert_eq!(crate::history::SYNCS.with(|n| n.get()), 1, "synced before replacing {id}");
            let e = &entries(&rr, &target)[0];
            let befores: Vec<bson::RawDocumentBuf> = saved(&rr, &e.id).into_iter().map(|s| s.before).collect();
            assert_eq!(befores, originals(40).iter().map(raw).collect::<Vec<_>>(), "the whole batch was saved before replacing {id}");
        };
        let report = with_hook(hook, copy_recorded(&src, &dst, &[item(from.clone(), to.clone(), OVERWRITE)], &r)).await;
        dst.set_mock_delay(std::time::Duration::ZERO);
        let out = &report.items[0];
        assert_eq!((out.inserted, out.overwritten, out.error.as_deref()), (2460, 40, None), "{out:?}");
        assert_eq!(dst.most_calls_at_once(), 8);
        assert_eq!(all(&dst, &to).await, all(&src, &from).await);
        assert!(saved(&r, out.recorded.as_ref().unwrap()).iter().all(|s| s.after_hash.is_some()));

        // Without history.
        let (src, dst, from, to) = with_existing(40).await;
        dst.set_mock_delay(std::time::Duration::from_millis(2));
        dst.most_calls_at_once();
        let report = copy(&src, &dst, &[item(from.clone(), to.clone(), OVERWRITE)]).await;
        dst.set_mock_delay(std::time::Duration::ZERO);
        assert_eq!((report.items[0].inserted, report.items[0].overwritten, report.items[0].error.as_deref()), (2460, 40, None));
        assert_eq!(dst.most_calls_at_once(), 8);
        assert_eq!(all(&dst, &to).await, all(&src, &from).await);
    }

    /// A write-concern error on a batch: its documents went in on the
    /// primary but were not confirmed. The item stops and says so (never a
    /// plain failure, which invites a retry), counting them.
    #[tokio::test]
    async fn a_write_concern_error_leaves_the_item_unconfirmed() {
        let (src, dst) = (mock(), mock());
        let (from, to) = (ns("db1", "orders"), ns("db2", "orders"));
        seed(&src, &from, 2500).await;
        dst.concern_fails_next();
        let report = copy(&src, &dst, &[item(from, to.clone(), SKIP)]).await;
        let out = &report.items[0];
        assert!(out.unconfirmed && out.error.as_deref().is_some_and(|e| e.contains("write concern error 64")), "{out:?}");
        assert_eq!(out.inserted, 1000);
        assert_eq!(all(&dst, &to).await.len(), 1000);
    }

    /// An overwrite without history that fails mid-window stops the item:
    /// no more are issued, and those under way finish and count.
    #[tokio::test]
    async fn an_overwrite_failing_mid_window_counts_what_went_through() {
        let (src, dst, from, to) = with_existing(40).await;
        dst.set_mock_delay(std::time::Duration::from_millis(2));
        // The first replace to finish (inserts are not affected) loses its
        // reply; seven more were under way.
        dst.lose_next_reply();
        let report = copy(&src, &dst, &[item(from.clone(), to.clone(), OVERWRITE)]).await;
        dst.set_mock_delay(std::time::Duration::ZERO);
        let out = &report.items[0];
        assert!(out.error.as_deref().is_some_and(|e| e.contains("network error")) && out.unconfirmed, "{out:?}");
        assert_eq!((out.inserted, out.overwritten), (960, 7), "{out:?}");
        let target = all(&dst, &to).await;
        assert_eq!(target[..40].iter().filter(|d| d.get_bool("other").is_err()).count(), 8, "seven confirmed, one whose reply was lost");
    }

    /// An overwrite with history that fails mid-window stops the item: no
    /// more are issued, those under way finish and count with their hashes,
    /// and the entry stays. A refusal is confirmed; a lost reply is not.
    #[tokio::test]
    async fn a_recorded_overwrite_failing_mid_window_counts_what_went_through() {
        // Refused at document 5: 0 to 4 were under way, 6 on never issued.
        let (t, (src, dst, from, to)) = (TempDir::new(), with_existing(40).await);
        let r = record(&t);
        dst.set_mock_delay(std::time::Duration::from_millis(2));
        let target = dst.clone();
        let hook = move |id: &Bson| {
            if *id == Bson::Int32(5) {
                target.set_read_only(true);
            }
        };
        let report = with_hook(hook, copy_recorded(&src, &dst, &[item(from.clone(), to.clone(), OVERWRITE)], &r)).await;
        dst.set_mock_delay(std::time::Duration::ZERO);
        dst.set_read_only(false);
        let out = &report.items[0];
        assert!(out.error.as_deref().is_some_and(|e| e.contains("read-only")) && !out.unconfirmed, "{out:?}");
        assert_eq!((out.inserted, out.overwritten), (960, 5), "{out:?}");
        let source = all(&src, &from).await;
        let s = saved(&r, out.recorded.as_ref().expect("what was replaced is kept"));
        assert_eq!(s.iter().map(|s| s.before.clone()).collect::<Vec<_>>(), originals(40).iter().map(raw).collect::<Vec<_>>());
        assert_eq!(s.iter().map(|s| s.after_hash).collect::<Vec<_>>(), (0..40).map(|i| (i < 5).then(|| doc_hash(&raw(&source[i])))).collect::<Vec<_>>());

        // The first replace to finish loses its reply: seven confirmed, one unknown.
        let (t, (src, dst, from, to)) = (TempDir::new(), with_existing(40).await);
        let r = record(&t);
        dst.set_mock_delay(std::time::Duration::from_millis(2));
        dst.lose_next_reply();
        let report = copy_recorded(&src, &dst, &[item(from.clone(), to.clone(), OVERWRITE)], &r).await;
        dst.set_mock_delay(std::time::Duration::ZERO);
        let out = &report.items[0];
        assert!(out.error.as_deref().is_some_and(|e| e.contains("network error")) && out.unconfirmed, "{out:?}");
        assert_eq!((out.inserted, out.overwritten), (960, 7), "{out:?}");
        let s = saved(&r, out.recorded.as_ref().expect("what may have been replaced is kept"));
        assert_eq!(s.len(), 40);
        assert_eq!(s.iter().filter(|s| s.after_hash.is_some()).count(), 7, "hashes for the confirmed replaces only");
        let replaced = all(&dst, &to).await[..40].iter().filter(|d| d.get_bool("other").is_err()).count();
        assert_eq!(replaced, 8, "seven confirmed, one whose reply was lost");
    }

    /// Nothing replaced: no entry, whatever stopped the item.
    #[tokio::test]
    async fn an_item_that_replaced_nothing_leaves_no_entry() {
        let (t, (src, dst, from, to)) = (TempDir::new(), with_existing(10).await);
        let r = record(&t);
        let target = dst.clone();
        let hook = move |id: &Bson| {
            if *id == Bson::Int32(0) {
                target.set_read_only(true);
            }
        };
        let report = with_hook(hook, copy_recorded(&src, &dst, &[item(from, to.clone(), OVERWRITE)], &r)).await;
        let out = &report.items[0];
        assert!(out.error.as_deref().is_some_and(|e| e.contains("read-only")), "{out:?}");
        assert_eq!((out.overwritten, out.recorded.clone()), (0, None));
        assert!(entries(&r, &to).is_empty());
        dst.set_read_only(false);
        assert_eq!(all(&dst, &to).await[..10], originals(10));
    }

    /// Overwriting 400 target documents on a demo server that waits 5 ms a
    /// call, as a round trip to a slow server would; with and without history.
    /// `cargo test -p rongo-core --release --lib -- --ignored --nocapture report_overwrite_time`
    #[tokio::test]
    #[ignore = "timing; run explicitly in release mode"]
    async fn report_overwrite_time() {
        for history in [false, true] {
            let (t, (src, dst, from, to)) = (TempDir::new(), with_existing(400).await);
            let r = record(&t);
            dst.set_mock_delay(std::time::Duration::from_millis(5));
            let t0 = std::time::Instant::now();
            let items = [item(from.clone(), to.clone(), OVERWRITE)];
            let report = if history { copy_recorded(&src, &dst, &items, &r).await } else { copy(&src, &dst, &items).await };
            let took = t0.elapsed();
            dst.set_mock_delay(std::time::Duration::ZERO);
            assert_eq!((report.items[0].overwritten, report.items[0].error.as_deref()), (400, None));
            println!("paste overwriting 400 of 2,500 documents, 5 ms a call, history {history}: {took:?}");
        }
    }

    /// Retention that can't read part of the history is a warning on the item.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_retention_problem_is_a_warning_on_the_item() {
        let (t, (src, dst, from, to)) = (TempDir::new(), with_existing(10).await);
        let r = record(&t);
        let closed = r.root.join(uuid::Uuid::from_u128(7).to_string());
        std::fs::create_dir_all(&closed).unwrap();
        std::fs::set_permissions(&closed, std::os::unix::fs::PermissionsExt::from_mode(0o000)).unwrap();
        let report = copy_recorded(&src, &dst, &[item(from.clone(), to.clone(), OVERWRITE)], &r).await;
        std::fs::set_permissions(&closed, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        let out = &report.items[0];
        assert_eq!((out.inserted, out.overwritten, out.error.as_deref()), (2490, 10, None), "{out:?}");
        assert!(out.warning.as_deref().is_some_and(|w| w.starts_with("Collection History could not be trimmed: ") && w.contains("ermission denied")), "{out:?}");
        assert_eq!(out.recorded.as_ref(), Some(&entries(&r, &to)[0].id));
    }

    /// An entry that could not be finished is a warning on a copy that went through.
    #[tokio::test]
    async fn an_entry_that_cannot_be_finished_is_a_warning_not_an_error() {
        let (t, (src, dst, from, to)) = (TempDir::new(), with_existing(10).await);
        let r = record(&t);
        // The history disappears after the first batch.
        let root = r.root.clone();
        let progress = Progress::with_hook(move |_| {
            let _ = std::fs::remove_dir_all(&root);
        });
        let options = CopyOptions { history: Some(r.clone()), ..Default::default() };
        let report = copy_collections(&src, &dst, false, &[item(from.clone(), to.clone(), OVERWRITE)], options, &progress, &CancellationToken::new()).await.unwrap();
        let out = &report.items[0];
        assert_eq!((out.inserted, out.overwritten, out.error.as_deref()), (2490, 10, None), "{out:?}");
        assert!(out.warning.as_deref().is_some_and(|w| w.starts_with("Collection History may be incomplete")), "{out:?}");
        assert_eq!(all(&dst, &to).await, all(&src, &from).await);
    }
}
