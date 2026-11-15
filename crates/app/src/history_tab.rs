//! The Collection History tab: what rongo saved of one collection before it
//! deleted or changed documents, newest first; the selected entry's saved
//! documents (in the Table view, or for an update beside the documents as
//! they are now); and Restore, which checks the collection as it is now and
//! then always asks (naming where it writes, and what to do with conflicts)
//! before it writes anything.
//!
//! Entries are read straight from the encrypted files: listing and reading a
//! preview run on a thread of their own, so a frame never waits on the disk.
//! Requests to the server (conflict check, restore, the current documents)
//! go through the app, which routes their answers back here.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use egui::{Align2, Color32, Context, Id, Key, Modal, Modifiers, RichText, Sense, Ui, pos2, vec2};
use egui_phosphor::regular as icon;
use rongo_core::bson::raw::cstr;
use rongo_core::bson::{Bson, DateTime, Document, RawDocumentBuf};
use rongo_core::format::{self, DisplayTz, Style};
use rongo_core::history::{EntryHeader, EntryId, Kind, Store};
use rongo_core::{ConnId, Namespace, OnConflict, RequestId};
use uuid::Uuid;

use crate::job::Job;
use crate::net::Net;
use crate::result_view::{LOCKED, ResultView, Source, ViewAction, thousands};
use crate::shortcut;
use crate::sidebar::{self, OpenConn};
use crate::theme;

/// Saved documents a preview reads, at most.
pub const PREVIEW: usize = 1000;

/// A preview stops reading once its documents take this much (keeping at
/// least one): a thousand large documents would not fit in memory.
const PREVIEW_BYTES: usize = 64 << 20;

/// A field's text in "Show changes" is cut after this many characters.
const VALUE_MAX: usize = 300;

/// Why Remove from History and Empty History wait.
pub const WRITING: &str = "Wait for the delete or update on this collection to finish: its entry is still being written";

/// Why Restore is off when the tab's connection isn't open (a history
/// opened from Settings, of a connection not open or deleted).
pub const NOT_CONNECTED: &str = "Not connected: open the connection to restore into it";

/// What the tab asks the app to do.
pub enum HistoryAction {
    /// Check the entry against the collection, then ask, then restore it (Restore or ↩).
    Restore(EntryId),
    /// Read the documents with these `_id`s as they are now ("Show changes").
    Compare(Vec<Bson>),
    /// Remove one entry, after asking.
    Remove(EntryId),
    /// Remove every entry of the collection, after asking.
    Empty,
    /// Something the preview's table asks (looking at a document).
    View(u64, ViewAction),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Checking,
    Restoring,
}

/// The conflict check or restore this tab is waiting for.
pub struct Running {
    pub req: RequestId,
    pub entry: EntryId,
    pub step: Step,
    /// Cancel was pressed: the answer is on its way.
    pub cancelling: bool,
}

/// The first saved documents of an entry, and why reading stopped early.
struct Loaded {
    docs: Vec<RawDocumentBuf>,
    error: Option<String>,
}

/// One line of "Show changes": the saved version's side and the current one's.
#[derive(Clone, Debug)]
struct DiffRow {
    /// The `_id` line a document starts with.
    head: bool,
    left: (String, Color32),
    right: (String, Color32),
}

enum Changes {
    NotAsked,
    /// The current documents were asked for (the request, once the app sent it).
    Asked(Option<RequestId>),
    Building(Job<Vec<DiffRow>>),
    Ready(Vec<DiffRow>),
    Failed(String),
}

struct Preview {
    entry: EntryId,
    kind: Kind,
    /// The first `PREVIEW` saved documents.
    saved: Arc<Vec<RawDocumentBuf>>,
    error: Option<String>,
    view: ResultView,
    changes: Changes,
}

pub struct HistoryTab {
    pub conn: ConnId,
    pub profile: Uuid,
    pub ns: Namespace,
    store: Store,
    entries: Vec<EntryHeader>,
    listed_once: bool,
    listing: Option<Job<Result<Vec<EntryHeader>, String>>>,
    needs_list: bool,
    error: Option<String>,
    selected: Option<EntryId>,
    /// Where `selected` is in `entries`, kept with it (set when either
    /// changes): the toolbar, the list and the preview ask several times a
    /// frame, and a month of history can hold thousands of entries.
    selected_ix: Option<usize>,
    loading: Option<(EntryId, Job<Loaded>)>,
    preview: Option<Preview>,
    /// For an update: each saved version beside the document as it is now.
    pub show_changes: bool,
    pub running: Option<Running>,
    /// A recorded write on this collection is running (set by the app each
    /// frame): its entry is still being written, so nothing is removed.
    pub writing: bool,
    /// ↩ belongs to the entry list: true until the preview below is
    /// clicked, and again once a row of the list is.
    list_keys: bool,
}

impl HistoryTab {
    pub fn new(conn: ConnId, profile: Uuid, ns: Namespace, store: Store) -> Self {
        Self {
            conn,
            profile,
            ns,
            store,
            entries: Vec::new(),
            listed_once: false,
            listing: None,
            needs_list: true,
            error: None,
            selected: None,
            selected_ix: None,
            loading: None,
            preview: None,
            show_changes: false,
            running: None,
            writing: false,
            list_keys: true,
        }
    }

    /// The entries, newest first (empty until listed).
    pub fn entries(&self) -> &[EntryHeader] {
        &self.entries
    }

    /// Listed, and not being listed again.
    #[cfg(test)]
    pub fn listed(&self) -> bool {
        self.listed_once && self.listing.is_none() && !self.needs_list
    }

    #[cfg(test)]
    pub fn selected(&self) -> Option<&EntryId> {
        self.selected.as_ref()
    }

