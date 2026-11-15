//! Collection History: what a write keeps, the History tabs and restores, and the history files.

use std::path::PathBuf;

use rongo_core::history::{EntryId, HistoryKey, Kind, Store};
use rongo_core::bson::doc;
use rongo_core::{CollKind, Command, ConnId, CopyOptions, CoreError, Event, FindSpec, IfExists, Namespace, OnConflict, OnDuplicateId, Record, RequestId, Waker};

use crate::history_tab::{HistoryAction, HistoryTab, RestoreAsk, Running, Step, WRITING};
use crate::dialogs::{Confirm, Pending};
use crate::net::{HistoryReq, Origin};
use crate::sidebar::OpenConn;
use crate::settings::{HISTORY_ALL_IN_USE, HISTORY_IN_USE, HistorySettings, mb};
use crate::tools::HistoryOffer;
use crate::tabs::TabKind;

use super::RongoApp;
use super::outcome::restored_toast;

impl RongoApp {
    /// Tests: where Collection History is kept (a temporary folder of each
    /// test app's own).
    #[cfg(test)]
    pub fn history_root(&self) -> PathBuf {
        self.history_root.clone().expect("a test app has a history folder")
    }

    /// Where Collection History is kept (`history/` in rongo's data
    /// folder) and its key (it is in the vault), or why they can't
    /// be had.
    fn history_at(&self) -> Result<(PathBuf, HistoryKey), String> {
        let Some(root) = &self.history_root else { return Err("there is no data folder to keep it in".into()) };
        let key = self.vault.as_ref().map(|v| v.history_key()).ok_or_else(|| "the vault is locked".to_string())?;
        Ok((root.clone(), key))
    }

    /// The Collection History key (it is in the vault), or why it can't be had.
    pub(super) fn history_key(&self) -> Result<HistoryKey, String> {
        self.history_at().map(|(_, key)| key)
    }

    /// The history, for listing and reading entries; None when its key can't be read.
    pub fn history_store(&self) -> Option<Store> {
        self.history_at().ok().map(|(root, key)| Store::new(root, key))
    }

    /// Whether a write of `kind` on `ns` should keep what it changes: the
    /// connection's own choice, else Settings; the kind switched on; and
    /// not a time series collection (as the tree listed it; one not listed
    /// yet is kept).
    fn history_wanted(&self, conn: ConnId, ns: &Namespace, kind: Kind) -> bool {
        let Some(c) = self.conns.get(&conn) else { return false };
        let h = &self.settings.history;
        let kind_on = match kind {
            Kind::Delete => h.deletes,
            Kind::Update => h.updates,
        };
        c.profile.history.unwrap_or(h.enabled) && kind_on && listed_kind(c, ns) != Some(CollKind::Timeseries)
    }

    /// Where and how to keep what a write of `kind` on `ns` deletes or
    /// changes; None when history is off for it or its key can't be read.
    pub fn record_for(&self, conn: ConnId, ns: &Namespace, kind: Kind) -> Option<Record> {
        if !self.history_wanted(conn, ns, kind) {
            return None;
        }
        let (root, key) = self.history_at().ok()?;
        let c = self.conns.get(&conn)?;
        let h = &self.settings.history;
        Some(Record { root, key, profile: c.profile.id, conn_name: c.profile.name.clone(), keep_days: h.keep_days, max_bytes: h.max_bytes() })
    }

    /// Where a restore on `conn` reads its entry (of `profile`) and keeps
    /// what it replaces. Settings don't turn this off: a restore always
    /// keeps what it overwrites, so it can be undone. Only the key can be
    /// missing, and then nothing can be read to restore either.
    fn restore_record(&self, conn: ConnId, profile: uuid::Uuid) -> Result<Record, String> {
        let (root, key) = self.history_at()?;
        let h = &self.settings.history;
        let conn_name = self.conns.get(&conn).map(|c| c.profile.name.clone()).unwrap_or_default();
        Ok(Record { root, key, profile, conn_name, keep_days: h.keep_days, max_bytes: h.max_bytes() })
    }

    /// What the Update/Delete dialog says about history for `ns`.
    pub(super) fn history_offer(&self, conn: ConnId, ns: &Namespace, kind: Kind) -> HistoryOffer {
        if !self.history_wanted(conn, ns, kind) {
            return HistoryOffer::Off;
        }
        match self.history_key() {
            Ok(_) => HistoryOffer::On,
            Err(why) => HistoryOffer::Unavailable(why),
        }
    }

