//! The Operations panel under the tree: every write and data move sent this
//! session (a request whose `net::Sent::log` is set), running or finished,
//! newest first, with its progress, the full text of its result and, for a
//! copy of collections, each collection's own line. Kept for the session
//! only, at most `MAX_ENTRIES`; never written to disk.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use egui::text::{LayoutJob, TextFormat};
use egui::{Align, Color32, Layout, Rect, RichText, Sense, TextStyle, Ui, UiBuilder, pos2, vec2};
use egui_phosphor::regular as icon;
use rongo_core::bson::DateTime;
use rongo_core::format::{self, DisplayTz};
use rongo_core::{CopyReport, Namespace, RequestId};

use crate::history_tab::when;
use crate::result_view::thousands;
use crate::theme;

/// The most entries kept: past it, the oldest finished one goes (running
/// ones never do).
pub const MAX_ENTRIES: usize = 200;
/// The panel's height (header and list) when first unfolded.
pub const DEFAULT_HEIGHT: f32 = 220.0;
/// The least the drag handle leaves it…
pub const MIN_HEIGHT: f32 = 80.0;
/// …and the most of the sidebar's height it takes.
pub const MAX_SHARE: f32 = 0.7;

/// The folded panel's height: its header.
pub const HEADER: f32 = 24.0;
const HANDLE: f32 = 7.0;
const ROW: f32 = 20.0;
/// A row's height before it was ever laid out (a title or a collection's
/// line, and one line under it).
const ROW_GUESS: f32 = 40.0;
/// What a second line and a copy's collections are indented by (past the
/// status icon).
const INDENT: i8 = 20;

/// A connection as the entry names it, captured when the request was sent
/// (the connection may close later): its name and its profile's colour.
#[derive(Clone, Debug, PartialEq)]
pub struct ConnLabel {
    pub name: String,
    pub color: Option<Color32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Running,
    Done,
    /// Done, with something to know: what made its toast an error toast
    /// (skipped, refused, unconfirmed, a history warning, a collection of a
    /// copy that failed).
    Warning,
    Failed,
    /// Stopped by the user (⊗, a Cancel): between batches, with exact counts,
    /// or before it began. A stop the user didn't ask for (the history's
    /// size limit, a switch to read-only) is a Warning, or Failed when
    /// nothing was written.
    Stopped,
}

impl Status {
    fn word(self) -> &'static str {
        match self {
            Status::Running => "Running",
            Status::Done => "Done",
            Status::Warning => "Done, with something to know",
            Status::Failed => "Failed",
            Status::Stopped => "Stopped",
        }
    }
}

/// One request in the panel.
pub struct Entry {
    pub id: RequestId,
    /// The status bar's words for it ("Duplicating shop.users").
    pub title: String,
    /// Its connection; a copy's two ends, source first.
    pub conns: Vec<ConnLabel>,
    /// When it was sent, by the wall clock.
    pub at: DateTime,
    pub started: Instant,
    /// How long it took, once finished.
    pub took: Option<Duration>,
    pub status: Status,
    /// Whether ⊗ can stop it (`Sent::cancellable`).
    pub cancellable: bool,
    /// The user asked it to stop (⊗, a Cancel): its answer is on its way.
    pub stopping: bool,
    /// The latest progress: done, total (if known) and a note.
    pub progress: Option<(u64, Option<u64>, String)>,
    /// What it did: the text of the toasts its answer gave, whole.
    pub result: Option<String>,
    /// An export's file.
    pub path: Option<PathBuf>,
    /// A copy's collections, in the request's order.
    pub children: Vec<Child>,
    /// The toasts said while its answer is handled, and whether one was an error.
    said: Vec<String>,
    said_error: bool,
}

impl Entry {
    /// A running entry; `items` (a copy's source and target of each
    /// collection) make its children.
    pub fn new(id: RequestId, title: String, conns: Vec<ConnLabel>, at: DateTime, started: Instant, cancellable: bool, items: &[(Namespace, Namespace)]) -> Self {
        let children = items.iter().map(|(src, dst)| Child { src: src.clone(), dst: dst.clone(), state: ChildState::Waiting, started: None, took: None, last: None }).collect();
        Entry { id, title, conns, at, started, took: None, status: Status::Running, cancellable, stopping: false, progress: None, result: None, path: None, children, said: Vec::new(), said_error: false }
    }

    fn running(&self) -> bool {
        self.status == Status::Running
    }

    /// How long it has run, or took.
    fn duration(&self, now: Instant) -> Duration {
        self.took.unwrap_or_else(|| now.saturating_duration_since(self.started))
    }

    /// Its second line: the progress while it runs, then what it did.
    fn summary(&self) -> String {
        if !self.running() {
            return self.result.clone().unwrap_or_else(|| self.status.word().to_string());
        }
        if self.stopping {
            return "Stopping…".into();
        }
        match &self.progress {
            None => "Running…".into(),
            Some((done, total, note)) => {
                let count = match total {
                    Some(t) => format!("{} / {}", thousands(*done), thousands(*t)),
                    None => thousands(*done),
                };
                if note.is_empty() { count } else { format!("{note} {count}") }
            }
        }
    }

    /// The whole entry as plain text, for Copy: its title, connection, when
    /// it started and how long it took, its status, its result, and a line
    /// for each collection of a copy.
    pub fn text(&self, now: Instant, tz: DisplayTz) -> String {
        let mut lines = vec![self.title.clone()];
        if !self.conns.is_empty() {
            lines.push(self.conns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(" → "));
        }
        let how_long = theme::elapsed(self.duration(now));
        lines.push(match self.took {
            Some(_) => format!("Started {}, took {how_long}", when(self.at, tz)),
            None => format!("Started {}, running for {how_long}", when(self.at, tz)),
        });
        lines.push(self.status.word().to_string());
        let summary = self.summary();
        if summary != self.status.word() {
            lines.push(summary);
        }
        if let Some(p) = &self.path {
            lines.push(format!("File: {}", p.display()));
        }
        for c in &self.children {
            let mut line = format!("  {}: {}", c.label(), c.detail().map_or_else(|| "waiting".to_string(), |(t, _)| t));
            if let Some(t) = c.took {
                line.push_str(&format!(" ({})", theme::elapsed(t)));
            }
            lines.push(line);
        }
        lines.join("\n")
    }
}