    fn selected_entry(&self) -> Option<&EntryHeader> {
        self.entries.get(self.selected_ix?)
    }

    /// Finds `selected` in `entries` again, after either changed.
    fn locate_selected(&mut self) {
        self.selected_ix = self.selected.as_ref().and_then(|id| self.entries.iter().position(|e| &e.id == id));
    }

    /// How many saved documents the selected entry's preview holds, once read.
    #[cfg(test)]
    pub fn previewed(&self) -> Option<usize> {
        self.preview.as_ref().filter(|p| Some(&p.entry) == self.selected.as_ref()).map(|p| p.saved.len())
    }

    /// "Show changes" has its rows.
    #[cfg(test)]
    pub fn changes_ready(&self) -> bool {
        self.preview.as_ref().is_some_and(|p| matches!(p.changes, Changes::Ready(_)))
    }

    pub fn views_mut(&mut self) -> Vec<&mut ResultView> {
        self.preview.as_mut().map(|p| &mut p.view).into_iter().collect()
    }

    /// List the entries again (after a restore or a removal). What "Show
    /// changes" compared with may have changed too.
    pub fn reload(&mut self) {
        self.needs_list = true;
        if let Some(p) = &mut self.preview {
            p.changes = Changes::NotAsked;
        }
    }

    /// The collection was renamed and its history moved along: the tab
    /// lists it under the new name, from scratch (the entries' ids name
    /// the old collection).
    pub fn renamed(&mut self, ns: Namespace) {
        self.ns = ns;
        self.entries.clear();
        self.listed_once = false;
        self.listing = None;
        self.needs_list = true;
        self.selected = None;
        self.selected_ix = None;
        self.loading = None;
        self.preview = None;
    }