    /// The history a write would keep, when it is wanted: the connection it
    /// belongs to (a paste's target), a collection it changes, the kind,
    /// and the name to type to go on without it (a paste overwriting
    /// several collections: the database's, as the Paste dialog asks).
    fn history_plan(&self, conn: ConnId, cmd: &Command) -> Option<(ConnId, Namespace, Kind, String)> {
        if let Command::CopyCollections { to, items, .. } = cmd {
            // Append + Overwrite replaces target documents; nothing else is kept.
            let over: Vec<&Namespace> = items
                .iter()
                .filter(|i| i.if_exists == IfExists::Append(OnDuplicateId::Overwrite))
                .map(|i| &i.dst)
                .filter(|ns| self.history_wanted(*to, ns, Kind::Update))
                .collect();
            let name = match over.as_slice() {
                [] => return None,
                [one] => one.coll.clone(),
                [first, ..] => first.db.clone(),
            };
            return Some((*to, over[0].clone(), Kind::Update, name));
        }
        let (ns, kind) = cmd.recorded_target()?;
        self.history_wanted(conn, ns, kind).then(|| (conn, ns.clone(), kind, ns.coll.clone()))
    }

    /// Sends a write, keeping the documents it deletes or changes in
    /// Collection History first when history is on for it and `keep` (a
    /// dialog's box) is ticked. When history is wanted but its key can't be
    /// read, asks before going on without it and sends nothing yet (None);
    /// when the vault can't be caught up first (`history_key_current`),
    /// sends nothing (None). Commands that change no documents are sent as
    /// they are.
    pub(super) fn send_write(&mut self, origin: Origin, conn: ConnId, cmd: Command, keep: bool) -> Option<RequestId> {
        let plan = if keep { self.history_plan(conn, &cmd) } else { None };
        let Some((owner, ns, kind, name)) = plan else { return Some(self.net().send(origin, Some(conn), cmd)) };
        match self.record_for(owner, &ns, kind) {
            Some(record) => {
                if !self.history_key_current() {
                    return None;
                }
                let recorded = match cmd.clone() {
                    Command::CopyCollections { to, items, options } => Command::CopyCollections { to, items, options: CopyOptions { history: Some(record), ..options } },
                    plain => Command::Recorded { record, cmd: Box::new(plain) },
                };
                let id = self.net().send(origin, Some(conn), recorded);
                self.requests.unrecorded.insert(id, Unrecorded { cmd, name });
                Some(id)
            }
            None => {
                let why = self.history_key().err().unwrap_or_default();
                let why = format!("Collection History is unavailable: {}.", why.trim_end_matches('.'));
                self.ask_without_history(origin, conn, cmd, name, why);
                None
            }
        }
    }

    /// Whether `cmd` may be sent as far as Collection History goes: when it
    /// would be recorded, the vault is caught up first (`history_key_current`,
    /// which says why not). Asked before a question or dialog is let go, so
    /// that a refusal keeps it for the user to go on with.
    pub(super) fn history_ready(&mut self, conn: ConnId, cmd: &Command) -> bool {
        match self.history_plan(conn, cmd) {
            Some((owner, ns, kind, _)) if self.record_for(owner, &ns, kind).is_some() => self.history_key_current(),
            _ => true,
        }
    }

    /// "Keep no history for this operation?", saying `why`; going on sends
    /// `cmd` as it is, once `name` is typed. It may come when the core
    /// answers, by when another question can be open (another write's):
    /// that one is never replaced unanswered; `cmd` is not sent, and a
    /// toast says so, to try again.
    pub(super) fn ask_without_history(&mut self, origin: Origin, conn: ConnId, cmd: Command, name: String, why: String) {
        if self.confirm.is_some() {
            return self.toast(format!("{why} The operation on {name} was not sent, as another question is open: answer it, then try again."), true);
        }
        let msg = format!("{why}\n\nGo on without keeping the documents it deletes or changes? Type {name} to confirm.");
        let pending = Pending::WithoutHistory { origin, conn, cmd: Box::new(cmd), name: name.clone() };
        self.confirm = Some(Confirm::new(WITHOUT_HISTORY, msg, "Go On Without History", pending).danger().input(String::new()).must_type(name));
    }