/// One collection of a copy.
pub struct Child {
    pub src: Namespace,
    pub dst: Namespace,
    pub state: ChildState,
    /// From the first progress naming it…
    started: Option<Instant>,
    /// …to the first naming the next one, or the copy's answer.
    took: Option<Duration>,
    /// Its latest progress: done and total.
    last: Option<(u64, Option<u64>)>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ChildState {
    Waiting,
    Running { done: u64, total: Option<u64> },
    /// A later collection started: this one ended, with `done` documents
    /// read across. How it ended (its indexes may still have failed) comes
    /// with the copy's answer: no ✓ until then.
    Ended { done: u64 },
    Copied { inserted: u64, skipped: u64, overwritten: u64, warning: Option<String> },
    Failed { error: String, unconfirmed: bool },
    /// The copy was stopped (⊗) while it copied this one.
    Stopped { done: u64 },
    /// The copy was stopped before it.
    NotCopied,
}

fn plural(n: u64) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn documents(n: u64) -> String {
    format!("{} document{}", thousands(n), plural(n))
}

impl Child {
    /// `src → dst`, the target's database said only when it differs.
    pub fn label(&self) -> String {
        if self.src.db == self.dst.db { format!("{} → {}", self.src, self.dst.coll) } else { format!("{} → {}", self.src, self.dst) }
    }

    /// The line under it, and its colour; none while it waits.
    pub fn detail(&self) -> Option<(String, Color32)> {
        Some(match &self.state {
            ChildState::Waiting => return None,
            ChildState::Running { done, total: Some(t) } => (format!("{} / {}", thousands(*done), thousands(*t)), theme::MUTED),
            ChildState::Running { done, total: None } => (thousands(*done), theme::MUTED),
            ChildState::Ended { done } => (documents(*done), theme::MUTED),
            ChildState::Copied { inserted, skipped, overwritten, warning } => {
                let mut s = documents(*inserted);
                if *skipped > 0 {
                    s.push_str(&format!(", {} skipped", thousands(*skipped)));
                }
                if *overwritten > 0 {
                    s.push_str(&format!(", {} overwritten", thousands(*overwritten)));
                }
                match warning {
                    Some(w) => (format!("{s}; {w}"), theme::WARN),
                    None => (s, theme::MUTED),
                }
            }
            ChildState::Failed { error, unconfirmed: true } => (format!("The server did not confirm a write: it may have been applied. Check before retrying. ({error})"), theme::ERROR),
            ChildState::Failed { error, .. } => (error.clone(), theme::ERROR),
            ChildState::Stopped { done } => (format!("Stopped after {}", documents(*done)), theme::MUTED),
            ChildState::NotCopied => ("Not copied".into(), theme::MUTED),
        })
    }

    fn final_state(&self) -> bool {
        !matches!(self.state, ChildState::Waiting | ChildState::Running { .. } | ChildState::Ended { .. })
    }

    /// Its time ends now, if it was running.
    fn end(&mut self, now: Instant) {
        if self.took.is_none()
            && let Some(s) = self.started
        {
            self.took = Some(now.saturating_duration_since(s));
        }
    }

    /// A later collection started.
    fn pass(&mut self, now: Instant) {
        match self.state {
            ChildState::Running { done, .. } => {
                self.end(now);
                self.state = ChildState::Ended { done };
            }
            // Passed without a report (a view, or refused at once).
            ChildState::Waiting => self.state = ChildState::Ended { done: 0 },
            _ => {}
        }
    }

    /// Its progress: it runs.
    fn run(&mut self, done: u64, total: Option<u64>, now: Instant) {
        if matches!(self.state, ChildState::Waiting) {
            self.started = Some(now);
        }
        if matches!(self.state, ChildState::Waiting | ChildState::Running { .. }) {
            self.state = ChildState::Running { done, total };
            self.last = Some((done, total));
        }
    }

    /// Whether its documents were all read across: its last report reached
    /// its total (a copy always reports that once a collection's documents
    /// are done).
    fn reached_total(&self) -> bool {
        self.last.is_some_and(|(done, total)| Some(done) == total)
    }
}

/// What the app makes of a logged request's terminal answer, before the
/// toasts its handling gives are counted.
pub struct Answer {
    /// Done, Warning, Failed or Stopped; Done turns Warning when an error
    /// toast is said for it, and Stopped turns Warning (Failed when
    /// `nothing_done`) unless the user asked for the stop.
    pub status: Status,
    /// No document was written (nor may have been: an unconfirmed write
    /// counts as written).
    pub nothing_done: bool,
    /// The result when handling it toasts nothing.
    pub fallback: Option<String>,
    /// An export's file.
    pub path: Option<PathBuf>,
    /// A copy's report, for its children.
    pub copied: Option<CopyReport>,
}

/// The entries, and the panel's state: folded or not, and its height.
pub struct Operations {
    /// Oldest first (by request id, which grows).
    entries: Vec<Entry>,
    /// Unfolded.
    pub open: bool,
    /// The panel's height (header and list), as the handle left it.
    pub height: f32,
    /// The sidebar's height, last frame.
    room: f32,
    /// The request whose answer is being handled: the toasts said meanwhile
    /// are its result.
    answering: Option<RequestId>,
    /// The list's rows as last laid out.
    rows: Rows,
}

impl Default for Operations {
    fn default() -> Self {
        Operations { entries: Vec::new(), open: false, height: DEFAULT_HEIGHT, room: 0.0, answering: None, rows: Rows::default() }
    }
}

/// The list's rows (an entry's own, by `(id, 0)`, and each collection of a
/// copy, by `(id, k + 1)`), as last laid out: only rows in view are laid
/// out; the others take the height they had then (`ROW_GUESS` before), so
/// a long list costs a frame what is on screen.
#[derive(Default)]
struct Rows {
    heights: HashMap<(RequestId, usize), f32>,
    /// How many rows the last frame laid out.
    laid_out: usize,
    /// The list scrolled last frame: its scrollbar shows.
    scrolls: bool,
}

impl Operations {
    #[cfg(test)]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// How many rows the last frame laid out.
    #[cfg(test)]
    pub fn rows_laid_out(&self) -> usize {
        self.rows.laid_out
    }

    /// Forgets the heights of rows whose entry has gone.
    fn prune_rows(&mut self) {
        let entries = &self.entries;
        self.rows.heights.retain(|(id, _), _| entries.binary_search_by_key(id, |e| e.id).is_ok());
    }

    fn index(&self, id: RequestId) -> Option<usize> {
        self.entries.binary_search_by_key(&id, |e| e.id).ok()
    }

    pub fn get(&self, id: RequestId) -> Option<&Entry> {
        self.index(id).map(|i| &self.entries[i])
    }

    fn get_mut(&mut self, id: RequestId) -> Option<&mut Entry> {
        self.index(id).map(|i| &mut self.entries[i])
    }

    pub fn has(&self, id: RequestId) -> bool {
        self.index(id).is_some()
    }

    pub fn is_running(&self, id: RequestId) -> bool {
        self.get(id).is_some_and(Entry::running)
    }

    /// How many are running.
    pub fn running(&self) -> usize {
        self.entries.iter().filter(|e| e.running()).count()
    }

    pub fn any_finished(&self) -> bool {
        self.entries.iter().any(|e| !e.running())
    }