    /// Why Restore is off for the selected entry, if it is.
    pub fn restore_blocked(&self, conn: Option<&OpenConn>) -> Option<&'static str> {
        self.blocked(conn, self.selected_entry())
    }

    fn blocked(&self, conn: Option<&OpenConn>, entry: Option<&EntryHeader>) -> Option<&'static str> {
        let Some(c) = conn.filter(|c| c.info().is_some()) else { return Some(NOT_CONNECTED) };
        if c.profile.read_only {
            return Some(LOCKED);
        }
        if self.running.is_some() {
            return Some("A restore is running");
        }
        match entry {
            None => Some("Select an entry first"),
            Some(e) => entry_blocked(e),
        }
    }

    /// Why Remove from History and Empty History are off, if they are.
    pub fn removal_blocked(&self) -> Option<&'static str> {
        if self.running.is_some() {
            Some("Wait for the restore to finish")
        } else if self.writing {
            Some(WRITING)
        } else {
            None
        }
    }

    /// What an entry is, for questions: "the delete of 2 documents saved 2026-…".
    pub fn describe(&self, id: &EntryId, tz: DisplayTz) -> String {
        match self.entries.iter().find(|e| &e.id == id) {
            Some(e) => {
                let what = match e.kind {
                    Kind::Delete => "delete",
                    Kind::Update => "update",
                };
                format!("the {what} of {} document{} saved {}", thousands(e.count), plural(e.count), when(e.at, tz))
            }
            None => "this entry".into(),
        }
    }

    /// The app sent the request for the current documents.
    pub fn comparing(&mut self, req: RequestId) {
        if let Some(p) = &mut self.preview
            && matches!(p.changes, Changes::Asked(None))
        {
            p.changes = Changes::Asked(Some(req));
        }
    }

    /// The current documents arrived: compare them on a thread.
    pub fn on_current(&mut self, ctx: &Context, req: RequestId, docs: Vec<RawDocumentBuf>) {
        if let Some(p) = &mut self.preview
            && matches!(p.changes, Changes::Asked(Some(r)) if r == req)
        {
            let saved = p.saved.clone();
            p.changes = Changes::Building(Job::start(ctx, "history-read", move |stop| diff_rows(&saved, &docs, stop)));
        }
    }

    /// A request of this tab failed: the comparison, if it was that one.
    pub fn on_compare_failed(&mut self, req: RequestId, e: String) {
        if let Some(p) = &mut self.preview
            && matches!(p.changes, Changes::Asked(Some(r)) if r == req)
        {
            p.changes = Changes::Failed(e);
        }
    }

    fn select(&mut self, ctx: &Context, id: EntryId) {
        if self.selected.as_ref() == Some(&id) {
            return;
        }
        self.selected = Some(id.clone());
        self.locate_selected();
        self.preview = None;
        let store = self.store.clone();
        let read = id.clone();
        self.loading = Some((id, Job::start(ctx, "history-read", move |stop| read_preview(&store, &read, stop))));
    }

    /// Starts listing when asked, and takes in what the threads finished.
    fn poll(&mut self, ctx: &Context, net: &mut Net) {
        if self.needs_list && self.listing.is_none() {
            self.needs_list = false;
            let (store, profile, ns) = (self.store.clone(), self.profile, self.ns.clone());
            self.listing = Some(Job::start(ctx, "history-read", move |_| store.list(profile, &ns).map_err(|e| e.to_string())));
        }
        if let Some(listed) = self.listing.as_ref().and_then(Job::poll) {
            self.listing = None;
            self.listed_once = true;
            match listed.and_then(|r| r) {
                Ok(entries) => {
                    self.entries = entries;
                    self.error = None;
                }
                Err(e) => self.error = Some(format!("Could not list the history: {e}")),
            }
            self.locate_selected();
            // The selection stays while its entry is listed; else the newest.
            if self.selected_entry().is_none() {
                self.selected = None;
                self.preview = None;
                self.loading = None;
                if let Some(first) = self.entries.first().map(|e| e.id.clone()) {
                    self.select(ctx, first);
                }
            }
        }
        if let Some(read) = self.loading.as_ref().and_then(|(_, job)| job.poll()) {
            let (id, _) = self.loading.take().expect("polled");
            let loaded = read.unwrap_or_else(|e| Loaded { docs: Vec::new(), error: Some(e) });
            if let Some(entry) = self.entries.iter().find(|e| e.id == id) {
                let saved = Arc::new(loaded.docs);
                let truncated = (saved.len() as u64) < entry.count;
                let title = format!("{} history", self.ns.coll);
                let view = ResultView::new(net.view_id(), title, Source::Raw { docs: saved.clone(), truncated });
                self.preview = Some(Preview { entry: id, kind: entry.kind, saved, error: loaded.error, view, changes: Changes::NotAsked });
            }
        }
        if let Some(p) = &mut self.preview
            && let Changes::Building(job) = &p.changes
            && let Some(built) = job.poll()
        {
            p.changes = match built {
                Ok(rows) => Changes::Ready(rows),
                Err(e) => Changes::Failed(e),
            };
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, net: &mut Net, conn: Option<&OpenConn>, tz: DisplayTz) -> Vec<HistoryAction> {
        let mut actions = Vec::new();
        self.poll(ui.ctx(), net);
        sidebar::breadcrumb(ui, conn, &self.ns.db);
        ui.label(RichText::new(format!("{}  {}  {}  Collection History", self.ns.coll, icon::CARET_RIGHT, icon::CLOCK_COUNTER_CLOCKWISE)).strong());
        ui.add_space(4.0);
        self.toolbar(ui, net, conn, &mut actions);
        ui.add_space(4.0);
        if let Some(e) = &self.error {
            ui.colored_label(theme::ERROR, e);
        }
        self.list(ui, conn, tz, &mut actions);
        ui.separator();
        self.preview_ui(ui, net, conn, &mut actions);
        // ↩ (no modifier: `consume_key` would take ⇧↩ too) starts restoring
        // the selected entry, which asks before writing. Not after a click
        // in the preview: there ↩ is the table's, and nothing else has the
        // keyboard either.
        if self.restore_blocked(conn).is_none()
            && self.list_keys
            && net.keys_enabled
            && !ui.ctx().egui_wants_keyboard_input()
            && ui.input(|i| i.modifiers.is_none())
            && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter))
            && let Some(id) = &self.selected
        {
            actions.push(HistoryAction::Restore(id.clone()));
        }
        actions
    }

    /// Refresh, Restore, the menu, and the running check or restore with its
    /// progress and Cancel. Wraps on a narrow tab rather than widening it.
    fn toolbar(&mut self, ui: &mut Ui, net: &mut Net, conn: Option<&OpenConn>, actions: &mut Vec<HistoryAction>) {
        let blocked = self.restore_blocked(conn);
        ui.horizontal_wrapped(|ui| {
            if ui.add_enabled(self.listing.is_none(), egui::Button::new(icon::ARROWS_CLOCKWISE)).on_hover_text("Refresh").clicked() {
                self.reload();
            }
            ui.separator();
            let restore = ui.add_enabled(blocked.is_none(), egui::Button::new(format!("{}  Restore", icon::ARROW_U_UP_LEFT)));
            let restore = match blocked {
                Some(why) => restore.on_disabled_hover_text(why),
                None => restore.on_hover_text(shortcut::label("Put the selected entry's documents back; asks before writing (↩)")),
            };
            if restore.clicked()
                && let Some(id) = &self.selected
            {
                actions.push(HistoryAction::Restore(id.clone()));
            }
            let removal = self.removal_blocked();
            let why = |r: egui::Response| match removal {
                Some(why) => r.on_disabled_hover_text(why),
                None => r,
            };
            ui.menu_button(icon::DOTS_THREE, |ui| {
                ui.set_min_width(260.0);
                let remove = egui::Button::new(format!("{}  Remove from History…", icon::TRASH));
                if why(ui.add_enabled(removal.is_none() && self.selected.is_some(), remove)).clicked()
                    && let Some(id) = &self.selected
                {
                    actions.push(HistoryAction::Remove(id.clone()));
                    ui.close();
                }
                let empty = egui::Button::new(RichText::new(format!("{}  Empty History for This Collection…", icon::BROOM)).color(theme::ERROR));
                if why(ui.add_enabled(removal.is_none() && !self.entries.is_empty(), empty)).clicked() {
                    actions.push(HistoryAction::Empty);
                    ui.close();
                }
            })
            .response
            .on_hover_text("More");
            if let Some(run) = &mut self.running {
                ui.separator();
                theme::busy_bar(ui, 80.0);
                let what = match run.step {
                    Step::Checking => "Checking for conflicts",
                    Step::Restoring => "Restoring",
                };
                let text = match net.pending.get(&run.req).and_then(|s| s.progress.as_ref()) {
                    Some((done, Some(total), _)) => format!("{what}… {} / {}", thousands(*done), thousands(*total)),
                    Some((done, None, _)) => format!("{what}… {}", thousands(*done)),
                    None => format!("{what}…"),
                };
                ui.label(text);
                if ui.add_enabled(!run.cancelling, egui::Button::new("Cancel")).on_hover_text("Stop between batches; what was restored stays").clicked() {
                    net.send(crate::net::Origin::None, None, rongo_core::Command::Cancel(run.req));
                    run.cancelling = true;
                }
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
            }
        });
    }

    /// The entries, newest first: when, what, how many, size and status.
    fn list(&mut self, ui: &mut Ui, conn: Option<&OpenConn>, tz: DisplayTz, actions: &mut Vec<HistoryAction>) {
        let width = ui.available_width();
        let row_h = 24.0;
        let cols = [0.30, 0.16, 0.14, 0.14, 0.26].map(|f| f * width);
        let (rect, _) = ui.allocate_exact_size(vec2(width, row_h), Sense::hover());
        ui.painter().rect_filled(rect, 0.0, theme::RAISED);
        let mut x = rect.left();
        for (title, w) in ["Saved", "Operation", "Documents", "Size", "Status"].iter().zip(cols) {
            let clip = egui::Rect::from_x_y_ranges(x..=x + w - 4.0, rect.y_range());
            ui.painter_at(clip).text(pos2(x + 6.0, rect.center().y), Align2::LEFT_CENTER, *title, egui::FontId::proportional(13.0), theme::TEXT);
            x += w;
        }
        if !self.listed_once {
            ui.add_space(6.0);
            ui.spinner();
            return;
        }
        if self.entries.is_empty() {
            ui.add_space(6.0);
            ui.weak("Nothing is saved for this collection. rongo keeps documents here before it deletes or changes them.");
            return;
        }
        let mut clicked = None;
        let height = self.entries.len().min(6) as f32 * (row_h + ui.spacing().item_spacing.y);
        // Only the rows in view are laid out: a month of history can hold thousands.
        let scroll = egui::ScrollArea::vertical().id_salt(("history-list", &self.ns.db, &self.ns.coll)).max_height(height).auto_shrink([false, true]);
        scroll.show_rows(ui, row_h, self.entries.len(), |ui, range| {
            for (i, e) in self.entries.iter().enumerate().skip(range.start).take(range.len()) {
                // Rows aren't focusable: egui keeps every focusable id.
                let (rect, resp) = ui.allocate_exact_size(vec2(width, row_h), Sense::CLICK);
                let fill = if self.selected.as_ref() == Some(&e.id) {
                    theme::SELECTION
                } else if i % 2 == 1 {
                    Color32::from_white_alpha(6)
                } else {
                    Color32::TRANSPARENT
                };
                ui.painter().rect_filled(rect, 0.0, fill);
                let kind = match e.kind {
                    Kind::Delete => format!("{}  Delete", icon::TRASH),
                    Kind::Update => format!("{}  Update", icon::PENCIL_SIMPLE),
                };
                let cells = [
                    (when(e.at, tz), theme::TEXT),
                    (kind, theme::TEXT),
                    (thousands(e.count), theme::TEXT),
                    (format::human_bytes(e.bytes), theme::MUTED),
                    status(e),
                ];
                let mut x = rect.left();
                for ((text, colour), w) in cells.iter().zip(cols) {
                    let clip = egui::Rect::from_x_y_ranges(x..=x + w - 4.0, rect.y_range());
                    ui.painter_at(clip).text(pos2(x + 6.0, rect.center().y), Align2::LEFT_CENTER, text, egui::FontId::proportional(13.0), *colour);
                    x += w;
                }
                let resp = resp.on_hover_ui(|ui| {
                    ui.label(row_hint(e));
                });
                if resp.clicked() || resp.secondary_clicked() {
                    clicked = Some(e.id.clone());
                }
                let removal = self.removal_blocked();
                let blocked = self.blocked(conn, Some(e));
                resp.context_menu(|ui| {
                    let restore = ui.add_enabled(blocked.is_none(), egui::Button::new(format!("{}  Restore", icon::ARROW_U_UP_LEFT)));
                    if blocked.map_or(restore.clone(), |why| restore.on_disabled_hover_text(why)).clicked() {
                        actions.push(HistoryAction::Restore(e.id.clone()));
                        ui.close();
                    }
                    let remove = ui.add_enabled(removal.is_none(), egui::Button::new(format!("{}  Remove from History…", icon::TRASH)));
                    if removal.map_or(remove.clone(), |why| remove.on_disabled_hover_text(why)).clicked() {
                        actions.push(HistoryAction::Remove(e.id.clone()));
                        ui.close();
                    }
                });
            }
        });
        if let Some(id) = clicked {
            self.list_keys = true;
            self.select(ui.ctx(), id);
        }
    }

    /// The selected entry's saved documents: the Table view, or for an
    /// update with "Show changes" on, each beside the document as it is now.
    fn preview_ui(&mut self, ui: &mut Ui, net: &mut Net, conn: Option<&OpenConn>, actions: &mut Vec<HistoryAction>) {
        let area = ui.available_rect_before_wrap();
        if ui.input(|i| i.pointer.any_pressed() && i.pointer.interact_pos().is_some_and(|p| area.contains(p))) {
            self.list_keys = false;
        }
        // The entry by its fields, not cloned: the preview is changed below.
        let Some(entry) = self.selected_ix.and_then(|i| self.entries.get(i)) else {
            if !self.entries.is_empty() {
                ui.weak("Select an entry to see the documents it saved.");
            }
            return;
        };
        let Some(p) = self.preview.as_mut().filter(|p| p.entry == entry.id) else {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak("Reading the entry…");
            });
            return;
        };
        let show_changes = &mut self.show_changes;
        ui.horizontal_wrapped(|ui| {
            let what = match p.kind {
                Kind::Delete => "Deleted",
                Kind::Update => "Changed",
            };
            ui.label(RichText::new(format!("{what} {} document{}", thousands(entry.count), plural(entry.count))).strong());
            if (p.saved.len() as u64) < entry.count {
                ui.weak(format!("· showing the first {}", thousands(p.saved.len() as u64)));
            }
            if p.kind == Kind::Update {
                ui.separator();
                ui.toggle_value(show_changes, format!("{}  Show changes", icon::GIT_DIFF))
                    .on_hover_text("Each saved version beside the document as it is now, the changed fields in colour");
            }
        });
        if let Some(e) = &p.error {
            ui.colored_label(theme::WARN, format!("{}  Not every document could be read: {e}", icon::WARNING));
        }
        ui.add_space(2.0);
        if !(*show_changes && p.kind == Kind::Update) {
            actions.extend(p.view.ui(ui, net).into_iter().map(|(v, a)| HistoryAction::View(v, a)));
            return;
        }
        if matches!(p.changes, Changes::NotAsked) {
            if conn.is_some_and(|c| c.info().is_some()) {
                let ids: Vec<Bson> = p.saved.iter().filter_map(|d| d.get("_id").ok().flatten().and_then(|v| Bson::try_from(v).ok())).collect();
                if ids.is_empty() {
                    p.changes = Changes::Ready(Vec::new());
                } else {
                    actions.push(HistoryAction::Compare(ids));
                    p.changes = Changes::Asked(None);
                }
            } else {
                ui.weak("Connect to compare the saved versions with the documents as they are now.");
                return;
            }
        }
        match &p.changes {
            Changes::NotAsked | Changes::Asked(_) | Changes::Building(_) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.weak("Reading the documents as they are now…");
                });
            }
            Changes::Failed(e) => {
                ui.colored_label(theme::ERROR, format!("Could not read the documents as they are now: {e}"));
            }
            Changes::Ready(rows) => changes_ui(ui, rows, &p.entry.file),
        }
    }
}