    /// The collection's Collection History tab. It opens for a collection
    /// the tree doesn't list (one dropped since), but not for a time series
    /// collection or a view, which keep no history.
    pub(super) fn open_history(&mut self, cid: ConnId, ns: Namespace) {
        if self.focus_tab(|k| matches!(k, TabKind::History(h) if h.conn == cid && h.ns == ns)) {
            return;
        }
        if let Some(kind @ (CollKind::Timeseries | CollKind::View)) = self.conns.get(&cid).and_then(|c| listed_kind(c, &ns)) {
            let what = if kind == CollKind::View { "A view" } else { "A time series collection" };
            self.toast(format!("{what} keeps no Collection History"), true);
            return;
        }
        let Some(store) = self.history_store() else {
            let why = self.history_key().err().unwrap_or_default();
            self.toast(format!("Collection History is unavailable: {why}"), true);
            return;
        };
        // History belongs to the saved profile the connection was opened from.
        self.push_tab(|_| TabKind::History(Box::new(HistoryTab::new(cid, cid.0, ns, store))));
    }

    pub(super) fn history_tab_mut(&mut self, tab: u64) -> Option<&mut HistoryTab> {
        self.tabs.iter_mut().find(|t| t.id == tab).and_then(|t| match &mut t.kind {
            TabKind::History(h) => Some(h.as_mut()),
            _ => None,
        })
    }

    pub(super) fn history_action(&mut self, tab: u64, a: HistoryAction) {
        let tz = self.settings.timezone;
        let writing = match self.tabs.iter().find(|t| t.id == tab).map(|t| &t.kind) {
            Some(TabKind::History(h)) => self.history_busy(Some(h.conn.0), Some(&h.ns)),
            _ => return,
        };
        let Some(h) = self.history_tab_mut(tab) else { return };
        let (conn, profile, ns, count) = (h.conn, h.profile, h.ns.clone(), h.entries().len());
        match a {
            HistoryAction::Restore(entry) => {
                if h.running.is_some() || !h.entries().iter().any(|e| e.id == entry) {
                    return;
                }
                // One restore question at a time: another check's answer
                // (its question) is on its way or open.
                if self.restore_ask.is_some() || self.requests.pending.values().any(|s| matches!(s.origin, Origin::History { req: HistoryReq::Check, .. })) {
                    self.toast("Another restore is being checked: answer its question first", true);
                    return;
                }
                // First what conflicts; the restore follows (asking if anything does).
                self.send_history(tab, entry, Step::Checking, |record, entry| Command::HistoryConflicts { record, entry });
            }
            HistoryAction::Compare(ids) => {
                // One query for every previewed `_id`; the values go as BSON.
                let spec = FindSpec { filter: doc! { "_id": { "$in": &ids } }, ..FindSpec::all(ns.clone(), ids.len().max(1) as i64) };
                let origin = Origin::History { tab, ns, req: HistoryReq::Compare };
                let req = self.net().send(origin, Some(conn), Command::Find(spec));
                if let Some(h) = self.history_tab_mut(tab) {
                    h.comparing(req);
                }
            }
            HistoryAction::Remove(_) | HistoryAction::Empty if let Some(why) = h.removal_blocked().or(writing.then_some(WRITING)) => {
                self.toast(why, true);
            }
            HistoryAction::Remove(entry) => {
                let what = h.describe(&entry, tz);
                let msg = format!("Remove {what} from Collection History? Its saved documents can't be restored afterwards.");
                self.confirm = Some(Confirm::new("Remove from History", msg, "Remove", Pending::RemoveHistory { tab, entry }).danger());
            }
            HistoryAction::Empty => {
                let entries = if count == 1 { "its 1 entry".to_string() } else { format!("all {count} entries") };
                let msg = format!("Remove {entries} of {ns} from Collection History? Their saved documents can't be restored afterwards.");
                let before = rongo_core::bson::DateTime::now().timestamp_millis();
                self.confirm = Some(Confirm::new("Empty Collection History", msg, "Remove All", Pending::EmptyHistory { profile, ns, before }).danger());
            }
            HistoryAction::View(view, a) => self.view_action(view, a),
        }
    }

    /// Restores an entry, conflicts going `on_conflict` (once the restore
    /// question was answered).
    fn send_restore(&mut self, tab: u64, entry: EntryId, on_conflict: OnConflict) {
        // Locked while the question was open: `send_history` says so.
        self.send_history(tab, entry, Step::Restoring, |record, entry| Command::HistoryRestore { record, entry, on_conflict });
    }