    /// Adds a request just sent; past `MAX_ENTRIES` the oldest finished
    /// entries go.
    pub fn add(&mut self, e: Entry) {
        let at = self.entries.partition_point(|x| x.id < e.id);
        if self.entries.get(at).is_some_and(|x| x.id == e.id) {
            return;
        }
        self.entries.insert(at, e);
        let before = self.entries.len();
        while self.entries.len() > MAX_ENTRIES {
            match self.entries.iter().position(|x| !x.running()) {
                Some(i) => _ = self.entries.remove(i),
                None => break,
            }
        }
        if self.entries.len() < before {
            self.prune_rows();
        }
    }

    /// Request `id` reported progress; `item`, a copy's collection being
    /// copied: those before it have ended, it runs.
    pub fn progress(&mut self, id: RequestId, done: u64, total: Option<u64>, note: &str, item: Option<usize>, now: Instant) {
        let Some(e) = self.get_mut(id).filter(|e| e.running()) else { return };
        e.progress = Some((done, total, note.to_string()));
        let Some(k) = item else { return };
        for (j, c) in e.children.iter_mut().enumerate() {
            match j.cmp(&k) {
                std::cmp::Ordering::Less => c.pass(now),
                std::cmp::Ordering::Equal => c.run(done, total, now),
                std::cmp::Ordering::Greater => break,
            }
        }
    }

    /// The user asked it to stop (⊗, or a History tab's Cancel).
    pub fn stopping(&mut self, id: RequestId) {
        if let Some(e) = self.get_mut(id).filter(|e| e.running()) {
            e.stopping = true;
        }
    }

    /// Request `id`'s answer is being handled: whether it is a running
    /// entry, whose result the toasts said until `finish` will be.
    pub fn begin_answer(&mut self, id: RequestId) -> bool {
        let running = self.is_running(id);
        self.answering = running.then_some(id);
        running
    }

    /// A toast: part of the result of the answer being handled, if any.
    pub fn said(&mut self, text: &str, error: bool) {
        if let Some(id) = self.answering
            && let Some(e) = self.get_mut(id)
        {
            e.said.push(text.to_string());
            e.said_error |= error;
        }
    }

    /// Request `id` has answered: its status, result, file and, for a copy,
    /// each collection's outcome.
    pub fn finish(&mut self, id: RequestId, a: Answer, now: Instant) {
        if self.answering == Some(id) {
            self.answering = None;
        }
        let Some(e) = self.get_mut(id).filter(|e| e.running()) else { return };
        e.took = Some(now.saturating_duration_since(e.started));
        e.status = match a.status {
            // Stopped by a problem, not by the user: the history's size
            // limit, a switch to read-only, a write not confirmed.
            Status::Stopped if !e.stopping && a.nothing_done => Status::Failed,
            Status::Stopped if !e.stopping => Status::Warning,
            Status::Done | Status::Running if e.said_error => Status::Warning,
            Status::Running => Status::Done,
            s => s,
        };
        let said = std::mem::take(&mut e.said);
        e.result = if said.is_empty() { a.fallback } else { Some(said.join("\n")) };
        e.path = a.path;
        e.progress = None;
        e.stopping = false;
        end_children(&mut e.children, a.copied.as_ref(), now);
    }

    /// Request `id` is gone without doing anything (a recorded write refused
    /// up front because its history would not fit): no entry.
    pub fn forget(&mut self, id: RequestId) {
        if let Some(i) = self.index(id) {
            self.entries.remove(i);
            self.prune_rows();
        }
        if self.answering == Some(id) {
            self.answering = None;
        }
    }

    /// Remove: a finished entry goes.
    pub fn remove(&mut self, id: RequestId) {
        if let Some(i) = self.index(id).filter(|i| !self.entries[*i].running()) {
            self.entries.remove(i);
            self.prune_rows();
        }
    }

    /// Clear Finished: only the running entries stay.
    pub fn clear_finished(&mut self) {
        self.entries.retain(Entry::running);
        self.prune_rows();
    }

    /// Running entries first, then finished ones, newest first in each.
    pub fn shown(&self) -> impl Iterator<Item = &Entry> {
        let running = self.entries.iter().rev().filter(|e| e.running());
        running.chain(self.entries.iter().rev().filter(|e| !e.running()))
    }

    /// The sidebar's `area` cut in two: the tree's part, and the panel's
    /// under it (the header only while folded).
    pub fn split(&mut self, area: Rect) -> (Rect, Rect) {
        self.room = area.height();
        let h = if self.open { self.panel_height() + HANDLE } else { HEADER };
        let cut = (area.bottom() - h).max(area.top());
        (Rect::from_min_max(area.min, pos2(area.right(), cut)), Rect::from_min_max(pos2(area.left(), cut), area.max))
    }

    /// The panel's height in the sidebar as it is: at least `MIN_HEIGHT`, at
    /// most `MAX_SHARE` of the sidebar.
    fn panel_height(&self) -> f32 {
        let most = (self.room * MAX_SHARE).max(HEADER);
        self.height.clamp(MIN_HEIGHT.min(most), most)
    }
}

/// A copy has answered: each collection the report lists gets its counts,
/// its error or its warning (matched in order by target); those it doesn't
/// list were not copied (it was stopped before them, or failed before it
/// began). Of a stopped copy, the last collection listed was cut short
/// unless its documents had all been read across.
fn end_children(children: &mut [Child], report: Option<&CopyReport>, now: Instant) {
    if let Some(r) = report {
        let mut next = 0;
        for (n, item) in r.items.iter().enumerate() {
            let Some(k) = (next..children.len()).find(|&k| children[k].dst == item.dst) else { continue };
            next = k + 1;
            let c = &mut children[k];
            let cut = r.cancelled && n + 1 == r.items.len() && !c.reached_total();
            c.end(now);
            c.state = match &item.error {
                Some(e) => ChildState::Failed { error: e.clone(), unconfirmed: item.unconfirmed },
                None if cut => ChildState::Stopped { done: item.inserted + item.overwritten + item.skipped },
                None => ChildState::Copied { inserted: item.inserted, skipped: item.skipped, overwritten: item.overwritten, warning: item.warning.clone() },
            };
        }
    }
    for c in children.iter_mut().filter(|c| !c.final_state()) {
        c.end(now);
        c.state = ChildState::NotCopied;
    }
}

/// The right-click item that shows an export's file in the file manager,
/// named as the system names it: Windows texts never say Finder or Mac.
pub fn reveal_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "Reveal in Finder"
    } else if cfg!(windows) {
        "Show in Explorer"
    } else {
        "Show in Folder"
    }
}

/// What the panel asks of the app.
#[derive(Clone, Debug, PartialEq)]
pub enum OpsAction {
    /// ⊗ on a running request (`RongoApp::cancel_op`).
    Cancel(RequestId),
    /// Show an export's file in the file manager.
    Reveal(PathBuf),
}

/// The time an entry started, `HH:MM:SS` in the Settings time zone.
fn clock(at: DateTime, tz: DisplayTz) -> String {
    let s = format::iso_date_in(at, tz);
    s.get(11..19).map_or_else(|| s.clone(), str::to_string)
}

