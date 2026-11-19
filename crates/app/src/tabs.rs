//! Main-area tabs: collection tabs and IntelliShell tabs.

use std::time::Instant;

use egui::{Align, Key, Layout, Modifiers, RichText, Ui};
use egui_phosphor::regular as icon;
use rongo_core::{ConnId, FindSpec, Namespace, Session, Waker};
use rongo_shell::{ResultItem, ShellError, ShellEvent, ShellHandle};

use crate::editor::{CodeEditor, RunRequest};
use crate::net::{Net, Origin};
use crate::result_view::{ResultView, Source, TextLines, ViewAction, line_view};
use crate::shortcut;
use crate::sidebar::{self, OpenConn};
use crate::theme;

pub enum TabKind {
    Welcome,
    Collection(Box<CollectionTab>),
    Shell(Box<ShellTab>),
    /// A command reply (statistics, server info) shown as a document.
    Info { conn: ConnId, view: Box<ResultView> },
    Indexes(Box<crate::indexes::IndexTab>),
    History(Box<crate::history_tab::HistoryTab>),
}

pub struct Tab {
    pub id: u64,
    pub kind: TabKind,
}

impl Tab {
    pub fn title(&self, conn_name: impl Fn(ConnId) -> String) -> String {
        match &self.kind {
            TabKind::Welcome => format!("{}  Welcome", icon::HOUSE),
            TabKind::Collection(c) => format!("{}  {}", icon::TABLE, c.ns.coll),
            TabKind::Shell(s) => format!("{}  IntelliShell: {}", icon::TERMINAL_WINDOW, conn_name(s.conn)),
            TabKind::Info { view, .. } => format!("{}  {}", icon::INFO, view.title),
            TabKind::Indexes(t) => format!("{}  Index Manager - {}", icon::LIGHTNING, t.ns.coll),
            TabKind::History(h) => format!("{}  Collection History - {}", icon::CLOCK_COUNTER_CLOCKWISE, h.ns.coll),
        }
    }

    pub fn conn(&self) -> Option<ConnId> {
        match &self.kind {
            TabKind::Welcome => None,
            TabKind::Collection(c) => Some(c.conn),
            TabKind::Shell(s) => Some(s.conn),
            TabKind::Info { conn, .. } => Some(*conn),
            TabKind::Indexes(t) => Some(t.conn),
            TabKind::History(h) => Some(h.conn),
        }
    }

    pub fn views_mut(&mut self) -> Vec<&mut ResultView> {
        match &mut self.kind {
            TabKind::Welcome => Vec::new(),
            TabKind::Collection(c) => vec![&mut c.view],
            TabKind::Shell(s) => s.results.iter_mut().map(|r| &mut r.view).collect(),
            TabKind::Info { view, .. } => vec![view.as_mut()],
            TabKind::Indexes(_) => Vec::new(),
            TabKind::History(h) => h.views_mut(),
        }
    }
}

// ---------------------------------------------------------------- collection tab

pub struct CollectionTab {
    pub conn: ConnId,
    pub ns: Namespace,
    filter: String,
    projection: String,
    sort: String,
    skip: String,
    pub view: ResultView,
    query_error: Option<String>,
}

impl CollectionTab {
    pub fn new(conn: ConnId, ns: Namespace, view_id: u64) -> Self {
        let spec = FindSpec::all(ns.clone(), 50);
        let view = ResultView::new(view_id, ns.coll.clone(), Source::Find { conn, spec, max: None });
        Self {
            conn,
            ns,
            filter: String::new(),
            projection: String::new(),
            sort: String::new(),
            skip: String::new(),
            view,
            query_error: None,
        }
    }

    fn run(&mut self) {
        let parse = |label: &str, text: &str| rongo_shell::parse_document(text).map_err(|e| format!("{label}: {}", e.message));
        let result = (|| {
            let filter = parse("Query", &self.filter)?;
            let projection = parse("Projection", &self.projection)?;
            let sort = parse("Sort", &self.sort)?;
            let skip = match self.skip.trim() {
                "" => 0,
                s => s.parse::<u64>().map_err(|_| "Skip must be a whole number".to_string())?,
            };
            Ok::<_, String>(FindSpec {
                ns: self.ns.clone(),
                filter,
                sort: (!sort.is_empty()).then_some(sort),
                projection: (!projection.is_empty()).then_some(projection),
                skip,
                limit: 50,
            })
        })();
        match result {
            Ok(spec) => {
                self.query_error = None;
                self.view.set_spec(spec);
            }
            Err(e) => self.query_error = Some(e),
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, net: &mut Net, conn: Option<&OpenConn>) -> Vec<(u64, ViewAction)> {
        sidebar::breadcrumb(ui, conn, &self.ns.db);
        ui.add_space(4.0);
        let mut run = false;
        egui::Frame::new().fill(theme::RAISED).inner_margin(6.0).corner_radius(4.0).show(ui, |ui| {
            // The fields share the full width (a query can be long); on a
            // narrow tab the buttons wrap below instead of going off screen.
            let full = ui.available_width();
            let label_w = 72.0;
            let gap = ui.spacing().item_spacing.x;
            let mono = |ui: &mut Ui, text: &mut String, hint: &str, w: f32| {
                let r = ui.add(egui::TextEdit::singleline(text).font(theme::mono(13.0)).hint_text(hint).desired_width(w));
                r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter))
            };
            let label = |ui: &mut Ui, text: &str| {
                ui.allocate_ui_with_layout(egui::vec2(label_w, 20.0), Layout::left_to_right(Align::Center), |ui| {
                    ui.set_min_width(label_w);
                    ui.label(text);
                });
            };
            ui.horizontal_wrapped(|ui| {
                label(ui, "Query");
                let buttons = 130.0;
                run |= mono(ui, &mut self.filter, "{ status: \"active\" }", (full - label_w - buttons - 3.0 * gap).max(160.0));
                if ui.button(RichText::new(format!("{}  Run", icon::PLAY)).color(theme::OK)).clicked() {
                    run = true;
                }
                if ui.button("Clear").clicked() {
                    self.filter.clear();
                    self.projection.clear();
                    self.sort.clear();
                    self.skip.clear();
                    run = true;
                }
            });
            ui.add_space(2.0);
            ui.horizontal_wrapped(|ui| {
                label(ui, "Projection");
                let text_w = |ui: &Ui, t: &str| ui.fonts_mut(|f| f.layout_no_wrap(t.into(), egui::TextStyle::Body.resolve(ui.style()), egui::Color32::WHITE).size().x);
                let fixed = label_w + text_w(ui, "Sort") + text_w(ui, "Skip") + 60.0 + 7.0 * gap;
                let w = ((full - fixed) / 2.0).max(80.0);
                run |= mono(ui, &mut self.projection, "{ name: 1 }", w);
                ui.label("Sort");
                run |= mono(ui, &mut self.sort, "{ _id: -1 }", w);
                ui.label("Skip");
                run |= mono(ui, &mut self.skip, "0", 60.0);
            });
            if let Some(e) = &self.query_error {
                ui.colored_label(theme::ERROR, e);
            }
        });
        if run {
            self.run();
        }
        ui.add_space(4.0);
        let actions = self.view.ui(ui, net);
        for (_, a) in &actions {
            if let ViewAction::SortChanged(sort) = a {
                self.show_sort(sort);
            }
        }
        actions
    }

    /// A column menu sort shows up in the Sort field, on one line. The text
    /// is not rewritten: spaces inside a quoted field name are the name.
    fn show_sort(&mut self, sort: &rongo_core::bson::Document) {
        use rongo_core::format::{Style, to_shell};
        self.sort = if sort.is_empty() { String::new() } else { to_shell(&rongo_core::bson::Bson::Document(sort.clone()), Style::Editable) };
    }
}