    /// Sends a History tab's conflict check or restore of `entry` (`step`
    /// says which; `cmd` makes it from where the entry is read and what it
    /// overwrites is kept), and the tab waits for it. Nothing is sent, and a
    /// toast says why, while the connection is locked or the key can't be
    /// had; nor when the vault can't be caught up (`history_key_current`).
    fn send_history(&mut self, tab: u64, entry: EntryId, step: Step, cmd: impl FnOnce(Record, EntryId) -> Command) {
        let Some(h) = self.history_tab_mut(tab) else { return };
        let (conn, profile, ns) = (h.conn, h.profile, h.ns.clone());
        if self.conns.get(&conn).is_some_and(|c| c.profile.read_only) {
            return self.toast(crate::result_view::LOCKED, true);
        }
        let record = match self.restore_record(conn, profile) {
            Ok(r) => r,
            Err(why) => return self.toast(format!("Collection History is unavailable: {why}"), true),
        };
        if !self.history_key_current() {
            return;
        }
        let req = match step {
            Step::Checking => HistoryReq::Check,
            Step::Restoring => HistoryReq::Restore,
        };
        let id = self.net().send(Origin::History { tab, ns, req }, Some(conn), cmd(record, entry.clone()));
        if let Some(h) = self.history_tab_mut(tab) {
            h.running = Some(Running { req: id, entry, step, cancelling: false });
        }
    }

    /// The answer to the restore question: restore as chosen there, or not at all.
    pub(super) fn restore_answer(&mut self, go: bool) {
        let Some(ask) = self.restore_ask.take() else { return };
        if go {
            self.send_restore(ask.tab, ask.entry, ask.on_conflict);
        }
    }

    /// Whether the history of `profile` (any, when None) on `ns` (any,
    /// when None) is being written now (`Requests::in_use`).
    pub(super) fn history_busy(&self, profile: Option<uuid::Uuid>, ns: Option<&Namespace>) -> bool {
        self.requests.in_use().any(|(p, n)| profile.is_none_or(|q| q == p) && ns.is_none_or(|m| m == n))
    }

    /// Why Rename waits, if it does: a collection's history moves with it,
    /// so not while some of it is being written, nor while Settings is
    /// changing the history files (or has a change waiting).
    pub(super) fn rename_blocked(&self, conn: ConnId, ns: &Namespace) -> Option<String> {
        if self.history_busy(Some(conn.0), Some(ns)) {
            return Some(format!("Wait for the delete, update or restore on {ns} to finish before renaming it: its Collection History is still being written"));
        }
        let doing = self.history_files.doing()?;
        Some(format!("Wait until Collection History is done changing ({}) before renaming {ns}", doing.trim_end_matches('…')))
    }

    /// The delete-connection question, once the profile's history is
    /// measured: how much it holds, or (none) no offer to delete it.
    fn measure_delete_offer(&mut self) {
        let Some(c) = &mut self.confirm else { return };
        let Pending::DeleteProfile { id } = c.pending else { return };
        if c.checkbox.as_ref().is_none_or(|(label, _)| label != DELETE_ITS_HISTORY) || !self.history_files.idle() {
            return;
        }
        // Not measured: still offered, without a size.
        let Some(Ok(s)) = self.history_files.scan() else { return };
        fit_offer(&mut c.checkbox, DELETE_ITS_HISTORY, s.by_profile.get(&id).copied().unwrap_or(0));
    }

    /// Takes in the history files' finished work: a change lists open
    /// History tabs again (and says if it failed). Starts the next change
    /// waiting (deleting, clearing, trimming) only while nothing is being
    /// written where it removes entries, checked now rather than when it
    /// was queued.
    pub(super) fn poll_history_files(&mut self) {
        if let Some(done) = self.history_files.poll() {
            if let Err(e) = done {
                self.toast(format!("Collection History: {e}"), true);
            }
            for t in &mut self.tabs {
                if let TabKind::History(h) = &mut t.kind {
                    h.reload();
                }
            }
        }
        if self.history_files.next().is_some_and(|(profile, ns)| !self.history_busy(profile, ns)) {
            self.history_files.start_next(&self.ctx);
        }
        self.measure_delete_offer();
        self.measure_reset_offer();
    }