/// The panel, in `ui` (the part of the sidebar `Operations::split` gave
/// it): the drag handle while unfolded, the header, and the list.
pub fn panel(ui: &mut Ui, ops: &mut Operations, now: Instant, tz: DisplayTz) -> Vec<OpsAction> {
    let area = ui.max_rect();
    let mut actions = Vec::new();
    let mut top = area.top();
    if ops.open {
        let handle = Rect::from_min_size(area.min, vec2(area.width(), HANDLE));
        let r = ui.interact(handle, egui::Id::new("operations-resize"), Sense::DRAG);
        if r.dragged() {
            let most = (ops.room * MAX_SHARE).max(HEADER);
            ops.height = (ops.panel_height() - r.drag_delta().y).clamp(MIN_HEIGHT.min(most), most);
        }
        let color = if r.hovered() || r.dragged() { theme::ACCENT } else { theme::BORDER };
        ui.painter().hline(handle.x_range(), handle.center().y, egui::Stroke::new(1.0, color));
        r.on_hover_cursor(egui::CursorIcon::ResizeVertical);
        top = handle.bottom();
    }
    let header = Rect::from_min_max(pos2(area.left(), top), pos2(area.right(), (top + HEADER).min(area.bottom())));
    header_ui(ui, ops, header);
    if ops.open {
        let list = Rect::from_min_max(pos2(area.left(), header.bottom() + 4.0), area.max);
        if list.height() > 0.0 {
            let mut lui = ui.new_child(UiBuilder::new().max_rect(list).layout(Layout::top_down(Align::Min)));
            lui.set_clip_rect(list.intersect(ui.clip_rect()));
            list_ui(&mut lui, ops, now, tz, &mut actions);
        }
        // Elapsed times and the busy marks (sent progress comes at most every 100 ms).
        if ops.running() > 0 {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
    }
    actions
}

/// ▸/▾ Operations · N running, folding the panel on a click, and Clear
/// Finished on the right.
fn header_ui(ui: &mut Ui, ops: &mut Operations, rect: Rect) {
    ui.painter().rect_filled(rect, 4.0, theme::RAISED);
    let mut h = ui.new_child(UiBuilder::new().max_rect(rect.shrink2(vec2(6.0, 0.0))).layout(Layout::right_to_left(Align::Center)));
    h.set_clip_rect(rect.intersect(ui.clip_rect()));
    let clear = h
        .add_enabled(ops.any_finished(), egui::Button::new(RichText::new(icon::ERASER).size(14.0)).frame(false))
        .on_hover_text("Clear Finished")
        .on_disabled_hover_text("Clear Finished: nothing has finished");
    if clear.clicked() {
        ops.clear_finished();
    }
    let rest = h.available_rect_before_wrap();
    let r = h.interact(rest, egui::Id::new("operations-header"), Sense::CLICK);
    let font = TextStyle::Body.resolve(h.style());
    let mut job = LayoutJob::default();
    let caret = if ops.open { icon::CARET_DOWN } else { icon::CARET_RIGHT };
    job.append(caret, 0.0, TextFormat::simple(font.clone(), theme::MUTED));
    job.append("  Operations", 0.0, TextFormat::simple(font.clone(), theme::TEXT));
    let running = ops.running();
    if running > 0 {
        job.append(&format!(" · {running} running"), 0.0, TextFormat::simple(font, theme::ACCENT));
    }
    job.wrap.max_width = rest.width().max(20.0);
    job.wrap.max_rows = 1;
    job.wrap.overflow_character = Some('…');
    let galley = h.fonts_mut(|f| f.layout_job(job));
    let pos = pos2(rest.left(), rest.center().y - galley.size().y / 2.0);
    h.painter().galley(pos, galley, theme::TEXT);
    if r.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
        ops.open = !ops.open;
    }
}

/// The entries, scrolled, laid out only where in view; Remove applied once
/// drawn. While the list scrolls, its rows keep clear of the scrollbar
/// (egui's floats over the content, the app's default): ⊗ is a row's
/// right-most widget, and the bar would take clicks on half of it.
fn list_ui(ui: &mut Ui, ops: &mut Operations, now: Instant, tz: DisplayTz, actions: &mut Vec<OpsAction>) {
    let mut remove = None;
    let mut rows = std::mem::take(&mut ops.rows);
    rows.laid_out = 0;
    let bar = if rows.scrolls {
        let s = &ui.spacing().scroll;
        s.bar_inner_margin + s.bar_width + s.bar_outer_margin
    } else {
        0.0
    };
    let out = egui::ScrollArea::vertical().id_salt("operations").auto_shrink([false, false]).show(ui, |ui| {
        ui.set_max_width((ui.available_width() - bar).max(40.0));
        if ops.entries.is_empty() {
            ui.add(egui::Label::new(RichText::new("Writes, copies, imports and exports of this session show here.").small().color(theme::MUTED)).wrap().selectable(false));
        }
        for e in ops.shown() {
            match entry_ui(ui, e, &mut rows, now, tz) {
                Some(EntryAct::Remove) => remove = Some(e.id),
                Some(EntryAct::Panel(a)) => actions.push(a),
                None => {}
            }
        }
    });
    rows.scrolls = out.content_size.y > out.inner_rect.height();
    ops.rows = rows;
    if let Some(id) = remove {
        ops.remove(id);
    }
}

enum EntryAct {
    Panel(OpsAction),
    Remove,
}

/// `add` indented by `by` points: a Frame's margin, so wrapped text inside
/// wraps at the width left (labels in a horizontal row never wrap).
fn indented<R>(ui: &mut Ui, by: i8, add: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new().inner_margin(egui::Margin { left: by, right: 0, top: 0, bottom: 0 }).show(ui, add).inner
}

/// A small turning arc: running. Drawn by the clock, asking no repaint of
/// its own (the panel asks every 100 ms while something runs).
fn busy(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
    let t = ui.input(|i| i.time);
    let start = (t * 1.5).fract() * std::f64::consts::TAU;
    let points: Vec<egui::Pos2> = (0..=16)
        .map(|i| {
            let a = start + f64::from(i) / 16.0 * 4.2;
            rect.center() + 5.0 * vec2(a.cos() as f32, a.sin() as f32)
        })
        .collect();
    ui.painter().add(egui::Shape::line(points, egui::Stroke::new(1.8, theme::ACCENT)));
}

fn mark(ui: &mut Ui, glyph: &str, color: Color32) -> egui::Response {
    ui.add_sized(vec2(14.0, 14.0), egui::Label::new(RichText::new(glyph).color(color)).selectable(false))
}