fn plural(n: u64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// When an entry was saved, in the Settings time zone: `2026-11-13 18:59:00 +07:00`.
pub fn when(at: DateTime, tz: DisplayTz) -> String {
    let s = format::iso_date_in(at, tz);
    match (s.get(..10), s.get(11..19), s.get(23..)) {
        (Some(day), Some(time), Some(zone)) if s.as_bytes().get(10) == Some(&b'T') => {
            format!("{day} {time} {}", if zone == "Z" { "UTC" } else { zone })
        }
        _ => s,
    }
}

/// Why an entry can't be restored, if it can't.
fn entry_blocked(e: &EntryHeader) -> Option<&'static str> {
    if e.unreadable.is_some() {
        Some("This entry could not be read just now; Refresh lists it again")
    } else if e.count == 0 {
        Some("This entry holds no documents that can be read")
    } else {
        None
    }
}

/// An entry's status cell: restored, and whether it is complete or was
/// cancelled; or that its file could not be read just now.
fn status(e: &EntryHeader) -> (String, Color32) {
    if e.unreadable.is_some() {
        return ("Could not be read".into(), theme::WARN);
    }
    match (e.complete, e.cancelled, e.restored) {
        (true, false, true) => ("Restored".into(), theme::OK),
        (true, false, false) => (String::new(), theme::TEXT),
        (true, true, true) => ("Cancelled, restored".into(), theme::OK),
        (true, true, false) => ("Cancelled".into(), theme::MUTED),
        (false, _, true) => ("Incomplete, restored".into(), theme::WARN),
        (false, _, false) => ("Incomplete".into(), theme::WARN),
    }
}

