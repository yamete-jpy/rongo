//! What a write, a restore or a paste did, in the words of its toast.

use rongo_core::{CopyReport, CoreError, Event, Namespace, WriteOutcome};

use crate::result_view::thousands;

/// What the app says of a write the server did not confirm: never "nothing
/// was written", which invites the retry that could apply it twice.
const UNCONFIRMED: &str = "The server did not confirm this write: it may have been applied. Check before retrying.";

fn plural(n: u64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// `UNCONFIRMED`, then what was confirmed (`confirmed`, starting lowercase;
/// none when empty) and why. `unsure`: documents counted that went in on
/// the primary under a write-concern error (`how`: "inserted", say), left
/// out of `confirmed` and said apart; without them, what was confirmed came
/// before the write that was not.
fn unconfirmed_text(confirmed: &str, unsure: u64, how: &str, why: Option<&str>) -> String {
    let mut s = UNCONFIRMED.to_string();
    match (confirmed.is_empty(), unsure) {
        (true, _) => {}
        (false, 0) => s.push_str(&format!(" Confirmed before it: {confirmed}.")),
        (false, _) => s.push_str(&format!(" Confirmed: {confirmed}.")),
    }
    if unsure > 0 {
        s.push_str(&format!(" Not confirmed: {} document{} {how}.", thousands(unsure), plural(unsure)));
    }
    if let Some(why) = why {
        s.push_str(&format!(" ({why})"));
    }
    s
}

/// A write's counts, starting lowercase, `inserted` of its inserts among them.
fn counts(w: &WriteOutcome, inserted: u64) -> Vec<String> {
    let mut parts = Vec::new();
    if inserted > 0 {
        parts.push(format!("inserted {} document{}", thousands(inserted), plural(inserted)));
    }
    // An import leaves out what is there already.
    if w.duplicates > 0 {
        parts.push(format!("skipped {} with an _id already in the collection", thousands(w.duplicates)));
    }
    if w.matched > 0 || w.modified > 0 {
        parts.push(format!("updated {} of {} matched", thousands(w.modified), thousands(w.matched)));
    }
    if w.deleted > 0 {
        parts.push(format!("deleted {} document{}", thousands(w.deleted), plural(w.deleted)));
    }
    if w.inserted_id.is_some() && w.inserted == 0 {
        parts.push("upserted 1 document".into());
    }
    parts
}

/// A one-line summary of a write's outcome for a toast.
pub(super) fn outcome_text(ev: &Event) -> String {
    match ev {
        Event::Written(w) => {
            let parts = counts(w, w.inserted);
            if w.unconfirmed {
                let confirmed = counts(w, w.inserted.saturating_sub(w.unconfirmed_docs));
                return unconfirmed_text(&confirmed.join(", "), w.unconfirmed_docs, "inserted", w.warning.as_deref());
            }
            let mut s = match (parts.is_empty(), w.cancelled) {
                (true, true) => "Stopped before anything was changed".to_string(),
                // Every document of an import refused, say.
                (true, false) if w.warning.is_some() => "Nothing was written".to_string(),
                (true, false) => "No documents matched".to_string(),
                (false, _) => {
                    let mut s = parts.join(", ");
                    s[..1].make_ascii_uppercase();
                    s
                }
            };
            // A recorded write that stopped between batches, or finished with
            // a problem after writing: the counts above are what it did.
            match (w.cancelled && !parts.is_empty(), &w.warning) {
                (true, None) => s.push_str(", then stopped (cancelled)"),
                (true, Some(why)) => s.push_str(&format!(", then stopped: {why}")),
                (false, Some(why)) => s.push_str(&format!("; {why}")),
                (false, None) => {}
            }
            s
        }
        Event::Exported { count, path } => format!("Exported {} document{} to {}", thousands(*count), plural(*count), path.display()),
        Event::Document(d) if d.contains_key("createdIndex") => format!("Created index {}", d.get_str("createdIndex").unwrap_or("?")),
        Event::Document(d) if d.contains_key("existingIndex") => format!("Index {} already exists; nothing was created", d.get_str("existingIndex").unwrap_or("?")),
        _ => "Done".into(),
    }
}

/// Whether an outcome says the write did nothing: an index that was
/// already there, identical (its toast mustn't read "… created").
pub(super) fn did_nothing(ev: &Event) -> bool {
    matches!(ev, Event::Document(d) if d.contains_key("existingIndex"))
}

/// Whether a write's outcome carries a problem to show as an error: a
/// recorded write that stopped on one, or whose history may be incomplete;
/// or an index that already existed, so nothing was created.
pub(super) fn troubled(ev: &Event) -> bool {
    matches!(ev, Event::Written(w) if w.warning.is_some() || w.unconfirmed) || did_nothing(ev)
}

/// What a restore did, for its toast, and whether to show it as an error:
/// what was put back (overwritten and new-`_id` ones among it), what was
/// skipped, a stop, and a warning.
pub(super) fn restored_toast(ns: &Namespace, ev: &Event) -> (String, bool) {
    let Event::Restored { restored, skipped, overwritten, inserted_new, cancelled, warning, unconfirmed, unconfirmed_docs, .. } = ev else { return ("Done".into(), false) };
    let put = restored + overwritten + inserted_new;
    if *unconfirmed {
        let mut confirmed = Vec::new();
        let sure = put.saturating_sub(*unconfirmed_docs);
        if sure > 0 {
            confirmed.push(format!("restored {} document{} into {ns}", thousands(sure), plural(sure)));
        }
        if *skipped > 0 {
            confirmed.push(format!("{} skipped", thousands(*skipped)));
        }
        return (unconfirmed_text(&confirmed.join(", "), *unconfirmed_docs, "restored", warning.as_deref()), true);
    }
    let mut s = if put > 0 {
        let mut s = format!("Restored {} document{} into {ns}", thousands(put), plural(put));
        let mut how = Vec::new();
        if *overwritten > 0 {
            how.push(format!("{} overwritten", thousands(*overwritten)));
        }
        if *inserted_new > 0 {
            how.push(format!("{} with new _ids", thousands(*inserted_new)));
        }
        if !how.is_empty() {
            s.push_str(&format!(" ({})", how.join(", ")));
        }
        s
    } else if *cancelled && *skipped == 0 {
        let mut s = "The restore stopped before anything was restored".to_string();
        if let Some(w) = warning {
            s.push_str(&format!(": {w}"));
        }
        return (s, warning.is_some());
    } else {
        format!("Nothing restored into {ns}")
    };
    if *skipped > 0 {
        s.push_str(&format!(", {} skipped", thousands(*skipped)));
    }
    match (cancelled, warning) {
        (true, None) => s.push_str(", then stopped (cancelled)"),
        (true, Some(w)) => s.push_str(&format!(", then stopped: {w}")),
        (false, Some(w)) => s.push_str(&format!("; {w}")),
        (false, None) => {}
    }
    (s, warning.is_some())
}

/// Why a renamed collection's history didn't all move, said after
/// "Collection History: ". A move that failed may have stopped part way (it
/// moves entry by entry when the new name already has a history), so it
/// doesn't say where the history is: some entries may be under each name,
/// both listed in Settings.
pub(super) fn move_failed(from: &Namespace, to: &Namespace, e: CoreError) -> String {
    format!("{from}'s entries could not all be moved to {} after the rename ({e}): some entries may be under each name; Settings → Collection History lists both", to.coll)
}

/// Whether any item of a finished copy stopped on an error.
pub(super) fn copy_failed(r: &CopyReport) -> bool {
    r.items.iter().any(|i| i.error.is_some())
}

/// Whether a finished copy's toast shows as an error: an item stopped on
/// one, or carries a warning (such as a history that may be incomplete),
/// though that item still counts as pasted.
pub(super) fn copy_troubled(r: &CopyReport) -> bool {
    copy_failed(r) || r.items.iter().any(|i| i.warning.is_some())
}

/// What a copy did, for a toast, without saying which connection it went to.
pub(super) fn copy_summary(r: &CopyReport) -> String {
    copy_toast(r, None)
}

/// What a copy did: the collections and documents written, what was
/// skipped or overwritten, and each collection that stopped on an error.
/// `conn` names the connection pasted into.
pub(super) fn copy_toast(r: &CopyReport, conn: Option<&str>) -> String {
    let plural = |n: u64| if n == 1 { "" } else { "s" };
    let docs: u64 = r.items.iter().map(|i| i.inserted).sum();
    let skipped: u64 = r.items.iter().map(|i| i.skipped).sum();
    let overwritten: u64 = r.items.iter().map(|i| i.overwritten).sum();
    let done = r.items.iter().filter(|i| i.error.is_none()).count() as u64;
    let mut dbs: Vec<&str> = Vec::new();
    for i in &r.items {
        if !dbs.contains(&i.dst.db.as_str()) {
            dbs.push(&i.dst.db);
        }
    }
    let dbs = dbs.join(", ");
    let into = match (conn, dbs.is_empty()) {
        (_, true) => String::new(),
        (Some(c), false) => format!(" into {c} · {dbs}"),
        (None, false) => format!(" into {dbs}"),
    };
    let mut s = if r.cancelled {
        format!("Copy cancelled after {} document{}{into}", thousands(docs), plural(docs))
    } else if into.is_empty() {
        format!("Pasted {done} collection{}", plural(done))
    } else {
        format!("Pasted {done} collection{}{into}: {} document{}", plural(done), thousands(docs), plural(docs))
    };
    if skipped > 0 {
        s.push_str(&format!(", {} skipped", thousands(skipped)));
    }
    if overwritten > 0 {
        s.push_str(&format!(", {} overwritten", thousands(overwritten)));
    }
    for i in &r.items {
        match &i.error {
            Some(e) if i.unconfirmed => s.push_str(&format!("; {}: the server did not confirm a write: it may have been applied. Check before retrying. ({e})", i.dst.coll)),
            Some(e) => s.push_str(&format!("; {} failed: {e}", i.dst.coll)),
            None => {}
        }
    }
    for i in &r.items {
        if let Some(w) = &i.warning {
            s.push_str(&format!("; {}: {w}", i.dst.coll));
        }
    }
    s
}