// ---------------------------------------------------------------- shell tab

pub struct ShellResult {
    pub label: String,
    pub view: ResultView,
    pub pinned: bool,
}

/// How much printed output a shell tab keeps (the end of it).
const RAW_MAX: usize = 1 << 20;
const RAW_DROPPED: &str = "[earlier output dropped]";

/// What a shell tab's runs printed, as lines: a frame lays out only the
/// lines in view (`line_view`, as the JSON view does), and printing adds
/// only what was printed.
#[derive(Default)]
struct RawOutput {
    lines: TextLines,
    /// ⌘A: all of it is selected, on screen or not.
    all: bool,
}

impl RawOutput {
    fn clear(&mut self) {
        *self = Self::default();
    }

    /// `text` (a line, or several) at the end. Past RAW_MAX bytes only the
    /// last three quarters or so are kept, from a line start (from inside
    /// the last line when that alone is longer), after a note that more
    /// came before: a script printing in a loop can't fill memory. Copying
    /// what is kept is rare (every quarter of RAW_MAX printed).
    fn push(&mut self, text: &str) {
        self.lines.push_text(text);
        let all = self.lines.text();
        if all.len() > RAW_MAX {
            let from = all.ceil_char_boundary(all.len() - RAW_MAX * 3 / 4);
            let cut = all[from..].find('\n').map_or(from, |i| from + i + 1);
            let mut kept = TextLines::default();
            kept.push(RAW_DROPPED);
            kept.push_text(&all[cut..]);
            self.lines = kept;
        }
    }

    /// Only `text`.
    fn set(&mut self, text: &str) {
        self.clear();
        self.push(text);
    }
}

pub struct ShellTab {
    tab_id: u64,
    pub conn: ConnId,
    pub db: String,
    editor: CodeEditor,
    shell: Option<ShellHandle>,
    running: Option<(u64, Instant)>,
    raw: RawOutput,
    pub results: Vec<ShellResult>,
    /// 0 is the raw output; n is `results[n - 1]`.
    active: usize,
    status: Option<Result<u64, ShellError>>,
    file: Option<std::path::PathBuf>,
    /// Run the whole script as soon as the connection is ready (demo tab).
    pub run_when_ready: bool,
    /// Height of the editor area, changed by dragging the handle below it.
    split: f32,
    /// Collections (db.coll) whose fields were already requested.
    requested_fields: std::collections::HashSet<String>,
}