fn row_hint(e: &EntryHeader) -> String {
    if let Some(why) = &e.unreadable {
        return format!("This entry's file could not be read just now ({why}); Refresh lists it again");
    }
    if e.conn_name.is_empty() && e.count == 0 {
        return "This entry can't be read: it was saved under a master password that was reset since, or the file is damaged".into();
    }
    let mut s = format!("Saved from {}", e.conn_name);
    let (n, docs) = (thousands(e.count), plural(e.count));
    if !e.complete {
        s.push_str(&format!("\nIncomplete: the operation failed part way, or the file is damaged. It holds {n} document{docs}."));
    } else if e.cancelled {
        s.push_str(&format!("\nCancelled: the operation stopped between batches. It holds the {n} document{docs} saved before that."));
    }
    s
}

/// The first `PREVIEW` saved documents of an entry (fewer when they are
/// large); nothing more once `stop` is raised.
fn read_preview(store: &Store, id: &EntryId, stop: &AtomicBool) -> Loaded {
    let mut docs = Vec::new();
    let error = match store.read(id) {
        Ok(reader) if reader.kind().is_none() => Some("the entry can't be read (it was saved under a master password that was reset since, or the file is damaged)".into()),
        Ok(reader) => {
            let (mut error, mut bytes) = (None, 0);
            for saved in reader.take(PREVIEW) {
                if bytes >= PREVIEW_BYTES || stop.load(Ordering::Relaxed) {
                    break;
                }
                match saved {
                    Ok(s) => {
                        bytes += s.before.as_bytes().len();
                        docs.push(s.before);
                    }
                    Err(e) => {
                        error = Some(e.to_string());
                        break;
                    }
                }
            }
            error
        }
        Err(e) => Some(e.to_string()),
    };
    Loaded { docs, error }
}

/// A key for a document's `_id`, by type and bytes.
fn id_key(doc: &RawDocumentBuf) -> Option<Vec<u8>> {
    let id = doc.get("_id").ok().flatten()?;
    let mut d = RawDocumentBuf::new();
    d.append(cstr!("v"), id);
    Some(d.into_bytes())
}

/// `field: value` on one line, as display text (never evaluated), cut short.
fn field_text(k: &str, v: &Bson) -> String {
    let s = format!("{k}: {}", format::to_shell(v, Style::Display));
    if s.chars().count() > VALUE_MAX { s.chars().take(VALUE_MAX).chain(['…']).collect() } else { s }
}

