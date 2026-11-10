//! Main-area tabs: collection tabs and IntelliShell tabs.

use std::time::Instant;

use egui::{Align, Key, Layout, Modifiers, RichText, Ui};
use egui_phosphor::regular as icon;
use rongo_core::{ConnId, FindSpec, Namespace, Session, Waker};
use rongo_shell::{ResultItem, ShellError, ShellEvent, ShellHandle};

use crate::editor::{CodeEditor, RunRequest};
use crate::net::{Net, Origin};
use crate::result_view::{ResultView, Source, ViewAction};
use crate::sidebar::{self, OpenConn};
use crate::theme;

pub enum TabKind {
    Welcome,
    Collection(Box<CollectionTab>),
    Shell(Box<ShellTab>),
    /// A command reply (statistics, server info) shown as a document.
    Info { conn: ConnId, view: Box<ResultView> },
    Indexes(Box<crate::indexes::IndexTab>),
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
        }
    }

    pub fn conn(&self) -> Option<ConnId> {
        match &self.kind {
            TabKind::Welcome => None,
            TabKind::Collection(c) => Some(c.conn),
            TabKind::Shell(s) => Some(s.conn),
            TabKind::Info { conn, .. } => Some(*conn),
            TabKind::Indexes(t) => Some(t.conn),
        }
    }

    pub fn views_mut(&mut self) -> Vec<&mut ResultView> {
        match &mut self.kind {
            TabKind::Welcome => Vec::new(),
            TabKind::Collection(c) => vec![&mut c.view],
            TabKind::Shell(s) => s.results.iter_mut().map(|r| &mut r.view).collect(),
            TabKind::Info { view, .. } => vec![view.as_mut()],
            TabKind::Indexes(_) => Vec::new(),
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
            egui::Grid::new(("query", self.view.id)).num_columns(4).spacing(egui::vec2(8.0, 6.0)).show(ui, |ui| {
                let mono = |ui: &mut Ui, text: &mut String, hint: &str, w: f32| {
                    let r = ui.add(egui::TextEdit::singleline(text).font(theme::mono(13.0)).hint_text(hint).desired_width(w));
                    r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter))
                };
                let wide = (ui.available_width() - 170.0).max(240.0);
                ui.label("Query");
                run |= mono(ui, &mut self.filter, "{ status: \"active\" }", wide);
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
                ui.end_row();
                ui.label("Projection");
                ui.horizontal(|ui| {
                    let w = (wide - 260.0) / 2.0;
                    run |= mono(ui, &mut self.projection, "{ name: 1 }", w.max(120.0));
                    ui.label("Sort");
                    run |= mono(ui, &mut self.sort, "{ _id: -1 }", w.max(120.0));
                    ui.label("Skip");
                    run |= mono(ui, &mut self.skip, "0", 60.0);
                });
                ui.end_row();
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
        // A column menu sort shows up in the Sort field.
        for (_, a) in &actions {
            if let ViewAction::SortChanged(sort) = a {
                self.sort = if sort.is_empty() { String::new() } else { rongo_core::format::doc_to_shell(sort, rongo_core::format::Style::Editable).replace('\n', " ").split_whitespace().collect::<Vec<_>>().join(" ") };
            }
        }
        actions
    }
}

// ---------------------------------------------------------------- shell tab

pub struct ShellResult {
    pub label: String,
    pub view: ResultView,
    pub pinned: bool,
}

pub struct ShellTab {
    tab_id: u64,
    pub conn: ConnId,
    pub db: String,
    editor: CodeEditor,
    shell: Option<ShellHandle>,
    running: Option<(u64, Instant)>,
    raw: String,
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
            raw: String::new(),
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
        let Some(shell) = self.shell.as_mut() else { return };
        if req.code.trim().is_empty() {
            return;
        }
        self.results.retain(|r| r.pinned);
        self.active = if self.results.is_empty() { 0 } else { self.active.min(self.results.len()) };
        self.editor.error_line = None;
        self.status = None;
        self.raw.clear();
        let run = shell.run(req.code, self.db.clone(), req.line_offset);
        self.running = Some((run, Instant::now()));
    }

    pub fn poll(&mut self, net: &mut Net) {
        let Some(shell) = &self.shell else { return };
        while let Some(ev) = shell.try_recv() {
            match ev {
                ShellEvent::Print { text, .. } => {
                    self.raw.push_str(&text);
                    self.raw.push('\n');
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
                        self.raw.push_str(&format!("{e}\n"));
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
        let collections = conn.map(|c| c.collection_names(&self.db)).unwrap_or_default();
        sidebar::breadcrumb(ui, conn, &self.db);
        ui.add_space(2.0);

        let mut run: Option<RunRequest> = None;
        ui.horizontal(|ui| {
            let busy = self.running.is_some();
            if ui.add_enabled(!busy, egui::Button::new(RichText::new(icon::PLAY).color(theme::OK).size(16.0))).on_hover_text("Run all or selection (⌘↩ / F5)").clicked() {
                run = Some(self.editor.run_all());
            }
            if ui.add_enabled(!busy, egui::Button::new(RichText::new(icon::SKIP_FORWARD).color(theme::OK).size(16.0))).on_hover_text("Run current statement (⇧⌘↩)").clicked() {
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
                    Err(e) => self.raw = format!("could not read {}: {e}\n", path.display()),
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
                        Err(e) => self.raw = format!("could not save {}: {e}\n", path.display()),
                    }
                }
            }
            if let Some(f) = &self.file {
                ui.weak(f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
            }
        });

        // Editor on top, results below. The split is a stored height that only
        // changes when the user drags the handle: sizing it from the content
        // made it creep a pixel every frame.
        let tab_salt = self.tab_id;
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
                ui.allocate_ui(egui::vec2(width, (inner.height() - footer_h).max(20.0)), |ui| {
                    let db = self.db.clone();
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
                                ui.colored_label(theme::ERROR, e.to_string()).on_hover_text(&e.message);
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
            self.results.remove(i);
            if self.active > self.results.len() {
                self.active = self.results.len();
            }
        }
        ui.separator();
        if self.active == 0 {
            egui::ScrollArea::both().id_salt(("raw", tab_salt)).auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
                let mut text = self.raw.as_str();
                ui.add(egui::TextEdit::multiline(&mut text).font(theme::mono(12.5)).frame(egui::Frame::NONE).desired_width(f32::INFINITY));
            });
            Vec::new()
        } else {
            let idx = self.active - 1;
            self.results[idx].view.ui(ui, net)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::*;

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
}