    /// The start-up purge finished: what it could not read or remove is
    /// said, since the history may then stay over its limits until the next
    /// trim.
    pub(super) fn poll_startup_purge(&mut self) {
        if !self.startup_purge.as_ref().is_some_and(|h| h.is_finished()) {
            return;
        }
        let Some(purge) = self.startup_purge.take() else { return };
        let why = match purge.join() {
            Ok(Ok(_)) => return,
            Ok(Err(CoreError::History(why))) => why,
            Ok(Err(e)) => e.to_string(),
            Err(_) => "it stopped unexpectedly".into(),
        };
        self.toast(format!("Collection History could not be trimmed at start-up: {why}"), true);
    }

    /// An answer for a History tab: the conflict check (then the question
    /// every restore asks), the restore's outcome, or the documents "Show
    /// changes" compares with.
    pub(super) fn history_answer(&mut self, tab: u64, ns: Namespace, kind: HistoryReq, req: RequestId, conn: Option<ConnId>, event: Event) {
        let (ctx, tz) = (self.ctx.clone(), self.settings.timezone);
        let h = self.history_tab_mut(tab);
        match (kind, event) {
            (HistoryReq::Check, Event::HistoryConflicts { total, conflicts, sample }) => {
                let Some(h) = h else { return };
                // Cancel pressed while the answer was on its way: stop here.
                let Some(run) = h.running.take_if(|r| r.req == req).filter(|r| !r.cancelling) else { return };
                let Some(entry) = h.entries().iter().find(|e| e.id == run.entry).cloned() else { return };
                let target = h.conn;
                self.restore_ask = Some(RestoreAsk::new(tab, &entry, tz, self.conns.get(&target), total, conflicts, &sample));
            }
            (HistoryReq::Restore, ev @ Event::Restored { .. }) => {
                if let Some(h) = h {
                    h.running.take_if(|r| r.req == req);
                    h.reload();
                }
                let (text, error) = restored_toast(&ns, &ev);
                self.toast(text, error);
                if let Some(conn) = conn {
                    if let Some(v) = self.view_for(conn, &ns).and_then(|v| self.view_mut(v)) {
                        v.refresh();
                    }
                    // Restoring into a dropped collection (or database) made it again.
                    if self.conns.get(&conn).is_some_and(|c| c.dbs.is_some() && listed_kind(c, &ns).is_none()) {
                        self.reload_database(conn, &ns.db);
                    }
                }
            }
            (HistoryReq::Compare, Event::Page { docs, .. }) => {
                if let Some(h) = h {
                    h.on_current(&ctx, req, docs);
                }
            }
            // A failed comparison shows in its tab, if that is still open.
            (HistoryReq::Compare, Event::Failed(e)) => {
                if let Some(h) = h {
                    h.on_compare_failed(req, e.to_string());
                }
            }
            (HistoryReq::Check | HistoryReq::Restore, Event::Failed(e)) => {
                if let Some(h) = h {
                    h.running.take_if(|r| r.req == req);
                }
                // Cancelled: nothing to say.
                if e != CoreError::Cancelled {
                    let what = match kind {
                        HistoryReq::Check => format!("Checking {ns} for conflicts failed"),
                        _ => format!("Restoring into {ns} failed"),
                    };
                    self.toast(format!("{what}: {e}"), true);
                }
            }
            _ => {}
        }
    }

    /// After Remove from History or Empty History: say if it failed, and list again.
    fn history_removed(&mut self, tab: u64, removed: Option<rongo_core::CoreResult<()>>) {
        match removed {
            Some(Ok(())) => {}
            Some(Err(CoreError::History(why))) => self.toast(format!("Could not remove it from Collection History: {why}"), true),
            Some(Err(e)) => self.toast(format!("Could not remove it from Collection History: {e}"), true),
            None => self.toast("Collection History is unavailable", true),
        }
        if let Some(h) = self.history_tab_mut(tab) {
            h.reload();
        }
    }

    /// Removes `entry` from Collection History, once confirmed; not while
    /// a write begun since the question may be writing an entry there.
    pub(super) fn remove_history_entry(&mut self, tab: u64, entry: EntryId) {
        if self.history_busy(Some(entry.profile), Some(&entry.ns)) {
            return self.toast(WRITING, true);
        }
        let removed = self.history_store().map(|s| s.remove(&entry));
        self.history_removed(tab, removed);
    }