/// Each saved version beside the document with its `_id` now, field by
/// field: an unchanged field muted, a changed one's old value in the removed
/// colour and its new one in the added colour, a removed field in the
/// removed colour, an added one in the added colour.
fn diff_rows(saved: &[RawDocumentBuf], current: &[RawDocumentBuf], stop: &AtomicBool) -> Vec<DiffRow> {
    let now: HashMap<Vec<u8>, &RawDocumentBuf> = current.iter().filter_map(|d| Some((id_key(d)?, d))).collect();
    let none = || (String::new(), theme::TEXT);
    let mut rows = Vec::new();
    for raw in saved {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let Ok(before) = Document::try_from(raw.as_ref()) else { continue };
        let after = id_key(raw).and_then(|k| now.get(&k)).and_then(|d| Document::try_from(d.as_ref()).ok());
        let id = before.get("_id").map_or_else(|| "(no _id)".to_string(), |v| field_text("_id", v));
        let right = match &after {
            Some(_) => (id.clone(), theme::TEXT),
            None => ("no longer exists".to_string(), theme::WARN),
        };
        rows.push(DiffRow { head: true, left: (id, theme::TEXT), right });
        for (k, v) in before.iter().filter(|(k, _)| k.as_str() != "_id") {
            let text = field_text(k, v);
            rows.push(match after.as_ref().map(|a| a.get(k)) {
                // Gone: it comes back as saved.
                None => DiffRow { head: false, left: (text, theme::TEXT), right: none() },
                Some(Some(now)) if now == v => DiffRow { head: false, right: (text.clone(), theme::MUTED), left: (text, theme::MUTED) },
                Some(Some(now)) => DiffRow { head: false, left: (text, theme::ERROR), right: (field_text(k, now), theme::OK) },
                Some(None) => DiffRow { head: false, left: (text, theme::ERROR), right: none() },
            });
        }
        if let Some(a) = &after {
            for (k, v) in a.iter().filter(|(k, _)| k.as_str() != "_id" && !before.contains_key(k.as_str())) {
                rows.push(DiffRow { head: false, left: none(), right: (field_text(k, v), theme::OK) });
            }
        }
    }
    rows
}

/// "Show changes": two columns, only the rows in view laid out.
fn changes_ui(ui: &mut Ui, rows: &[DiffRow], salt: &str) {
    let width = ui.available_width();
    let row_h = 20.0;
    let gap = 8.0;
    let half = (width - gap) / 2.0;
    let font = theme::mono(12.5);
    let (rect, _) = ui.allocate_exact_size(vec2(width, 22.0), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, theme::RAISED);
    for (i, title) in ["Saved version", "Now"].into_iter().enumerate() {
        let x = rect.left() + i as f32 * (half + gap);
        ui.painter().text(pos2(x + 6.0, rect.center().y), Align2::LEFT_CENTER, title, egui::FontId::proportional(13.0), theme::TEXT);
    }
    if rows.is_empty() {
        ui.weak("No documents");
        return;
    }
    egui::ScrollArea::vertical().id_salt(("history-changes", salt)).auto_shrink([false, false]).show_rows(ui, row_h, rows.len(), |ui, range| {
        for r in &rows[range] {
            let (rect, _) = ui.allocate_exact_size(vec2(width, row_h), Sense::hover());
            if r.head {
                ui.painter().rect_filled(rect, 0.0, Color32::from_white_alpha(10));
            }
            for (i, (text, colour)) in [&r.left, &r.right].into_iter().enumerate() {
                let x = rect.left() + i as f32 * (half + gap);
                let clip = egui::Rect::from_x_y_ranges(x..=x + half - 4.0, rect.y_range());
                ui.painter_at(clip).text(pos2(x + 6.0, rect.center().y), Align2::LEFT_CENTER, text, font.clone(), *colour);
            }
        }
    });
}

/// The question every restore asks before it writes: where to (the
/// connection, by name and colour, and the collection), what it will do,
/// and, when some saved documents conflict, what to do with those.
pub struct RestoreAsk {
    pub tab: u64,
    pub entry: EntryId,
    pub kind: Kind,
    /// The connection the restore writes to.
    pub conn_name: String,
    pub color: Option<Color32>,
    /// The connection's name when the entry was saved, when that was another.
    pub saved_from: Option<String>,
    /// When the entry was saved, in the Settings time zone.
    pub when: String,
    pub total: u64,
    pub conflicts: u64,
    /// The first conflicting `_id`s as display text: shown, never evaluated.
    pub sample: Vec<String>,
    /// What to do with the conflicts; Skip unless another is chosen.
    pub on_conflict: OnConflict,
}

impl RestoreAsk {
    pub fn new(tab: u64, entry: &EntryHeader, tz: DisplayTz, conn: Option<&OpenConn>, total: u64, conflicts: u64, sample: &[Bson]) -> Self {
        let conn_name = conn.map_or_else(|| "(closed)".to_string(), |c| c.profile.name.clone());
        Self {
            tab,
            entry: entry.id.clone(),
            kind: entry.kind,
            saved_from: (!entry.conn_name.is_empty() && entry.conn_name != conn_name).then(|| entry.conn_name.clone()),
            conn_name,
            color: conn.and_then(|c| theme::conn_color(c.profile.color.as_deref())),
            when: when(entry.at, tz),
            total,
            conflicts,
            sample: sample.iter().map(|id| format::to_shell(id, Style::Display)).collect(),
            on_conflict: OnConflict::Skip,
        }
    }

    /// How many documents the restore puts back as chosen: skipped conflicts don't count.
    pub fn count(&self) -> u64 {
        match self.on_conflict {
            OnConflict::Skip => self.total.saturating_sub(self.conflicts),
            OnConflict::Overwrite | OnConflict::InsertNew => self.total,
        }
    }