fn status_icon(ui: &mut Ui, e: &Entry) {
    match e.status {
        Status::Running => busy(ui),
        Status::Done => _ = mark(ui, icon::CHECK, theme::OK),
        Status::Warning => _ = mark(ui, icon::WARNING, theme::WARN),
        Status::Failed => _ = mark(ui, icon::X, theme::ERROR),
        // Stopped as asked, though with something to know (its toast was an error).
        Status::Stopped if e.said_error => _ = mark(ui, icon::PROHIBIT, theme::WARN),
        Status::Stopped => _ = mark(ui, icon::PROHIBIT, theme::MUTED),
    }
}

fn child_icon(ui: &mut Ui, s: &ChildState) {
    match s {
        ChildState::Waiting => _ = mark(ui, icon::CIRCLE_DASHED, theme::MUTED),
        ChildState::Running { .. } => busy(ui),
        ChildState::Ended { .. } => _ = mark(ui, icon::CIRCLE, theme::MUTED).on_hover_text("Its documents went across; how it ended shows once the copy is done"),
        ChildState::Copied { warning: Some(_), .. } => _ = mark(ui, icon::WARNING, theme::WARN),
        ChildState::Copied { .. } => _ = mark(ui, icon::CHECK, theme::OK),
        ChildState::Failed { .. } => _ = mark(ui, icon::X, theme::ERROR),
        ChildState::Stopped { .. } | ChildState::NotCopied => _ = mark(ui, icon::PROHIBIT, theme::MUTED),
    }
}

/// `parts` (text and colour) on one row of `width`, cut with "…" and whole
/// on hover: a long title or name never widens the panel.
fn one_line(ui: &mut Ui, parts: &[(&str, Color32)], width: f32) {
    let font = TextStyle::Body.resolve(ui.style());
    let mut job = LayoutJob::default();
    for (text, color) in parts {
        job.append(text, 0.0, TextFormat::simple(font.clone(), *color));
    }
    job.wrap.max_width = width.max(20.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    let cut = galley.elided;
    let r = ui.add(egui::Label::new(galley).selectable(false));
    if cut {
        r.on_hover_text(parts.iter().map(|(t, _)| *t).collect::<String>());
    }
}

/// A wrapped line of small text, never clipped.
fn wrapped(ui: &mut Ui, text: &str, color: Color32) {
    wrapped_parts(ui, &[(text, color)]);
}

/// `parts` (text and colour) as small text, wrapped at the width left and
/// never clipped.
fn wrapped_parts(ui: &mut Ui, parts: &[(&str, Color32)]) {
    let font = TextStyle::Small.resolve(ui.style());
    let mut job = LayoutJob::default();
    for (text, color) in parts {
        job.append(text, 0.0, TextFormat::simple(font.clone(), *color));
    }
    job.wrap.max_width = ui.available_width().max(20.0);
    ui.add(egui::Label::new(job).wrap().selectable(false));
}

/// The arrow drawn between a copy's ends: the icon font's, which sits on
/// the line (the text fonts' "→" comes from a fallback font, raised).
fn arrows(text: &str) -> String {
    text.replace('→', icon::ARROW_RIGHT)
}

/// One row of the list (`key`: an entry's own, or one collection of a
/// copy), laid out only while it is in view: out of view it takes the
/// height it had when last laid out (`ROW_GUESS` before that). Answers the
/// row's response when laid out; each row senses clicks (not focusable),
/// below its own widgets, for the entry's right-click menu. Its id is its
/// key's, explicitly: an `id_salt` child's id also counts the children
/// laid out before it, which skipping rows changes, and a click pressed on
/// one id and released on another is lost.
fn row(ui: &mut Ui, rows: &mut Rows, key: (RequestId, usize), add: impl FnOnce(&mut Ui)) -> Option<egui::Response> {
    let top = ui.cursor().top();
    let height = rows.heights.get(&key).copied().unwrap_or(ROW_GUESS);
    let view = ui.clip_rect();
    if top > view.bottom() || top + height < view.top() {
        ui.add_space(height);
        return None;
    }
    rows.laid_out += 1;
    let width = ui.available_width();
    let r = ui
        .scope_builder(UiBuilder::new().id(ui.id().with(("operation", key))).sense(Sense::CLICK), |ui| {
            ui.set_width(width);
            add(ui);
        })
        .response;
    rows.heights.insert(key, ui.cursor().top() - top);
    Some(r)
}

/// One entry: icon and title, the time it started and how long it ran on
/// the right with ⊗ while it can be stopped; under it its connection (a
/// copy's two ends) and its progress or what it did; then a copy's
/// collections, a row each. Right-click any of its rows: Copy, Reveal (an
/// export's file), Remove (once finished).
fn entry_ui(ui: &mut Ui, e: &Entry, rows: &mut Rows, now: Instant, tz: DisplayTz) -> Option<EntryAct> {
    let mut act = None;
    let hover = ui.painter().add(egui::Shape::Noop);
    let (top, left, width) = (ui.cursor().top(), ui.cursor().left(), ui.available_width());
    let mut lit = false;
    let mut responses = Vec::new();
    responses.extend(row(ui, rows, (e.id, 0), |ui| head_ui(ui, e, now, tz, &mut act)));
    for (k, c) in e.children.iter().enumerate() {
        responses.extend(row(ui, rows, (e.id, k + 1), |ui| indented(ui, INDENT, |ui| child_ui(ui, c, now))));
    }
    for r in &responses {
        lit |= r.contains_pointer() || r.context_menu_opened();
        if let Some(a) = entry_menu(r, e, now, tz) {
            act = Some(a);
        }
    }
    if lit {
        let rect = Rect::from_min_max(pos2(left - 2.0, top - 1.0), pos2(left + width + 2.0, ui.cursor().top()));
        ui.painter().set(hover, egui::Shape::rect_filled(rect, 4.0, theme::RAISED));
    }
    ui.add_space(4.0);
    act
}

/// An entry's own row: its icon, title, times and ⊗, and the line under it.
fn head_ui(ui: &mut Ui, e: &Entry, now: Instant, tz: DisplayTz, act: &mut Option<EntryAct>) {
    let width = ui.available_width();
    ui.allocate_ui_with_layout(vec2(width, ROW), Layout::right_to_left(Align::Center), |ui| {
        if e.running() && e.cancellable && !e.stopping && theme::discard_button(ui) {
            *act = Some(EntryAct::Panel(OpsAction::Cancel(e.id)));
        }
        let times = format!("{} · {}", clock(e.at, tz), theme::elapsed(e.duration(now)));
        ui.add(egui::Label::new(RichText::new(times).small().color(theme::MUTED)).selectable(false));
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            status_icon(ui, e);
            let w = ui.available_width();
            one_line(ui, &[(&e.title, theme::TEXT)], w);
        });
    });
    // The connection leads the wrapped line, so a narrow sidebar never
    // cuts it (it says which server: prod, or a test one).
    let arrow = format!(" {} ", icon::ARROW_RIGHT);
    let summary = arrows(&e.summary());
    let mut parts: Vec<(&str, Color32)> = Vec::new();
    for (i, c) in e.conns.iter().enumerate() {
        if i > 0 {
            parts.push((&arrow, theme::MUTED));
        }
        parts.push((&c.name, c.color.unwrap_or(theme::TEXT)));
    }
    if !parts.is_empty() {
        parts.push(("  ", theme::MUTED));
    }
    parts.push((&summary, theme::MUTED));
    indented(ui, INDENT, |ui| wrapped_parts(ui, &parts));
}