    /// Empties a collection's history, once confirmed: off the UI thread,
    /// as Settings deletes (a walk and an unlink per entry, maybe
    /// thousands); the History tabs list again when it is done
    /// (`poll_history_files`), and it starts only while nothing is being
    /// written there. Only entries recorded by `before` (ms, when the user
    /// asked) go: one recorded while it waited keeps its documents.
    pub(super) fn empty_history(&mut self, profile: uuid::Uuid, ns: Namespace, before: i64) {
        if self.history_busy(Some(profile), Some(&ns)) {
            return self.toast(WRITING, true);
        }
        let gone = ns.clone();
        self.history_files.queue("Emptying a collection's history…", Some(profile), Some(ns), move |s| s.empty(profile, &gone, before));
    }

    /// Deletes a collection's history from Settings, once confirmed: off
    /// the UI thread, then measured again; never while some of it is being
    /// written (a write may have begun since the question, or begin before
    /// it starts). Only entries recorded by `before` (ms, when the user
    /// asked) go, as for Empty.
    pub(super) fn delete_history(&mut self, profile: uuid::Uuid, ns: Namespace, before: i64) {
        if self.history_busy(Some(profile), Some(&ns)) {
            return self.toast(HISTORY_IN_USE, true);
        }
        let gone = ns.clone();
        self.history_files.queue("Deleting…", Some(profile), Some(ns), move |s| s.empty(profile, &gone, before));
    }

    /// Deletes all Collection History from Settings, once confirmed.
    pub(super) fn clear_history(&mut self) {
        if self.history_busy(None, None) {
            return self.toast(HISTORY_ALL_IN_USE, true);
        }
        self.history_files.queue("Clearing…", None, None, |s| s.clear());
    }

    /// "Keep no history for this operation?" answered with `typed`: when it
    /// is `name` (both trimmed, as the dialog compares them), `cmd` goes as
    /// it is, and the dialog that asked waits for its answer.
    pub(super) fn send_without_history(&mut self, origin: Origin, conn: ConnId, cmd: Command, name: &str, typed: &str) {
        if typed != name.trim() {
            self.toast("The name did not match; nothing was changed", true);
            return;
        }
        let id = self.net().send(origin.clone(), Some(conn), cmd);
        // The dialog that asked waits for this answer now.
        if let Some((waiting, error)) = self.waiting_dialog(&origin)
            && waiting.is_none()
        {
            *waiting = Some(id);
            *error = None;
        }
    }
}

/// What the tree's listing says `ns` is, once its database is listed.
fn listed_kind(c: &OpenConn, ns: &Namespace) -> Option<CollKind> {
    let db = c.dbs.as_ref()?.iter().find(|d| d.info.name == ns.db)?;
    db.colls.as_ref()?.iter().find(|n| n.info.name == ns.coll).map(|n| n.info.kind.clone())
}

/// Removes, off the UI thread, the Collection History entries `settings` no
/// longer keep: once at launch. Purging goes by the entries' file names and
/// sizes (and the first bytes of small ones, to find a crash's leftovers)
/// and never decrypts, so it runs without the key (and without the vault);
/// the store's key is a placeholder never used. The thread
/// gives the bytes freed, or why it failed, and calls `done` (a repaint,
/// so the outcome is taken in).
pub(super) fn purge_history_at_start(root: PathBuf, settings: &HistorySettings, done: Waker) -> Option<std::thread::JoinHandle<rongo_core::CoreResult<u64>>> {
    let (keep_days, max_bytes) = (settings.keep_days, settings.max_bytes());
    std::thread::Builder::new()
        .name("history-purge".into())
        .spawn(move || {
            let purged = Store::new(root, HistoryKey::new([0; 32])).purge(keep_days, max_bytes, &[]);
            done();
            purged
        })
        .ok()
}

/// The delete-profile question's offer, before the profile's history is measured.
pub(super) const DELETE_ITS_HISTORY: &str = "Also delete its Collection History";

/// A recorded write as it would be sent without history, and the name to
/// type to send it so.
pub(super) struct Unrecorded {
    pub(super) cmd: Command,
    pub(super) name: String,
}

/// The title of the question asked before a write goes on without history.
pub(super) const WITHOUT_HISTORY: &str = "Keep no history for this operation?";

/// An offer to delete history (`checkbox`, labelled `prefix`), once that
/// is measured: how much, or with none, no offer.
pub(super) fn fit_offer(checkbox: &mut Option<(String, bool)>, prefix: &str, bytes: u64) {
    match bytes {
        0 => *checkbox = None,
        bytes => {
            if let Some((label, _)) = checkbox {
                *label = format!("{prefix} ({})", mb(bytes));
            }
        }
    }
}