    /// The answer once given: true to restore as chosen, false for Cancel
    /// (or Esc). ↩ answers nothing: a restore writes, so it takes a click.
    pub fn show(&mut self, ctx: &Context) -> Option<bool> {
        let mut answer = None;
        let ns = self.entry.ns.to_string();
        let modal = Modal::new(Id::new("restore-dialog")).show(ctx, |ui| {
            ui.set_width(480.0);
            ui.heading(format!("{}  Restore from Collection History", icon::ARROW_U_UP_LEFT));
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("into").color(theme::MUTED));
                ui.label(RichText::new(&self.conn_name).strong().color(self.color.unwrap_or(theme::TEXT)));
                ui.label(RichText::new(format!("· {ns}")).strong());
            });
            let what = match self.kind {
                Kind::Delete => "the delete",
                Kind::Update => "the update",
            };
            ui.label(RichText::new(format!("From {what} of {} document{} saved {}", thousands(self.total), plural(self.total), self.when)).color(theme::MUTED));
            if let Some(name) = &self.saved_from {
                ui.label(RichText::new(format!("Saved from {name}")).color(theme::MUTED));
            }
            ui.separator();
            let n = self.count();
            ui.label(match self.kind {
                Kind::Delete => format!("Insert {} deleted document{} back into {ns}.", thousands(n), plural(n)),
                Kind::Update => format!("Replace {} document{} in {ns} with their saved versions.", thousands(n), plural(n)),
            });
            match self.kind {
                Kind::Delete => ui.weak("Inserted documents are not kept in Collection History."),
                Kind::Update => ui.weak("What it replaces is kept in Collection History first, so this can be undone."),
            };
            if self.conflicts > 0 {
                ui.add_space(6.0);
                let what = match self.kind {
                    Kind::Delete => "have an _id that is in the collection again",
                    Kind::Update => "no longer exist, or were changed again after the operation",
                };
                ui.label(RichText::new(format!("{} of {} saved documents {what}.", thousands(self.conflicts), thousands(self.total))).color(theme::WARN));
                let shown = self.sample.len() as u64;
                ui.weak(if shown < self.conflicts { format!("The first {shown} of their _ids:") } else { "Their _ids:".to_string() });
                egui::ScrollArea::vertical().id_salt("restore-conflict-ids").max_height(110.0).show(ui, |ui| {
                    for id in &self.sample {
                        theme::clipped_text(ui, id, theme::TEXT, 1, 460.0);
                    }
                });
                ui.add_space(4.0);
                ui.label("What should happen to them?");
                ui.radio_value(&mut self.on_conflict, OnConflict::Skip, "Skip: leave them as they are");
                ui.radio_value(&mut self.on_conflict, OnConflict::Overwrite, "Overwrite: put the saved versions in their place");
                if self.kind == Kind::Delete {
                    ui.radio_value(&mut self.on_conflict, OnConflict::InsertNew, "Insert as New: insert them with new _ids");
                }
            }
            ui.add_space(8.0);
            let n = self.count();
            // Replacing documents is the one that destroys something.
            let danger = self.kind == Kind::Update || self.on_conflict == OnConflict::Overwrite;
            theme::dialog_buttons(ui, |ui| {
                let go = ui.add_enabled(n > 0, theme::primary_button(format!("Restore {}", thousands(n)), danger));
                if go.on_disabled_hover_text("Every saved document conflicts: choose what to do with them").clicked() {
                    answer = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    answer = Some(false);
                }
            });
        });
        if modal.should_close() && answer.is_none() {
            answer = Some(false);
        }
        answer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rongo_core::bson::doc;

    fn raw(d: Document) -> RawDocumentBuf {
        RawDocumentBuf::try_from(&d).unwrap()
    }

    /// Times read in the zone given, with the zone named.
    #[test]
    fn times_in_the_settings_zone() {
        let at = DateTime::from_millis(1_763_060_340_123);
        assert_eq!(when(at, DisplayTz::Utc), "2025-11-13 18:59:00 UTC");
        assert_eq!(when(at, DisplayTz::Offset(7 * 60)), "2025-11-14 01:59:00 +07:00");
    }

    /// A cancelled entry is listed as Cancelled, not as complete with no
    /// status; Incomplete no longer blames a cancel (a cancel finishes the
    /// entry).
    #[test]
    fn cancelled_and_incomplete_entries_say_which() {
        let h = |complete, cancelled, restored| EntryHeader {
            id: EntryId { profile: Uuid::nil(), ns: Namespace::new("d", "c"), file: "x".into() },
            kind: Kind::Delete,
            at: DateTime::from_millis(0),
            conn_name: "Local".into(),
            count: 3,
            complete,
            cancelled,
            restored,
            bytes: 1,
            unreadable: None,
        };
        assert_eq!(status(&h(true, false, false)).0, "");
        assert_eq!(status(&h(true, true, false)).0, "Cancelled");
        assert_eq!(status(&h(true, true, true)).0, "Cancelled, restored");
        assert_eq!(status(&h(false, false, false)).0, "Incomplete");
        let hint = row_hint(&h(true, true, false));
        assert!(hint.contains("\nCancelled: ") && hint.contains("3 documents"), "{hint}");
        let hint = row_hint(&h(false, false, false));
        assert!(hint.contains("\nIncomplete: ") && !hint.to_lowercase().contains("cancel"), "{hint}");
        assert!(!row_hint(&h(true, false, false)).contains('\n'), "a finished entry needs no note");
    }

    /// An entry whose file could not be read just now says so, apart from a
    /// damaged or incomplete one, and can't be restored until it reads.
    #[test]
    fn an_entry_that_could_not_be_read_says_so() {
        let unreadable = EntryHeader {
            id: EntryId { profile: Uuid::nil(), ns: Namespace::new("d", "c"), file: "x".into() },
            kind: Kind::Delete,
            at: DateTime::from_millis(0),
            conn_name: String::new(),
            count: 0,
            complete: false,
            cancelled: false,
            restored: false,
            bytes: 1,
            unreadable: Some("Permission denied (os error 13)".into()),
        };
        assert_eq!(status(&unreadable), ("Could not be read".to_string(), theme::WARN));
        let hint = row_hint(&unreadable);
        assert!(hint.contains("could not be read just now") && hint.contains("Permission denied") && hint.contains("Refresh"), "{hint}");
        assert!(!hint.contains("damaged") && !hint.contains("master password"), "{hint}");
        assert_eq!(entry_blocked(&unreadable), Some("This entry could not be read just now; Refresh lists it again"));
        // Damaged (or another key's): as before.
        let damaged = EntryHeader { unreadable: None, ..unreadable.clone() };
        assert_eq!(status(&damaged).0, "Incomplete");
        assert!(row_hint(&damaged).contains("damaged"));
        assert_eq!(entry_blocked(&damaged), Some("This entry holds no documents that can be read"));
        // Restored earlier, unreadable now: still says it could not be read.
        assert_eq!(status(&EntryHeader { restored: true, ..unreadable }).0, "Could not be read");
    }

    /// A gone document says so and comes back as saved; a long value is cut.
    #[test]
    fn changes_of_a_gone_document_and_long_values() {
        let long = "x".repeat(1000);
        let rows = diff_rows(&[raw(doc! { "_id": 1, "s": &long }), raw(doc! { "_id": 2, "a": 1 })], &[raw(doc! { "_id": 2, "a": 1 })], &AtomicBool::new(false));
        assert_eq!(rows[0].right, ("no longer exists".to_string(), theme::WARN));
        assert_eq!(rows[1].left.1, theme::TEXT);
        assert_eq!(rows[1].left.0.chars().count(), VALUE_MAX + 1);
        assert!(rows[1].left.0.ends_with('…'));
        assert_eq!((rows[3].left.1, rows[3].right.1), (theme::MUTED, theme::MUTED), "unchanged");
    }

    /// The selected entry stays selected when the list comes again with a
    /// newer entry above it (it is found by its place, kept with the
    /// selection), and the newest is selected when it is gone.
    #[test]
    fn the_selection_follows_its_entry_through_a_reload() {
        let root = std::env::temp_dir().join(format!("rongo-test-{}", Uuid::new_v4())).join("history");
        let store = Store::new(root.clone(), rongo_core::history::HistoryKey::new([7; 32]));
        let (profile, ns) = (Uuid::from_u128(1), Namespace::new("d", "c"));
        let save = |n: i32| {
            let mut w = store.begin(profile, "c", &ns, Kind::Delete, doc! {}).unwrap();
            w.save(&(0..n).map(|i| raw(doc! { "_id": i })).collect::<Vec<_>>()).unwrap();
            let id = w.finish(false).unwrap();
            // Entries are named by time: the next one is newer.
            std::thread::sleep(std::time::Duration::from_millis(5));
            id
        };
        let (_a, b, _c) = (save(1), save(2), save(3));
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let core = rongo_core::CoreHandle::start(rt.handle().clone(), Arc::new(|| {}));
        let (mut pending, mut next) = (std::collections::HashMap::new(), 0);
        let ctx = Context::default();
        let mut tab = HistoryTab::new(ConnId(profile), profile, ns.clone(), store.clone());
        let mut listed = |tab: &mut HistoryTab| {
            let mut net = Net { core: &core, pending: &mut pending, next_view_id: &mut next, keys_enabled: false };
            tab.poll(&ctx, &mut net);
            let end = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while !tab.listed() && std::time::Instant::now() < end {
                std::thread::sleep(std::time::Duration::from_millis(5));
                tab.poll(&ctx, &mut net);
            }
            assert!(tab.listed());
        };
        listed(&mut tab);
        assert_eq!(tab.entries().len(), 3);
        assert_eq!(tab.selected_entry().map(|e| e.count), Some(3), "the newest at first");
        tab.select(&ctx, b.clone());
        assert_eq!(tab.selected_entry().map(|e| &e.id), Some(&b));

        save(4);
        tab.reload();
        listed(&mut tab);
        assert_eq!(tab.entries().len(), 4);
        assert_eq!(tab.selected_entry().map(|e| &e.id), Some(&b), "still the same entry, now further down");
        assert_eq!(tab.entries().iter().position(|e| e.id == b), Some(2));

        store.remove(&b).unwrap();
        tab.reload();
        listed(&mut tab);
        assert_eq!(tab.selected_entry().map(|e| e.count), Some(4), "gone: the newest instead");
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    /// A preview read nobody waits for any more stops: the flag a dropped
    /// job raises ends it before the next document.
    #[test]
    fn a_replaced_preview_stops_reading() {
        let root = std::env::temp_dir().join(format!("rongo-test-{}", Uuid::new_v4())).join("history");
        let store = Store::new(root.clone(), rongo_core::history::HistoryKey::new([7; 32]));
        let ns = Namespace::new("d", "c");
        let mut w = store.begin(Uuid::from_u128(1), "c", &ns, Kind::Delete, doc! {}).unwrap();
        w.save(&(1..=5).map(|i| raw(doc! { "_id": i })).collect::<Vec<_>>()).unwrap();
        let id = w.finish(false).unwrap();
        assert_eq!(read_preview(&store, &id, &AtomicBool::new(false)).docs.len(), 5);
        assert!(read_preview(&store, &id, &AtomicBool::new(true)).docs.is_empty(), "stopped");

        let job = Job::start(&Context::default(), "history-read", |_| ());
        let stop = job.stop_flag();
        drop(job);
        assert!(stop.load(Ordering::Relaxed), "dropping a job raises its flag");
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }
}