impl ShellTab {
    pub fn new(conn: ConnId, db: String, tab_id: u64, initial: String) -> Self {
        Self {
            tab_id,
            conn,
            db,
            editor: CodeEditor::new(egui::Id::new(("shell-editor", tab_id)), initial),
            shell: None,
            running: None,
            raw: RawOutput::default(),
            results: Vec::new(),
            active: 0,
            status: None,
            file: None,
            run_when_ready: false,
            split: 260.0,
            requested_fields: Default::default(),
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// Whether the editor holds a script (anything but blank space), which
    /// closing the tab would lose.
    pub fn has_script(&self) -> bool {
        !self.editor.text.trim().is_empty()
    }

    /// Whether the script, or its find bar's field, has the keyboard focus:
    /// ⌘F finds in the script then, wherever the pointer is.
    pub fn script_has_keys(&self, ctx: &egui::Context) -> bool {
        self.editor.has_keys(ctx)
    }

    /// Tests: the script's editor.
    #[cfg(test)]
    pub fn script(&self) -> &CodeEditor {
        &self.editor
    }

    /// When the running script started, if one is running.
    pub fn running_since(&self) -> Option<Instant> {
        self.running.map(|(_, t)| t)
    }

    /// Interrupt the running script, as its Stop button does.
    pub fn stop(&self) {
        if self.running.is_some()
            && let Some(s) = &self.shell
        {
            s.interrupt();
        }
    }

    fn start(&mut self, req: RunRequest, session: Option<Session>, waker: &Waker) {
        if self.shell.is_none() {
            match session {
                Some(s) => self.shell = Some(ShellHandle::spawn(s, waker.clone())),
                None => {
                    self.status = Some(Err(ShellError { message: "not connected".into(), line: None }));
                    return;
                }
            }
        }
        if self.shell.is_none() || req.code.trim().is_empty() {
            return;
        }
        self.keep_pinned();
        self.editor.error_line = None;
        self.status = None;
        self.raw.clear();
        let Some(shell) = self.shell.as_mut() else { return };
        let run = shell.run(req.code, self.db.clone(), req.line_offset);
        self.running = Some((run, Instant::now()));
    }

    /// A new run starts: only pinned results stay. The one on screen stays
    /// on screen if it is kept; if not, the raw output shows, where the run
    /// prints. (`active` counts from 1; 0 is the raw output.)
    fn keep_pinned(&mut self) {
        let shown = self.active.checked_sub(1).and_then(|i| self.results.get(i)).map(|r| r.view.id);
        self.results.retain(|r| r.pinned);
        self.active = shown.and_then(|id| self.results.iter().position(|r| r.view.id == id)).map_or(0, |i| i + 1);
    }

    /// The result tab `i` was closed. The one on screen stays on screen;
    /// closing that one shows the next (or the last).
    fn close_result(&mut self, i: usize) {
        self.results.remove(i);
        if i + 1 < self.active {
            self.active -= 1;
        } else if self.active > self.results.len() {
            self.active = self.results.len();
        }
    }

    /// One printed line onto the raw output, which keeps only its last
    /// `RAW_MAX` bytes or so (`RawOutput::push`).
    fn append_raw(&mut self, text: &str) {
        self.raw.push(text);
    }

    /// The raw output: only the lines in view laid out, following the end
    /// while a script prints. With nothing focused, ⌘A selects all of it,
    /// on screen or not, and ⌘C copies that; a click or Esc clears it.
    fn raw_ui(&mut self, ui: &mut Ui, net: &Net, salt: u64) {
        let raw = &mut self.raw;
        if net.keys_enabled && ui.memory(|m| m.focused().is_none()) {
            if ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::A)) {
                raw.all = true;
                // A drag selection would fight it.
                ui.ctx().with_plugin(|s: &mut egui::text_selection::LabelSelectionState| s.clear_selection());
            }
            if raw.all && ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy))) {
                ui.ctx().copy_text(raw.lines.text().to_owned());
            }
            if raw.all && ui.input(|i| i.key_pressed(Key::Escape)) {
                raw.all = false;
            }
        }
        let font = theme::mono(12.5);
        let row_h = ui.fonts_mut(|f| f.row_height(&font)).round();
        let area = egui::ScrollArea::both().id_salt(("raw", salt)).auto_shrink([false, false]).stick_to_bottom(true);
        let out = line_view(ui, area, &raw.lines, &font, row_h, raw.all, false, |_, _| {});
        if raw.all && ui.input(|i| i.pointer.any_pressed()) && ui.input(|i| i.pointer.interact_pos()).is_some_and(|p| out.inner_rect.contains(p)) {
            raw.all = false;
        }
    }

    pub fn poll(&mut self, net: &mut Net) {
        while let Some(ev) = self.shell.as_ref().and_then(ShellHandle::try_recv) {
            match ev {
                ShellEvent::Print { texts, .. } => {
                    for text in texts {
                        self.append_raw(&text);
                    }
                }
                ShellEvent::Result { item, .. } => {
                    let id = net.view_id();
                    let (label, view) = match item {
                        ResultItem::Find { line, spec, max } => {
                            let title = spec.ns.coll.clone();
                            (format!("Find Query (line {line})"), ResultView::new(id, title, Source::Find { conn: self.conn, spec, max }))
                        }
                        ResultItem::Docs { line, title, docs, truncated } => {
                            let label = if line > 0 { format!("{title} (line {line})") } else { title.clone() };
                            (label, ResultView::new(id, title, Source::Static { docs, truncated }))
                        }
                    };
                    self.results.push(ShellResult { label, view, pinned: false });
                    // Show the first new result of this run.
                    let first_new = self.results.iter().position(|r| !r.pinned).map_or(self.results.len(), |p| p + 1);
                    if self.active == 0 || self.active > self.results.len() || self.results[self.active - 1].pinned {
                        self.active = first_new;
                    }
                }
                ShellEvent::Done { run, elapsed_ms, db, error } => {
                    if self.running.is_some_and(|(r, _)| r == run) {
                        self.running = None;
                    }
                    self.db = db;
                    if let Some(e) = &error {
                        self.raw.push(&e.to_string());
                        self.editor.error_line = e.line;
                        self.active = 0;
                    } else if self.results.iter().all(|r| r.pinned) {
                        self.active = 0;
                    }
                    self.status = Some(error.map_or(Ok(elapsed_ms), Err));
                }
            }
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, net: &mut Net, conn: Option<&OpenConn>, session: impl FnOnce() -> Option<Session>, waker: &Waker) -> Vec<(u64, ViewAction)> {
        self.poll(net);
        sidebar::breadcrumb(ui, conn, &self.db);
        ui.add_space(2.0);

        let mut run = self.toolbar(ui, net);
        if let Some(r) = self.editor_pane(ui, conn, net.keys_enabled) {
            run = Some(r);
        }

        // Fetch sample documents once per collection for field suggestions.
        if let Some(coll) = self.editor.wanted_fields() {
            let ns = Namespace::new(&self.db, coll);
            if self.requested_fields.insert(ns.to_string()) {
                net.send(Origin::Fields(self.conn, ns.clone()), Some(self.conn), rongo_core::Command::Find(FindSpec::all(ns, 100)));
            }
        }
        if run.is_none() && self.run_when_ready && conn.is_some_and(|c| c.info().is_some()) {
            self.run_when_ready = false;
            run = Some(self.editor.run_all());
        }
        if let Some(req) = run {
            self.start(req, session(), waker);
        }

        self.result_tabs(ui, net)
    }

    /// The toolbar: Run, Run current statement, Stop, New, Load file, Save
    /// file. Returns the run a button asked for.
    fn toolbar(&mut self, ui: &mut Ui, net: &Net) -> Option<RunRequest> {
        let mut run: Option<RunRequest> = None;
        // Wraps on a narrow tab rather than widening it.
        ui.horizontal_wrapped(|ui| {
            let busy = self.running.is_some();
            if ui.add_enabled(!busy, egui::Button::new(RichText::new(icon::PLAY).color(theme::OK).size(16.0))).on_hover_text(shortcut::label("Run all or selection (⌘↩ / F5)")).clicked() {
                run = Some(self.editor.run_all());
            }
            if ui.add_enabled(!busy, egui::Button::new(RichText::new(icon::SKIP_FORWARD).color(theme::OK).size(16.0))).on_hover_text(shortcut::label("Run current statement (⇧⌘↩)")).clicked() {
                run = Some(self.editor.run_current());
            }
            if ui.add_enabled(busy, egui::Button::new(RichText::new(icon::STOP).color(theme::ERROR).size(16.0))).on_hover_text("Stop").clicked()
                && let Some(s) = &self.shell
            {
                s.interrupt();
            }
            ui.separator();
            if ui.button(format!("{}  New", icon::FILE)).clicked() {
                self.editor.set_text(String::new());
                self.file = None;
            }
            if ui.button(format!("{}  Load file", icon::FOLDER_OPEN)).clicked()
                && let Some(path) = rfd::FileDialog::new().add_filter("JavaScript", &["js", "mongodb"]).pick_file()
            {
                match std::fs::read_to_string(&path) {
                    Ok(text) => {
                        self.editor.set_text(text);
                        self.file = Some(path);
                    }
                    Err(e) => self.raw.set(&format!("could not read {}: {e}", path.display())),
                }
            }
            // ⌘S belongs to a dialog on top (the document editor saves with it).
            let save = ui.button(format!("{}  Save file", icon::FLOPPY_DISK)).clicked()
                || (net.keys_enabled && ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::S)));
            if save {
                let path = self.file.clone().or_else(|| rfd::FileDialog::new().add_filter("JavaScript", &["js"]).set_file_name("script.js").save_file());
                if let Some(path) = path {
                    match std::fs::write(&path, &self.editor.text) {
                        Ok(()) => self.file = Some(path),
                        Err(e) => self.raw.set(&format!("could not save {}: {e}", path.display())),
                    }
                }
            }
            if let Some(f) = &self.file {
                ui.weak(f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
            }
        });
        run
    }

    /// The editor, with its find bar (⌘F) over it, and its footer, and
    /// under them the handle that drags the split. Returns the run the
    /// editor asked for (⌘↩, F5), unless a script is running. `keys`: no
    /// dialog is open.
    fn editor_pane(&mut self, ui: &mut Ui, conn: Option<&OpenConn>, keys: bool) -> Option<RunRequest> {
        let mut run: Option<RunRequest> = None;
        // Editor on top, results below. The split is a stored height that only
        // changes when the user drags the handle: sizing it from the content
        // made it creep a pixel every frame.
        let width = ui.available_width();
        let max_split = (ui.available_height() - 140.0).max(90.0);
        self.split = self.split.clamp(90.0, max_split);
        let (editor_rect, _) = ui.allocate_exact_size(egui::vec2(width, self.split), egui::Sense::hover());
        ui.painter().rect_filled(editor_rect, 0.0, theme::BG);
        {
            let inner = editor_rect.shrink2(egui::vec2(0.0, 4.0));
            let mut ui = ui.new_child(egui::UiBuilder::new().max_rect(inner).layout(Layout::top_down(Align::Min)));
            ui.set_clip_rect(editor_rect);
            let ui = &mut ui;
            {
                let footer_h = 24.0;
                // The find bar's height comes off the script's: the results stay where they are.
                self.editor.find_bar(ui, keys);
                let script_h = (ui.available_height() - footer_h).max(20.0);
                ui.allocate_ui(egui::vec2(width, script_h), |ui| {
                    let db = self.db.clone();
                    // Read when completing, not copied every frame.
                    let collections = || conn.map(|c| c.collection_names(&db)).unwrap_or_default();
                    let fields = |coll: &str| conn.and_then(|c| c.fields.get(&Namespace::new(&db, coll)).cloned());
                    let out = self.editor.ui(ui, &collections, &fields);
                    if out.run.is_some() && self.running.is_none() {
                        run = out.run;
                    }
                });
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    ui.weak("Ctrl+Space: autocomplete");
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add_space(8.0);
                        match &self.status {
                            _ if self.running.is_some() => {
                                let secs = self.running.map_or(0.0, |(_, t)| t.elapsed().as_secs_f32());
                                ui.spinner();
                                ui.label(format!("Running… {secs:.1}s"));
                                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
                            }
                            Some(Ok(ms)) => {
                                ui.colored_label(theme::OK, format!("Done in {ms} ms"));
                            }
                            Some(Err(e)) => {
                                // One line from the left: in this right-to-left
                                // row a long error ran over the hint and lost its start.
                                let w = ui.available_width();
                                theme::clipped_text(ui, &e.to_string(), theme::ERROR, 1, w);
                            }
                            None => {
                                ui.colored_label(theme::OK, "No errors");
                            }
                        }
                        let (l, c) = self.editor.line_col();
                        ui.weak(format!("Lin {l}, Col {c}"));
                    });
                });
            }
        }
        let (handle, drag) = ui.allocate_exact_size(egui::vec2(width, 7.0), egui::Sense::drag());
        if drag.hovered() || drag.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
        }
        if drag.dragged() {
            self.split = (self.split + drag.drag_delta().y).clamp(90.0, max_split);
        }
        let line = if drag.hovered() || drag.dragged() { theme::ACCENT } else { theme::BORDER };
        ui.painter().hline(handle.x_range(), handle.center().y, egui::Stroke::new(1.0, line));
        run
    }

    /// The result tabs (the raw output, then each result, with its pin and
    /// close buttons), and under them the one shown.
    fn result_tabs(&mut self, ui: &mut Ui, net: &mut Net) -> Vec<(u64, ViewAction)> {
        let tab_salt = self.tab_id;
        // Result tabs.
        let mut close = None;
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(&mut self.active, 0, "Raw shell output");
            for (i, r) in self.results.iter_mut().enumerate() {
                ui.selectable_value(&mut self.active, i + 1, &r.label);
                let pin = RichText::new(if r.pinned { icon::PUSH_PIN } else { icon::PUSH_PIN_SIMPLE }).color(if r.pinned { theme::ACCENT } else { theme::MUTED });
                if ui.add(egui::Button::new(pin).frame(false)).on_hover_text("Keep this result when the script runs again").clicked() {
                    r.pinned = !r.pinned;
                }
                if ui.add(egui::Button::new(RichText::new(icon::X).color(theme::MUTED)).frame(false)).clicked() {
                    close = Some(i);
                }
            }
        });
        if let Some(i) = close {
            self.close_result(i);
        }
        ui.separator();
        if self.active == 0 {
            self.raw_ui(ui, net, tab_salt);
            Vec::new()
        } else {
            self.raw.all = false;
            let idx = self.active - 1;
            // In an area of its own: a view claims ⌘F (and a click ends its
            // select-all) for the pointer over its `max_rect`, which would
            // otherwise be the whole tab, the script and its find bar too.
            let area = ui.available_rect_before_wrap();
            ui.scope_builder(egui::UiBuilder::new().max_rect(area), |ui| self.results[idx].view.ui(ui, net)).inner
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::*;

    /// A script printing without end leaves the raw output its last
    /// megabyte or so, starting at a line, with a note that more came before;
    /// its lines are found where they are, after every trim.
    #[test]
    fn raw_output_keeps_only_its_end() {
        let mut tab = ShellTab::new(ConnId(uuid::Uuid::nil()), "test".into(), 7, String::new());
        let lines_hold_up = |tab: &ShellTab| {
            let raw = &tab.raw.lines;
            assert!((0..raw.len()).map(|i| raw.line(i)).eq(raw.text().split('\n')), "the lines are where they are in the text");
        };
        for i in 0..200_000 {
            tab.append_raw(&format!("line {i} é"));
            if i % 50_000 == 49_999 {
                lines_hold_up(&tab);
            }
        }
        let text = |tab: &ShellTab| tab.raw.lines.text().to_owned();
        assert!(text(&tab).len() <= RAW_MAX, "{} bytes kept", text(&tab).len());
        assert!(text(&tab).len() > RAW_MAX / 2, "{} bytes kept", text(&tab).len());
        assert!(text(&tab).starts_with(&format!("{RAW_DROPPED}\nline ")), "{:?}", &text(&tab)[..60]);
        assert!(text(&tab).ends_with("\nline 199999 é"));
        // A printed text of several lines adds them all.
        tab.append_raw("{\n  \"a\" : 1\n}");
        assert!(text(&tab).ends_with("line 199999 é\n{\n  \"a\" : 1\n}"));
        lines_hold_up(&tab);
        // One line longer than what is kept: its end stays.
        let huge = format!("{}the end", "é".repeat(600_000));
        tab.append_raw(&huge);
        assert!(text(&tab).len() <= RAW_MAX, "{} bytes kept", text(&tab).len());
        assert!(text(&tab).starts_with(&format!("{RAW_DROPPED}\n")) && text(&tab).ends_with("éthe end"), "{} bytes kept", text(&tab).len());
        assert!(text(&tab).len() > RAW_MAX / 2, "{} bytes kept", text(&tab).len());
        assert_eq!(tab.raw.lines.len(), 2);
        lines_hold_up(&tab);
    }

    /// One frame of a shell tab, 1200×800: the texts drawn, and what was
    /// copied.
    fn shell_frame(ctx: &egui::Context, tab: &mut ShellTab, events: Vec<egui::Event>) -> (Vec<Arc<egui::Galley>>, Option<String>) {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let core = rongo_core::CoreHandle::start(rt.handle().clone(), Arc::new(|| {}));
        let (mut pending, mut next_view) = (HashMap::new(), 0);
        let waker: Waker = Arc::new(|| {});
        let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 800.0))), events, ..Default::default() };
        let mut out = ctx.run_ui(input, |ui| {
            let mut net = Net { core: &core, pending: &mut pending, next_view_id: &mut next_view, keys_enabled: true };
            tab.ui(ui, &mut net, None, || None, &waker);
        });
        out.textures_delta.clear();
        let galleys = out.shapes.iter().filter_map(|s| if let egui::Shape::Text(t) = &s.shape { Some(t.galley.clone()) } else { None }).collect();
        let copied = out.platform_output.commands.into_iter().find_map(|c| if let egui::OutputCommand::CopyText(t) = c { Some(t) } else { None });
        (galleys, copied)
    }

    /// 20,000 printed lines: a frame lays out only those in view, each on
    /// its own, and shows the end, where a running script prints.
    #[test]
    fn raw_output_lays_out_only_the_lines_in_view() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut tab = ShellTab::new(ConnId(uuid::Uuid::nil()), "test".into(), 7, "print(1)".into());
        for i in 0..20_000 {
            tab.append_raw(&format!("printed line {i}"));
        }
        shell_frame(&ctx, &mut tab, vec![]);
        let (galleys, _) = shell_frame(&ctx, &mut tab, vec![]);
        let printed: Vec<_> = galleys.iter().filter(|g| g.text().contains("printed line")).collect();
        assert!(printed.iter().all(|g| g.rows.len() == 1), "a galley a line, not {:?} rows", printed.iter().map(|g| g.rows.len()).max());
        assert!(!printed.is_empty() && printed.len() < 100, "{} lines laid out", printed.len());
        assert!(printed.iter().any(|g| g.text() == "printed line 19999"), "the end is in view");
    }

    /// With nothing focused, ⌘A selects all of the raw output and ⌘C copies
    /// it, lines off screen too; a click clears it.
    #[test]
    fn command_a_copies_all_of_the_raw_output() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut tab = ShellTab::new(ConnId(uuid::Uuid::nil()), "test".into(), 7, "print(1)".into());
        let printed: Vec<String> = (0..5_000).map(|i| format!("printed line {i} สวัสดี")).collect();
        for p in &printed {
            tab.append_raw(p);
        }
        shell_frame(&ctx, &mut tab, vec![]);
        let cmd_a = egui::Event::Key { key: Key::A, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::COMMAND };
        shell_frame(&ctx, &mut tab, vec![cmd_a]);
        let (_, copied) = shell_frame(&ctx, &mut tab, vec![egui::Event::Copy]);
        assert_eq!(copied.as_deref(), Some(printed.join("\n").as_str()), "all of it");
        let at = egui::pos2(600.0, 700.0);
        let click = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        shell_frame(&ctx, &mut tab, vec![egui::Event::PointerMoved(at), click(true)]);
        shell_frame(&ctx, &mut tab, vec![click(false)]);
        let (_, copied) = shell_frame(&ctx, &mut tab, vec![egui::Event::Copy]);
        assert_ne!(copied.as_deref(), Some(printed.join("\n").as_str()), "a click cleared it");
    }
    /// Frame after frame with no input, the editor/results split must not move.
    /// (It used to creep by a pixel per frame.)
    #[test]
    fn shell_split_is_stable_without_input() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let core = rongo_core::CoreHandle::start(rt.handle().clone(), Arc::new(|| {}));
        let mut pending = HashMap::new();
        let mut next_view = 0;
        let waker: Waker = Arc::new(|| {});
        let mut tab = ShellTab::new(ConnId(uuid::Uuid::nil()), "test".into(), 7, "db.x.find()".into());
        let ctx = egui::Context::default();
        let input = || egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 800.0))),
            ..Default::default()
        };
        let mut heights = Vec::new();
        for _ in 0..30 {
            let mut out = ctx.run_ui(input(), |ui| {
                let mut net = Net { core: &core, pending: &mut pending, next_view_id: &mut next_view, keys_enabled: true };
                tab.ui(ui, &mut net, None, || None, &waker);
            });
            out.textures_delta.clear();
            // The editor as laid out on screen, not just the stored number.
            let editor = ctx.read_response(egui::Id::new(("shell-editor", 7u64))).expect("editor drawn");
            heights.push(editor.rect.height());
        }
        let settled = &heights[3..];
        assert!(settled.windows(2).all(|w| (w[0] - w[1]).abs() < 0.5), "split moved: {heights:?}");
    }

    /// The query field takes the tab's width (queries can be long), and on
    /// a narrow tab Run and Clear wrap rather than leave the screen.
    #[test]
    fn query_bar_uses_the_width_and_keeps_its_buttons() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let core = rongo_core::CoreHandle::start(rt.handle().clone(), Arc::new(|| {}));
        let mut pending = HashMap::new();
        let mut next_view = 0;
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut tab = CollectionTab::new(ConnId(uuid::Uuid::nil()), Namespace::new("db", "c"), 3);
        let mut at = |width: f32| {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 600.0))), ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| {
                let mut net = Net { core: &core, pending: &mut pending, next_view_id: &mut next_view, keys_enabled: true };
                tab.ui(ui, &mut net, None);
            });
            out.textures_delta.clear();
            let find = |t: &str| out.shapes.iter().find_map(|s| match &s.shape {
                egui::Shape::Text(g) if g.galley.text().ends_with(t) => Some(g.galley.rect.translate(g.pos.to_vec2())),
                _ => None,
            });
            (find("Run").expect("Run"), find("Clear").expect("Clear"), find("{ status: \"active\" }").expect("query hint"))
        };
        at(1400.0);
        let (run, clear, hint) = at(1400.0);
        assert!(run.left() > 1100.0, "the field pushes Run to the right edge: {run:?}");
        assert!(clear.right() <= 1400.0 && hint.left() < 200.0);
        at(520.0);
        let (run, clear, _) = at(520.0);
        assert!(run.right() <= 520.0 && clear.right() <= 520.0, "buttons stay on screen: {run:?} {clear:?}");
    }

    /// Column › Sort Collection writes its sort into the Sort field, and
    /// the next Run sorts by the same field, spaces in its name and all.
    #[test]
    fn sort_collection_keeps_the_field_name() {
        let mut tab = CollectionTab::new(ConnId(uuid::Uuid::nil()), Namespace::new("db", "c"), 3);
        let sort = rongo_core::bson::doc! { "a  b": 1, "c": -1 };
        tab.show_sort(&sort);
        assert!(!tab.sort.contains('\n'), "one line: {:?}", tab.sort);
        tab.run();
        assert_eq!(tab.query_error, None);
        let Source::Find { spec, .. } = &tab.view.source else { unreachable!() };
        assert_eq!(spec.sort, Some(sort), "Sort field: {:?}", tab.sort);
        tab.show_sort(&rongo_core::bson::Document::new());
        assert_eq!(tab.sort, "");
    }

    /// With a dialog open over the shell, ⌘S is left for the dialog (the
    /// document editor saves with it) instead of saving the script.
    #[test]
    fn command_s_is_left_for_a_dialog() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let core = rongo_core::CoreHandle::start(rt.handle().clone(), Arc::new(|| {}));
        let mut pending = HashMap::new();
        let mut next_view = 0;
        let waker: Waker = Arc::new(|| {});
        let mut tab = ShellTab::new(ConnId(uuid::Uuid::nil()), "test".into(), 7, "db.x.find()".into());
        let ctx = egui::Context::default();
        let m = Modifiers::COMMAND;
        let events = vec![egui::Event::ModifiersChanged(m), egui::Event::Key { key: Key::S, physical_key: None, pressed: true, repeat: false, modifiers: m }];
        let mut left = false;
        let mut out = ctx.run_ui(egui::RawInput { events, ..Default::default() }, |ui| {
            let mut net = Net { core: &core, pending: &mut pending, next_view_id: &mut next_view, keys_enabled: false };
            tab.ui(ui, &mut net, None, || None, &waker);
            left = ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::S));
        });
        out.textures_delta.clear();
        assert!(left, "the dialog still gets ⌘S");
    }

    /// A long script error in the editor's status row is one line that
    /// keeps its start ("line 4: …") and does not run over the hint.
    #[test]
    fn long_shell_error_keeps_its_start() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let core = rongo_core::CoreHandle::start(rt.handle().clone(), Arc::new(|| {}));
        let mut pending = HashMap::new();
        let mut next_view = 0;
        let waker: Waker = Arc::new(|| {});
        let mut tab = ShellTab::new(ConnId(uuid::Uuid::nil()), "test".into(), 7, "db.x.find()".into());
        tab.status = Some(Err(rongo_shell::ShellError { message: crate::test_support::long_driver_error(), line: Some(4) }));
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let width = 1000.0;
        let input = || egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 700.0))), ..Default::default() };
        let mut texts = Vec::new();
        for _ in 0..3 {
            let mut out = ctx.run_ui(input(), |ui| {
                let mut net = Net { core: &core, pending: &mut pending, next_view_id: &mut next_view, keys_enabled: false };
                tab.ui(ui, &mut net, None, || None, &waker);
            });
            out.textures_delta.clear();
            texts = out.shapes.iter().filter_map(|s| match &s.shape {
                egui::Shape::Text(t) => Some((t.galley.clone(), t.galley.rect.translate(t.pos.to_vec2()))),
                _ => None,
            }).collect();
        }
        let [hint, error] = ["Ctrl+Space", "line 4: timed out"].map(|start| {
            texts.iter().find(|(g, _)| g.text().starts_with(start)).unwrap_or_else(|| panic!("{start:?} drawn"))
        });
        assert_eq!(error.0.rows.len(), 1);
        assert!(error.0.elided);
        assert!(error.1.left() >= hint.1.right(), "the error {:?} runs over the hint {:?}", error.1, hint.1);
        assert!(error.1.right() <= width, "{:?}", error.1);
    }

    /// The result on screen stays on screen when other result tabs go:
    /// one closed to its left, or unpinned ones dropped as a run starts.
    #[test]
    fn the_shown_result_stays_when_others_close() {
        let mut tab = ShellTab::new(ConnId(uuid::Uuid::nil()), "test".into(), 7, String::new());
        let ids = std::cell::Cell::new(0);
        let result = |label: &str, pinned: bool| {
            ids.set(ids.get() + 1);
            ShellResult { label: label.into(), view: ResultView::new(ids.get(), label, Source::Static { docs: Vec::new(), truncated: false }), pinned }
        };
        let shown = |tab: &ShellTab| if tab.active == 0 { "raw".to_string() } else { tab.results[tab.active - 1].label.clone() };
        tab.results = vec![result("A", false), result("B", false), result("C", false)];
        tab.active = 2;
        tab.close_result(0);
        assert_eq!(shown(&tab), "B", "closing a tab to its left");
        tab.close_result(1);
        assert_eq!(shown(&tab), "B", "closing a tab to its right");
        tab.close_result(0);
        assert_eq!(shown(&tab), "raw", "closing it: the one before");

        tab.results = vec![result("A", false), result("B", true), result("C", true)];
        tab.active = 2;
        tab.keep_pinned();
        assert_eq!(shown(&tab), "B", "a run drops the unpinned ones");
        tab.active = 0;
        tab.keep_pinned();
        assert_eq!(shown(&tab), "raw");
        tab.results.push(result("D", false));
        tab.active = 3;
        tab.keep_pinned();
        assert_eq!(shown(&tab), "raw", "the shown one dropped: the raw output, where the run prints");
    }

    /// What the raw output costs while a script prints 200,000 lines (2,000
    /// a frame), and a frame once it is done, with about 1 MiB kept. Run with
    /// `cargo test -p rongo --release -- --ignored --nocapture report_shell_output_frame_time`.
    #[test]
    #[ignore = "timing; run explicitly in release mode"]
    fn report_shell_output_frame_time() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let core = rongo_core::CoreHandle::start(rt.handle().clone(), Arc::new(|| {}));
        let mut pending = HashMap::new();
        let mut next_view = 0;
        let waker: Waker = Arc::new(|| {});
        let mut tab = ShellTab::new(ConnId(uuid::Uuid::nil()), "test".into(), 7, "db.x.find()".into());
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut frame = |tab: &mut ShellTab| {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0))), ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| {
                let mut net = Net { core: &core, pending: &mut pending, next_view_id: &mut next_view, keys_enabled: true };
                tab.ui(ui, &mut net, None, || None, &waker);
            });
            out.textures_delta.clear();
        };
        frame(&mut tab);
        frame(&mut tab);
        let (mut appending, mut drawing, mut slowest) = (0.0, 0.0, 0.0f64);
        for f in 0..100 {
            let t = Instant::now();
            for i in 0..2_000 {
                tab.append_raw(&format!("{{ \"_id\" : {}, \"name\" : \"สล็อตมังกรทองคำ {i}\", \"rtp\" : 96.5 }}", f * 2_000 + i));
            }
            appending += t.elapsed().as_secs_f64() * 1000.0;
            let t = Instant::now();
            frame(&mut tab);
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            drawing += ms;
            slowest = slowest.max(ms);
        }
        let idle = (0..5)
            .map(|_| {
                let t = Instant::now();
                for _ in 0..10 {
                    frame(&mut tab);
                }
                t.elapsed().as_secs_f64() * 100.0
            })
            .fold(f64::INFINITY, f64::min);
        eprintln!("raw output, 200,000 lines printed 2,000 a frame: appending {appending:.1} ms in all, frames {drawing:.1} ms in all (slowest {slowest:.1} ms); then {idle:.2} ms/frame idle");
    }

    /// A shell tab in a harness, 1200×800, drawn as the app draws it, with
    /// no connection; `keys` off is a dialog open over the window.
    struct ShellProbe {
        tab: ShellTab,
        core: rongo_core::CoreHandle,
        pending: HashMap<rongo_core::RequestId, crate::net::Sent>,
        next_view: u64,
        waker: Waker,
        keys: bool,
        _rt: tokio::runtime::Runtime,
    }

    fn shell_harness(script: &str) -> egui_kittest::Harness<'static, ShellProbe> {
        shell_harness_sized(script, egui::vec2(1200.0, 800.0))
    }

    fn shell_harness_sized(script: &str, size: egui::Vec2) -> egui_kittest::Harness<'static, ShellProbe> {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let core = rongo_core::CoreHandle::start(rt.handle().clone(), Arc::new(|| {}));
        let tab = ShellTab::new(ConnId(uuid::Uuid::nil()), "test".into(), 7, script.into());
        let probe = ShellProbe { tab, core, pending: HashMap::new(), next_view: 0, waker: Arc::new(|| {}), keys: true, _rt: rt };
        let mut h = egui_kittest::Harness::builder().with_size(size).build_ui_state(
            |ui, p: &mut ShellProbe| {
                crate::theme::install(ui.ctx());
                let mut net = Net { core: &p.core, pending: &mut p.pending, next_view_id: &mut p.next_view, keys_enabled: p.keys };
                p.tab.ui(ui, &mut net, None, || None, &p.waker);
            },
            probe,
        );
        h.run_steps(3);
        h
    }

    /// Three matches of "apple" ignoring case, two matching it.
    const APPLES: &str = "db.apple.find({ name: \"Apple pie\" })\nprint(\"pineapple\")\n";

    /// The script editor's id (the tab's id is 7).
    fn script_id() -> egui::Id {
        egui::Id::new(("shell-editor", 7u64))
    }

    /// Focuses the script with its caret just after the first `after` in
    /// it ("" for the start).
    fn caret_after(h: &mut egui_kittest::Harness<'_, ShellProbe>, after: &str) {
        use egui_kittest::kittest::Queryable;
        h.get_by_role(egui::accesskit::Role::MultilineTextInput).focus();
        h.run_steps(2);
        let text = h.state().tab.editor.text.clone();
        let at = text.find(after).unwrap_or_else(|| panic!("{after:?} in {text}")) + after.len();
        let mut state = egui::TextEdit::load_state(&h.ctx, script_id()).expect("the script's state");
        state.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(text[..at].chars().count()))));
        state.store(&h.ctx, script_id());
        h.run_steps(2);
    }

    /// The texts the last frame drew.
    fn drawn(h: &egui_kittest::Harness<'_, ShellProbe>) -> Vec<String> {
        crate::test_support::flat_shapes(h).into_iter().filter_map(|(_, s)| if let egui::Shape::Text(t) = s { Some(t.galley.text().to_string()) } else { None }).collect()
    }

    /// ⌘F with the script focused opens a find bar under the toolbar,
    /// above the script, its field focused ("Find in this script"); not
    /// with the script unfocused (the app's) or a dialog open. Typing
    /// finds: "1 of 3", every match marked behind the script and the first
    /// the current one; ↩ / ⇧↩ step round, the field keeping its focus;
    /// Aa matches case; ⌘F again selects what was typed.
    #[test]
    fn cmd_f_finds_in_the_script() {
        use crate::test_support::{find_count, find_marks, press, type_in};
        use egui::Modifiers as M;
        use egui::accesskit::Role;
        use egui_kittest::kittest::Queryable;
        let mut h = shell_harness(APPLES);
        press(&mut h, M::COMMAND, Key::F);
        assert!(h.query_by_role(Role::TextInput).is_none(), "⌘F with the script not focused is left to the app");
        caret_after(&mut h, "");
        h.state_mut().keys = false;
        press(&mut h, M::COMMAND, Key::F);
        assert!(h.query_by_role(Role::TextInput).is_none(), "nor with a dialog open");
        h.state_mut().keys = true;
        caret_after(&mut h, "");
        press(&mut h, M::COMMAND, Key::F);
        assert!(h.get_by_role(Role::TextInput).is_focused(), "⌘F focused the find field");
        assert!(drawn(&h).iter().any(|t| t == "Find in this script"), "the field's hint: {:?}", drawn(&h));
        let field = h.get_by_role(Role::TextInput).rect();
        let new = h.get_by_label(&format!("{}  New", icon::FILE)).rect();
        let script = h.ctx.read_response(script_id()).expect("the script drawn").rect;
        assert!(new.bottom() <= field.top() && field.bottom() <= script.top(), "under the toolbar {new:?}, above the script {script:?}: {field:?}");
        assert_eq!(find_count(&h), None, "nothing typed, nothing said");
        type_in(&mut h, "apple");
        assert_eq!(find_count(&h).as_deref(), Some("1 of 3"));
        let (others, current) = find_marks(&h);
        assert_eq!((others.len(), current.len()), (2, 1), "every match marked, the current one apart");
        let c = current[0].0;
        assert!(others.iter().all(|(o, _)| o.top() > c.top() || (o.top() == c.top() && o.left() > c.right())), "the first match is the current one");
        assert!(others.iter().chain(&current).all(|(r, _)| r.top() >= script.top()), "the marks are on the script");
        for (modifiers, want) in [(M::NONE, "2 of 3"), (M::NONE, "3 of 3"), (M::NONE, "1 of 3"), (M::SHIFT, "3 of 3")] {
            press(&mut h, modifiers, Key::Enter);
            assert_eq!(find_count(&h).as_deref(), Some(want), "after {modifiers:?} ↩");
            assert!(h.get_by_role(Role::TextInput).is_focused(), "the field kept its focus");
        }
        assert_eq!(h.state().tab.editor.text, APPLES, "↩ went to the field, not the script");
        h.get_by_label("Aa").click();
        h.run_steps(2);
        assert_eq!(find_count(&h).as_deref(), Some("1 of 2"), "matching case leaves \"Apple\" out");
        // A click takes the focus: the bar gives it back to its field, so ⌘F, ↩ and Esc stay the bar's.
        assert!(h.get_by_role(Role::TextInput).is_focused(), "Aa left the keys with the field");
        h.get_by_label(icon::CARET_DOWN).click();
        h.run_steps(2);
        assert_eq!(find_count(&h).as_deref(), Some("2 of 2"));
        assert!(h.get_by_role(Role::TextInput).is_focused(), "↓ left the keys with the field");
        press(&mut h, M::COMMAND, Key::F);
        assert!(h.get_by_role(Role::TextInput).is_focused());
        type_in(&mut h, "pear");
        assert_eq!(h.get_by_role(Role::TextInput).value().as_deref(), Some("pear"), "⌘F selected what was typed");
        assert_eq!(find_count(&h).as_deref(), Some("No matches"));
        assert_eq!(find_marks(&h), (vec![], vec![]));
    }

    /// Esc in the find field (or ✕) closes the bar only and gives the
    /// script its focus with the current match selected: typing replaces
    /// it. Esc pressed while typing in the script closes the bar too and
    /// leaves the caret where it was.
    #[test]
    fn esc_closes_the_script_find_bar() {
        use crate::test_support::{find_count, press, type_in};
        use egui::Modifiers as M;
        use egui::accesskit::Role;
        use egui_kittest::kittest::Queryable;
        let mut h = shell_harness(APPLES);
        caret_after(&mut h, "");
        press(&mut h, M::COMMAND, Key::F);
        type_in(&mut h, "apple");
        press(&mut h, M::NONE, Key::Enter);
        assert_eq!(find_count(&h).as_deref(), Some("2 of 3"));
        press(&mut h, M::NONE, Key::Escape);
        assert!(h.query_by_role(Role::TextInput).is_none(), "Esc closed the find bar");
        assert!(h.get_by_role(Role::MultilineTextInput).is_focused(), "the script has the focus");
        type_in(&mut h, "Cherry");
        assert_eq!(h.state().tab.editor.text, APPLES.replace("Apple", "Cherry"), "typing replaced the current match");

        // Esc while typing in the script: the caret stays.
        let mut h = shell_harness(APPLES);
        caret_after(&mut h, "");
        press(&mut h, M::COMMAND, Key::F);
        type_in(&mut h, "apple");
        caret_after(&mut h, "print(");
        type_in(&mut h, "1, ");
        press(&mut h, M::NONE, Key::Escape);
        assert!(h.query_by_role(Role::TextInput).is_none(), "Esc closed the find bar");
        assert!(h.get_by_role(Role::MultilineTextInput).is_focused(), "the script kept the focus");
        type_in(&mut h, "2, ");
        assert!(h.state().tab.editor.text.contains("print(1, 2, \"pineapple\")"), "the keystroke went to the caret: {}", h.state().tab.editor.text);

        caret_after(&mut h, "");
        press(&mut h, M::COMMAND, Key::F);
        h.get_by_label(egui_phosphor::regular::X).click();
        h.run_steps(2);
        assert!(h.query_by_role(Role::TextInput).is_none(), "✕ closed the bar");
    }

    /// Editing the script with the bar open finds again, so the count
    /// follows it; so does New (and Load file) replacing it. A search runs
    /// only when the text, the needle or match case changes.
    #[test]
    fn script_find_follows_the_text() {
        use crate::test_support::{find_count, find_marks, press, type_in};
        use egui::Modifiers as M;
        use egui_kittest::kittest::Queryable;
        let searches = |h: &egui_kittest::Harness<'_, ShellProbe>| h.state().tab.editor.find().map_or(0, |f| f.searches);
        let mut h = shell_harness(APPLES);
        caret_after(&mut h, "");
        press(&mut h, M::COMMAND, Key::F);
        type_in(&mut h, "apple");
        assert_eq!(find_count(&h).as_deref(), Some("1 of 3"));
        let n = searches(&h);
        assert!(n > 0);
        h.run_steps(5);
        assert_eq!(searches(&h), n, "nothing changed, nothing searched");
        caret_after(&mut h, "pineapple\")");
        assert_eq!(searches(&h), n, "the script's focus is no change");
        type_in(&mut h, " // apple");
        assert_eq!(find_count(&h).as_deref(), Some("1 of 4"), "the count follows the script");
        assert_eq!(searches(&h), n + 1, "one search for the edit");
        assert_eq!(find_marks(&h).0.len(), 3);
        h.get_by_label("Aa").click();
        h.run_steps(2);
        assert_eq!(searches(&h), n + 2, "one for match case");
        assert_eq!(find_count(&h).as_deref(), Some("1 of 3"));
        h.state_mut().tab.editor.set_text("apple\napple".into());
        h.run_steps(2);
        assert_eq!(find_count(&h).as_deref(), Some("1 of 2"), "Load file: the count follows");
        h.get_by_label(&format!("{}  New", icon::FILE)).click();
        h.run_steps(2);
        assert_eq!(find_count(&h).as_deref(), Some("No matches"), "New: the count follows");
    }

    /// With the bar open, autocomplete works in the script as before:
    /// typing opens it, ↩ accepts. Esc with it open closes it, not the bar,
    /// and the script keeps the focus: the next Esc closes the bar.
    #[test]
    fn autocomplete_works_with_the_find_bar_open() {
        use crate::test_support::{find_count, press, type_in};
        use egui::Modifiers as M;
        use egui::accesskit::Role;
        use egui_kittest::kittest::Queryable;
        let mut h = shell_harness(APPLES);
        caret_after(&mut h, "");
        press(&mut h, M::COMMAND, Key::F);
        type_in(&mut h, "getName");
        assert_eq!(find_count(&h).as_deref(), Some("No matches"));
        caret_after(&mut h, "pineapple\")\n");
        type_in(&mut h, "db.getNa");
        assert!(h.state().tab.editor.completing(), "suggestions opened");
        press(&mut h, M::NONE, Key::Enter);
        assert!(!h.state().tab.editor.completing());
        assert!(h.state().tab.editor.text.ends_with("\ndb.getName"), "↩ accepted the suggestion: {:?}", h.state().tab.editor.text);
        assert_eq!(find_count(&h).as_deref(), Some("1 of 1"), "the accepted text is found");
        type_in(&mut h, "\ndb.getNa");
        assert!(h.state().tab.editor.completing(), "suggestions opened");
        press(&mut h, M::NONE, Key::Escape);
        assert!(!h.state().tab.editor.completing(), "Esc closed the suggestions");
        assert!(h.query_by_role(Role::TextInput).is_some(), "and not the find bar");
        assert!(h.get_by_role(Role::MultilineTextInput).is_focused(), "the script kept the focus");
        press(&mut h, M::NONE, Key::Escape);
        assert!(h.query_by_role(Role::TextInput).is_none(), "the next Esc closed the find bar");
        assert!(h.get_by_role(Role::MultilineTextInput).is_focused(), "the script still has the focus");
    }

    /// A script that replaces another (Load file, New) is found from its
    /// first match, wherever the old one was scrolled to.
    #[test]
    fn a_replaced_script_is_found_from_its_first_match() {
        use crate::test_support::{find_count, press, type_in};
        use egui::Modifiers as M;
        let mut script = String::from("print(\"zebra\")\n");
        for i in 0..400 {
            script.push_str(&format!("print({i})\n"));
        }
        script.push_str("print(\"zebra\")\n");
        let mut h = shell_harness(&script);
        caret_after(&mut h, "");
        press(&mut h, M::COMMAND, Key::F);
        type_in(&mut h, "zebra");
        press(&mut h, M::NONE, Key::Enter);
        h.run_steps(3);
        assert_eq!(find_count(&h).as_deref(), Some("2 of 2"), "scrolled to the end");
        h.state_mut().tab.editor.set_text("zebra\n".repeat(1000));
        h.run_steps(3);
        assert_eq!(find_count(&h).as_deref(), Some("1 of 1,000"), "Load file starts at the first match");
    }

    /// With the bar open the script keeps its colours and its error line's
    /// mark: the matches are marked apart, behind the text.
    #[test]
    fn the_script_keeps_its_colours_and_error_mark_with_the_find_bar_open() {
        use crate::test_support::{find_count, find_marks, flat_shapes, press, type_in};
        use egui::Modifiers as M;
        let mut h = shell_harness(APPLES);
        h.state_mut().tab.editor.error_line = Some(2);
        caret_after(&mut h, "");
        press(&mut h, M::COMMAND, Key::F);
        type_in(&mut h, "apple");
        assert_eq!(find_count(&h).as_deref(), Some("1 of 3"));
        assert_eq!(find_marks(&h).0.len(), 2, "the matches are marked");
        let shapes = flat_shapes(&h);
        let script = h.ctx.read_response(script_id()).expect("the script drawn").rect;
        let line_2 = shapes.iter().find_map(|(_, s)| match s {
            egui::Shape::Rect(r) if r.fill == crate::theme::ERROR.gamma_multiply(0.18) => Some(r.rect),
            _ => None,
        });
        let line_2 = line_2.expect("the error line is still marked");
        assert!(line_2.top() > script.top() + 5.0, "on the second line: {line_2:?}, the script at {script:?}");
        let colours: std::collections::HashSet<_> = shapes
            .iter()
            .find_map(|(_, s)| match s {
                egui::Shape::Text(t) if t.galley.text() == APPLES => Some(t.galley.job.sections.iter().map(|s| s.format.color).collect()),
                _ => None,
            })
            .expect("the script drawn");
        assert!(colours.contains(&crate::theme::STRING) && colours.contains(&crate::theme::FUNCTION), "the script keeps its colours: {colours:?}");
    }

    /// In a narrow tab the find bar fits: its field gives up width, and
    /// nothing under it moves (the footer stays where it was, ✕ is on
    /// screen). At 480 points it is one row; at 320 (about the narrowest a
    /// shell tab gets) ✕ may go to a second.
    #[test]
    fn the_find_bar_fits_a_narrow_tab() {
        use crate::test_support::{flat_shapes, press, type_in};
        use egui::Modifiers as M;
        use egui_kittest::kittest::Queryable;
        for width in [480.0, 320.0] {
            let mut h = shell_harness_sized(APPLES, egui::vec2(width, 700.0));
            caret_after(&mut h, "");
            let footer = |h: &egui_kittest::Harness<'_, ShellProbe>| {
                flat_shapes(h).into_iter().find_map(|(_, s)| match s {
                    egui::Shape::Text(t) if t.galley.text().starts_with("Lin ") => Some(t.galley.rect.translate(t.pos.to_vec2())),
                    _ => None,
                })
            };
            let before = footer(&h).expect("the footer drawn");
            press(&mut h, M::COMMAND, Key::F);
            type_in(&mut h, "apple");
            let after = footer(&h).expect("the footer drawn");
            assert!((after.right() - before.right()).abs() < 0.5, "{width}: the footer moved from {before:?} to {after:?}");
            let close = h.get_by_label(egui_phosphor::regular::X).rect();
            assert!(close.right() <= width, "{width}: ✕ off screen at {close:?}");
            let field = h.get_by_role(egui::accesskit::Role::TextInput).rect();
            assert!(field.width() < 280.0, "{width}: the field gave up width: {field:?}");
            if width >= 480.0 {
                // "No matches" is longer than "1 of 3": still one row.
                press(&mut h, M::COMMAND, Key::A);
                type_in(&mut h, "pear");
                let close = h.get_by_label(egui_phosphor::regular::X).rect();
                let field = h.get_by_role(egui::accesskit::Role::TextInput).rect();
                assert!((close.center().y - field.center().y).abs() < 4.0, "{width}: one row: ✕ at {close:?}, the field at {field:?}");
                assert!((footer(&h).expect("the footer drawn").right() - before.right()).abs() < 0.5, "{width}: the footer moved");
            }
        }
    }

    /// The current match is scrolled into view: in a long script, stepping
    /// to a match at the end and round to the start shows each.
    #[test]
    fn the_current_match_is_scrolled_into_the_script() {
        use crate::test_support::{find_count, find_marks, press, type_in};
        use egui::Modifiers as M;
        let mut script = String::from("print(\"zebra\")\n");
        for i in 0..400 {
            script.push_str(&format!("print({i})\n"));
        }
        script.push_str("print(\"zebra\")\n");
        let mut h = shell_harness(&script);
        caret_after(&mut h, "");
        press(&mut h, M::COMMAND, Key::F);
        type_in(&mut h, "zebra");
        for (step, want) in [(false, "1 of 2"), (true, "2 of 2"), (true, "1 of 2")] {
            if step {
                press(&mut h, M::NONE, Key::Enter);
            }
            h.run_steps(3);
            assert_eq!(find_count(&h).as_deref(), Some(want));
            let (_, current) = find_marks(&h);
            let [(rect, clip)] = current[..] else { panic!("{want}: the current match is on screen: {current:?}") };
            assert!(clip.contains_rect(rect), "{want}: {rect:?} in view ({clip:?})");
        }
    }
}