/// The right-click menu of an entry's row: Copy, Reveal, Remove.
fn entry_menu(r: &egui::Response, e: &Entry, now: Instant, tz: DisplayTz) -> Option<EntryAct> {
    let mut act = None;
    r.context_menu(|ui| {
        if ui.button(format!("{}  Copy", icon::COPY)).clicked() {
            ui.ctx().copy_text(e.text(now, tz));
            ui.close();
        }
        if let Some(p) = &e.path
            && ui.button(format!("{}  {}", icon::FOLDER_OPEN, reveal_label())).clicked()
        {
            act = Some(EntryAct::Panel(OpsAction::Reveal(p.clone())));
            ui.close();
        }
        if !e.running() && ui.button(format!("{}  Remove", icon::TRASH)).clicked() {
            act = Some(EntryAct::Remove);
            ui.close();
        }
    });
    act
}

/// A collection of a copy: its icon, `src → dst` and its time; under it
/// its count, error or warning.
fn child_ui(ui: &mut Ui, c: &Child, now: Instant) {
    let width = ui.available_width();
    ui.allocate_ui_with_layout(vec2(width, ROW), Layout::right_to_left(Align::Center), |ui| {
        let time = c.took.or_else(|| matches!(c.state, ChildState::Running { .. }).then(|| c.started.map(|s| now.saturating_duration_since(s))).flatten());
        if let Some(t) = time {
            ui.add(egui::Label::new(RichText::new(theme::elapsed(t)).small().color(theme::MUTED)).selectable(false));
        }
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            child_icon(ui, &c.state);
            let w = ui.available_width();
            one_line(ui, &[(&arrows(&c.label()), theme::TEXT)], w);
        });
    });
    if let Some((text, color)) = c.detail() {
        indented(ui, INDENT, |ui| wrapped(ui, &text, color));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rongo_core::CopiedItem;

    fn ns(db: &str, coll: &str) -> Namespace {
        Namespace::new(db, coll)
    }

    fn entry(id: RequestId, items: &[(Namespace, Namespace)]) -> Entry {
        Entry::new(id, format!("Op {id}"), vec![ConnLabel { name: "SIT".into(), color: None }], DateTime::from_millis(0), Instant::now(), true, items)
    }

    fn done(ops: &mut Operations, id: RequestId) {
        ops.finish(id, Answer { status: Status::Done, nothing_done: false, fallback: Some("Done".into()), path: None, copied: None }, Instant::now());
    }

    /// The panel alone, unfolded, in a sidebar of 300 × 900, with `n`
    /// finished entries whose results take one to four lines.
    struct Bench {
        ctx: egui::Context,
        ops: Operations,
        now: Instant,
    }

    impl Bench {
        fn new(n: u64) -> Self {
            let ctx = egui::Context::default();
            theme::install(&ctx);
            let mut ops = Operations { open: true, ..Default::default() };
            let now = Instant::now();
            for id in 1..=n {
                ops.add(entry(id, &[]));
                let result = "inserted 1,000 documents, skipped 3 with an _id already in the collection; ".repeat((id % 4 + 1) as usize);
                ops.finish(id, Answer { status: Status::Done, nothing_done: false, fallback: Some(result), path: None, copied: None }, now);
            }
            Bench { ctx, ops, now }
        }

        /// One frame: each text drawn, with where it is.
        fn frame(&mut self, events: Vec<egui::Event>) -> Vec<(String, Rect)> {
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(300.0, 900.0))), events, ..Default::default() };
            let (ops, now) = (&mut self.ops, self.now);
            let mut out = self.ctx.run_ui(input, |ui| {
                let (_, area) = ops.split(ui.max_rect());
                let mut child = ui.new_child(UiBuilder::new().max_rect(area).layout(Layout::top_down(Align::Min)));
                panel(&mut child, ops, now, DisplayTz::Utc);
            });
            out.textures_delta.clear();
            out.shapes
                .iter()
                .filter_map(|s| match &s.shape {
                    egui::Shape::Text(t) => Some((t.galley.text().to_string(), t.galley.rect.translate(t.pos.to_vec2()).intersect(s.clip_rect))),
                    _ => None,
                })
                .filter(|(_, r)| r.is_positive())
                .collect()
        }

        fn press(at: egui::Pos2, button: egui::PointerButton, pressed: bool) -> egui::Event {
            egui::Event::PointerButton { pos: at, button, pressed, modifiers: egui::Modifiers::NONE }
        }
    }

    /// A copy's collection that has just come into view, right-clicked
    /// `after` frames into a 120 pt wheel step (the wheel scrolls smoothly,
    /// rows coming in are measured, those above leave): its menu opens.
    fn right_click_after_scrolling(after: usize) -> Result<(), String> {
        let mut b = Bench::new(0);
        let items: Vec<(Namespace, Namespace)> = (0..200).map(|i| (ns("s", &format!("c{i}")), ns("s", &format!("c{i}")))).collect();
        b.ops.add(entry(1000, &items));
        b.ops.progress(1000, 10, Some(20), "s.c0 (1/200)", Some(0), b.now);
        b.frame(vec![]);
        let before: Vec<String> = b.frame(vec![]).into_iter().map(|(t, _)| t).collect();
        let wheel = egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: vec2(0.0, -120.0), phase: egui::TouchPhase::Move, modifiers: egui::Modifiers::NONE };
        let mut shapes = b.frame(vec![egui::Event::PointerMoved(pos2(150.0, 800.0)), wheel]);
        for _ in 0..after {
            shapes = b.frame(vec![]);
        }
        let came_in = shapes.iter().filter(|(t, r)| t.starts_with("s.c") && !before.contains(t) && r.height() > 6.0).max_by(|a, b| a.1.top().total_cmp(&b.1.top()));
        let Some((name, rect)) = came_in.cloned() else { return Ok(()) };
        let at = rect.center();
        b.frame(vec![egui::Event::PointerMoved(at), Bench::press(at, egui::PointerButton::Secondary, true)]);
        b.frame(vec![Bench::press(at, egui::PointerButton::Secondary, false)]);
        b.frame(vec![]);
        let menu = b.frame(vec![]);
        if menu.iter().any(|(t, _)| t.ends_with("  Copy")) { Ok(()) } else { Err(format!("{after} frames in: no menu for {name}")) }
    }

    /// A row that has just scrolled into view takes a right-click at once,
    /// whatever the frame of the scroll.
    #[test]
    fn a_row_just_scrolled_in_takes_a_right_click() {
        let missed: Vec<String> = (1..8).filter_map(|after| right_click_after_scrolling(after).err()).collect();
        assert!(missed.is_empty(), "{missed:?}");
    }

    fn copied(src: &Namespace, dst: &Namespace, inserted: u64) -> CopiedItem {
        CopiedItem { src: src.clone(), dst: dst.clone(), inserted, overwritten: 0, skipped: 0, recorded: None, warning: None, error: None, unconfirmed: false }
    }

    /// Running entries come first, newest first, then finished ones.
    #[test]
    fn running_entries_come_first_newest_first() {
        let mut ops = Operations::default();
        for id in [3, 1, 2, 4] {
            ops.add(entry(id, &[]));
        }
        done(&mut ops, 4);
        done(&mut ops, 1);
        assert_eq!(ops.shown().map(|e| e.id).collect::<Vec<_>>(), [3, 2, 4, 1]);
        assert_eq!(ops.running(), 2);
    }

    /// The toasts said while an answer is handled are its result, and an
    /// error toast makes a done entry one with something to know; without
    /// a toast, the fallback is the result.
    #[test]
    fn the_toasts_said_are_the_result() {
        let mut ops = Operations::default();
        ops.add(entry(1, &[]));
        ops.add(entry(2, &[]));
        ops.said("not answering: nobody's", true);
        assert!(ops.begin_answer(1));
        ops.said("Pasted 2 collections", false);
        ops.said("users: duplicate _id 3", true);
        ops.finish(1, Answer { status: Status::Done, nothing_done: false, fallback: Some("unused".into()), path: None, copied: None }, Instant::now());
        let e = ops.get(1).unwrap();
        assert_eq!((e.status, e.result.as_deref()), (Status::Warning, Some("Pasted 2 collections\nusers: duplicate _id 3")));
        assert!(e.took.is_some());
        ops.said("after the answer: nobody's", false);
        assert!(ops.begin_answer(2));
        ops.finish(2, Answer { status: Status::Done, nothing_done: false, fallback: Some("Deleted 3 documents".into()), path: None, copied: None }, Instant::now());
        assert_eq!(ops.get(2).unwrap().result.as_deref(), Some("Deleted 3 documents"));
        assert!(!ops.begin_answer(2), "finished: not answered again");
    }

    /// Past `MAX_ENTRIES` the oldest finished entry goes; running ones stay.
    #[test]
    fn the_oldest_finished_entry_goes_past_the_cap() {
        let mut ops = Operations::default();
        ops.add(entry(1, &[]));
        for id in 2..=MAX_ENTRIES as u64 {
            ops.add(entry(id, &[]));
            done(&mut ops, id);
        }
        assert_eq!(ops.entries().len(), MAX_ENTRIES);
        ops.add(entry(1000, &[]));
        assert_eq!(ops.entries().len(), MAX_ENTRIES);
        assert!(ops.has(1), "running: kept");
        assert!(!ops.has(2) && ops.has(3), "the oldest finished went");
    }

    /// Clear Finished keeps what runs; Remove takes a finished entry only.
    #[test]
    fn clear_finished_and_remove() {
        let mut ops = Operations::default();
        for id in 1..=3 {
            ops.add(entry(id, &[]));
        }
        done(&mut ops, 1);
        done(&mut ops, 2);
        ops.remove(3);
        assert!(ops.has(3), "a running entry can't be removed");
        ops.remove(2);
        assert_eq!(ops.entries().iter().map(|e| e.id).collect::<Vec<_>>(), [1, 3]);
        ops.clear_finished();
        assert_eq!(ops.entries().iter().map(|e| e.id).collect::<Vec<_>>(), [3]);
        assert!(!ops.any_finished());
    }

    /// A copy's progress names its collection: those before it have ended,
    /// it runs, those after it wait. The report then gives each its counts
    /// or its error.
    #[test]
    fn a_copy_follows_its_collections() {
        let (a, b, c) = (ns("shop", "a"), ns("shop", "b"), ns("shop", "c"));
        let items = [(a.clone(), ns("uat", "a")), (b.clone(), ns("shop", "b2")), (c.clone(), ns("uat", "c"))];
        let mut ops = Operations::default();
        ops.add(entry(1, &items));
        let t0 = Instant::now();
        ops.progress(1, 1000, Some(2500), "uat.a (1/3)", Some(0), t0);
        ops.progress(1, 2500, Some(2500), "uat.a (1/3)", Some(0), t0);
        ops.progress(1, 4, Some(8), "shop.b2 (2/3)", Some(1), t0 + Duration::from_secs(3));
        let e = ops.get(1).unwrap();
        let states: Vec<&ChildState> = e.children.iter().map(|c| &c.state).collect();
        assert_eq!(states, [&ChildState::Ended { done: 2500 }, &ChildState::Running { done: 4, total: Some(8) }, &ChildState::Waiting]);
        assert_eq!(e.children[0].took, Some(Duration::from_secs(3)));
        assert_eq!(e.summary(), "shop.b2 (2/3) 4 / 8");
        assert_eq!([e.children[0].label(), e.children[1].label()], ["shop.a → uat.a", "shop.b → b2"]);

        let mut failed = copied(&c, &ns("uat", "c"), 0);
        failed.error = Some("duplicate _id 1".into());
        let report = CopyReport { items: vec![copied(&a, &ns("uat", "a"), 2500), copied(&b, &ns("shop", "b2"), 8), failed], cancelled: false };
        ops.begin_answer(1);
        ops.finish(1, Answer { status: Status::Warning, nothing_done: false, fallback: None, path: None, copied: Some(report) }, t0 + Duration::from_secs(5));
        let e = ops.get(1).unwrap();
        assert_eq!(e.status, Status::Warning);
        let details: Vec<String> = e.children.iter().map(|c| c.detail().unwrap().0).collect();
        assert_eq!(details, ["2,500 documents", "8 documents", "duplicate _id 1"]);
        assert_eq!(e.children[1].took, Some(Duration::from_secs(2)), "from its first report to the answer");
        assert_eq!(e.children[2].detail().unwrap().1, theme::ERROR);
    }

    /// A stopped copy: the collection it was copying shows where it
    /// stopped, those after it were not copied.
    #[test]
    fn a_stopped_copy_says_what_was_not_copied() {
        let items = [(ns("s", "a"), ns("t", "a")), (ns("s", "b"), ns("t", "b")), (ns("s", "c"), ns("t", "c"))];
        let mut ops = Operations::default();
        ops.add(entry(1, &items));
        let now = Instant::now();
        ops.progress(1, 8, Some(8), "t.a (1/3)", Some(0), now);
        ops.progress(1, 1000, Some(25_000), "t.b (2/3)", Some(1), now);
        let report = CopyReport { items: vec![copied(&ns("s", "a"), &ns("t", "a"), 8), copied(&ns("s", "b"), &ns("t", "b"), 2000)], cancelled: true };
        ops.finish(1, Answer { status: Status::Stopped, nothing_done: false, fallback: None, path: None, copied: Some(report) }, now);
        let states: Vec<ChildState> = ops.get(1).unwrap().children.iter().map(|c| c.state.clone()).collect();
        assert_eq!(states, [ChildState::Copied { inserted: 8, skipped: 0, overwritten: 0, warning: None }, ChildState::Stopped { done: 2000 }, ChildState::NotCopied]);
    }

    /// Copy gives the whole entry as text.
    #[test]
    fn copy_gives_the_entry_as_text() {
        let mut ops = Operations::default();
        let mut e = entry(1, &[(ns("s", "a"), ns("t", "a"))]);
        e.conns.push(ConnLabel { name: "UAT".into(), color: None });
        ops.add(e);
        ops.progress(1, 4, Some(4), "t.a (1/1)", Some(0), Instant::now());
        let report = CopyReport { items: vec![copied(&ns("s", "a"), &ns("t", "a"), 4)], cancelled: false };
        ops.begin_answer(1);
        ops.said("Pasted 1 collection into UAT · t: 4 documents", false);
        ops.finish(1, Answer { status: Status::Done, nothing_done: false, fallback: None, path: None, copied: Some(report) }, Instant::now());
        let text = ops.get(1).unwrap().text(Instant::now(), DisplayTz::Utc);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0..2], ["Op 1", "SIT → UAT"]);
        assert!(lines[2].starts_with("Started 1970-01-01 00:00:00 UTC, took "), "{text}");
        assert_eq!(lines[3..5], ["Done", "Pasted 1 collection into UAT · t: 4 documents"]);
        assert_eq!(lines[5], "  s.a → t.a: 4 documents (0s)");
    }

    /// The menu item says the file manager of the system it runs on.
    #[test]
    fn reveal_says_the_systems_file_manager() {
        #[cfg(target_os = "macos")]
        assert_eq!(reveal_label(), "Reveal in Finder");
        #[cfg(windows)]
        assert_eq!(reveal_label(), "Show in Explorer");
        #[cfg(not(target_os = "macos"))]
        assert!(!reveal_label().contains("Finder") && !reveal_label().contains("Mac"));
    }

    /// The handle keeps the panel between its least height and its share
    /// of the sidebar; folded, only the header takes room.
    #[test]
    fn the_panel_takes_its_share_of_the_sidebar() {
        let area = Rect::from_min_size(pos2(0.0, 0.0), vec2(300.0, 1000.0));
        let mut ops = Operations::default();
        let (tree, panel) = ops.split(area);
        assert_eq!((tree.bottom(), panel.height()), (1000.0 - HEADER, HEADER), "folded: the header");
        ops.open = true;
        let (tree, panel) = ops.split(area);
        assert_eq!((tree.bottom(), panel.height()), (1000.0 - DEFAULT_HEIGHT - HANDLE, DEFAULT_HEIGHT + HANDLE));
        ops.height = 5000.0;
        assert_eq!(ops.split(area).1.height(), 700.0 + HANDLE, "at most 70 %");
        ops.height = 10.0;
        assert_eq!(ops.split(area).1.height(), MIN_HEIGHT + HANDLE, "at least 80");
    }

    /// ⊘ is a stop the user asked for. One they didn't (the history's size
    /// limit reached, a switch to read-only) is ⚠ after writing something,
    /// ✗ when nothing was written.
    #[test]
    fn only_a_stop_asked_for_is_stopped() {
        let stopped = |nothing_done| Answer { status: Status::Stopped, nothing_done, fallback: None, path: None, copied: None };
        let mut ops = Operations::default();
        for id in 1..=3 {
            ops.add(entry(id, &[]));
        }
        ops.stopping(1);
        ops.finish(1, stopped(false), Instant::now());
        ops.finish(2, stopped(false), Instant::now());
        ops.finish(3, stopped(true), Instant::now());
        let st: Vec<Status> = (1..=3).map(|id| ops.get(id).unwrap().status).collect();
        assert_eq!(st, [Status::Stopped, Status::Warning, Status::Failed]);
    }

    /// A long list costs a frame what is in view: 199 finished entries and
    /// a copy of 500 collections (700 rows) lay out only the rows on
    /// screen, from the first frame on, after a progress, and scrolled to
    /// the end, where the oldest entry shows.
    #[test]
    fn a_long_list_lays_out_only_what_is_in_view() {
        const IN_VIEW: usize = 16;
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let mut ops = Operations { open: true, ..Default::default() };
        let now = Instant::now();
        for id in 1..200 {
            ops.add(entry(id, &[]));
            done(&mut ops, id);
        }
        let items: Vec<(Namespace, Namespace)> = (0..500).map(|i| (ns("s", &format!("c{i}")), ns("t", &format!("c{i}")))).collect();
        ops.add(entry(1000, &items));
        ops.progress(1000, 10, Some(20), "t.c0 (1/500)", Some(0), now);
        let frame = |ops: &mut Operations, events: Vec<egui::Event>| -> Vec<String> {
            let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(300.0, 900.0))), events, ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| {
                let (_, area) = ops.split(ui.max_rect());
                let mut child = ui.new_child(UiBuilder::new().max_rect(area).layout(Layout::top_down(Align::Min)));
                panel(&mut child, ops, now, DisplayTz::Utc);
            });
            out.textures_delta.clear();
            out.shapes
                .iter()
                .filter_map(|s| match &s.shape {
                    egui::Shape::Text(t) => Some(t.galley.text().to_string()),
                    _ => None,
                })
                .collect()
        };
        for _ in 0..3 {
            let texts = frame(&mut ops, vec![]);
            assert!(ops.rows_laid_out() <= IN_VIEW, "{} rows laid out", ops.rows_laid_out());
            assert!(texts.iter().any(|t| t == "Op 1000"), "the copy, first: {texts:?}");
        }
        ops.progress(1000, 20, Some(20), "t.c1 (2/500)", Some(1), now);
        frame(&mut ops, vec![]);
        assert!(ops.rows_laid_out() <= IN_VIEW, "{} rows laid out", ops.rows_laid_out());
        let wheel = egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: vec2(0.0, -1_000_000.0), phase: egui::TouchPhase::Move, modifiers: egui::Modifiers::NONE };
        let mut texts = Vec::new();
        for i in 0..40 {
            let events = if i % 8 == 0 { vec![egui::Event::PointerMoved(pos2(150.0, 850.0)), wheel.clone()] } else { vec![] };
            texts = frame(&mut ops, events);
            assert!(ops.rows_laid_out() <= IN_VIEW, "{} rows laid out", ops.rows_laid_out());
        }
        assert!(texts.iter().any(|t| t == "Op 1"), "the oldest, at the end: {texts:?}");
    }
}
