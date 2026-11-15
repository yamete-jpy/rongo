//! The "Open connections" strip and tree: connection → replica set members /
//! databases → collections → indexes. Databases and indexes load when opened.

use std::collections::BTreeMap;

use egui::collapsing_header::CollapsingState;
use egui::epaint::TextShape;
use egui::{RichText, Sense, Ui, vec2};
use egui_phosphor::regular as icon;
use rongo_core::bson::Document;
use rongo_core::profile::ConnectionProfile;
use rongo_core::{CollInfo, CollKind, Command, ConnId, DbInfo, Namespace, ServerInfo};

use crate::net::{Net, Origin};
use crate::result_view::LOCKED;
use crate::shortcut;
use crate::theme;

pub enum Status {
    Connecting,
    Connected(ServerInfo),
    Failed(String),
}

pub struct CollNode {
    pub info: CollInfo,
    pub indexes: Option<Vec<Document>>,
    pub loading: bool,
    /// Statistics for the hover card, with when they were fetched.
    pub stats: Option<Result<(rongo_core::CollSummary, String), String>>,
    pub stats_loading: bool,
    /// Whether its node was open (or closing) when last drawn, so more
    /// than one row high; `None` until known. A closed one off screen is
    /// passed over as a row's height instead of being laid out.
    open: Option<bool>,
}

impl CollNode {
    pub fn new(info: CollInfo) -> Self {
        Self { info, indexes: None, loading: false, stats: None, stats_loading: false, open: None }
    }
}

pub struct DbNode {
    pub info: DbInfo,
    pub colls: Option<Vec<CollNode>>,
    pub loading: bool,
}

pub struct OpenConn {
    pub profile: ConnectionProfile,
    pub status: Status,
    pub dbs: Option<Vec<DbNode>>,
    pub loading_dbs: bool,
    /// Field paths per collection, sampled for autocomplete.
    pub fields: std::collections::HashMap<Namespace, Vec<String>>,
}

impl OpenConn {
    pub fn new(profile: ConnectionProfile) -> Self {
        Self { profile, status: Status::Connecting, dbs: None, loading_dbs: false, fields: Default::default() }
    }

    pub fn info(&self) -> Option<&ServerInfo> {
        match &self.status {
            Status::Connected(i) => Some(i),
            _ => None,
        }
    }

    pub fn db_mut(&mut self, name: &str) -> Option<&mut DbNode> {
        self.dbs.as_mut()?.iter_mut().find(|d| d.info.name == name)
    }

    pub fn collection_names(&self, db: &str) -> Vec<String> {
        self.dbs
            .as_ref()
            .and_then(|dbs| dbs.iter().find(|d| d.info.name == db))
            .and_then(|d| d.colls.as_ref())
            .map(|c| c.iter().map(|c| c.info.name.clone()).collect())
            .unwrap_or_default()
    }

    /// Replace the database list, keeping already-loaded collections.
    pub fn set_databases(&mut self, dbs: Vec<DbInfo>) {
        let mut old: BTreeMap<String, DbNode> =
            self.dbs.take().unwrap_or_default().into_iter().map(|d| (d.info.name.clone(), d)).collect();
        self.dbs = Some(
            dbs.into_iter()
                .map(|info| match old.remove(&info.name) {
                    Some(mut node) => {
                        node.info = info;
                        node
                    }
                    None => DbNode { info, colls: None, loading: false },
                })
                .collect(),
        );
        self.loading_dbs = false;
    }
}

/// Server Info submenu entries; each runs one admin command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InfoKind {
    ServerStatus,
    BuildInfo,
    HostInfo,
    ReplSetStatus,
    CurrentOp,
    ConnectionStatus,
}

impl InfoKind {
    pub const ALL: [InfoKind; 6] = [
        InfoKind::ServerStatus,
        InfoKind::BuildInfo,
        InfoKind::HostInfo,
        InfoKind::ReplSetStatus,
        InfoKind::CurrentOp,
        InfoKind::ConnectionStatus,
    ];

    pub fn title(self) -> &'static str {
        match self {
            InfoKind::ServerStatus => "Server Status",
            InfoKind::BuildInfo => "Build Info",
            InfoKind::HostInfo => "Host Info",
            InfoKind::ReplSetStatus => "Replica Set Status",
            InfoKind::CurrentOp => "Current Operations",
            InfoKind::ConnectionStatus => "Connection Status",
        }
    }

    pub fn command(self) -> rongo_core::bson::Document {
        use rongo_core::bson::doc;
        match self {
            InfoKind::ServerStatus => doc! { "serverStatus": 1 },
            InfoKind::BuildInfo => doc! { "buildInfo": 1 },
            InfoKind::HostInfo => doc! { "hostInfo": 1 },
            InfoKind::ReplSetStatus => doc! { "replSetGetStatus": 1 },
            InfoKind::CurrentOp => doc! { "currentOp": 1 },
            InfoKind::ConnectionStatus => doc! { "connectionStatus": 1, "showPrivileges": true },
        }
    }
}

pub enum TreeAction {
    /// Double-click or Enter: opens the way Settings says.
    OpenCollection(ConnId, Namespace),
    OpenCollectionShell(ConnId, Namespace),
    OpenCollectionTab(ConnId, Namespace),
    OpenShell(ConnId, Option<String>),
    Refresh(ConnId),
    RefreshAll,
    Disconnect(ConnId),
    DisconnectOthers(ConnId),
    DisconnectAll,
    Reconnect(ConnId),
    CreateCollection(ConnId, String),
    DropCollection(ConnId, Namespace),
    DropDatabase(ConnId, String),
    Export(ConnId, Namespace),
    Import(ConnId, String, Option<String>),
    Rename(ConnId, Namespace),
    Duplicate(ConnId, Namespace),
    Clear(ConnId, Namespace),
    AddIndex(ConnId, Namespace),
    OpenIndexes(ConnId, Namespace),
    /// Open the collection's Collection History tab (also for a collection
    /// that no longer exists).
    OpenHistory(ConnId, Namespace),
    DropIndex(ConnId, Namespace, String),
    Validator(ConnId, Namespace),
    CollStats(ConnId, Namespace),
    DbStats(ConnId, String),
    ServerInfo(ConnId, InfoKind),
    /// A connection was dragged to just before another, or (`None`) to the end.
    MoveConnection { id: ConnId, before: Option<ConnId> },
    SetColor(ConnId, Option<String>),
    /// Export Connection…: the connection as a `mongodb://` string.
    ExportConnection(ConnId),
    /// Lock (`true`) or unlock a connection's writes.
    SetReadOnly(ConnId, bool),
    /// Copy collections of a database (to paste elsewhere).
    CopyCollections { conn: ConnId, db: String, colls: Vec<String> },
    /// Paste what was copied into a database.
    PasteCollections { conn: ConnId, db: String },
    /// Paste what was copied into a new database on a connection.
    PasteAsDatabase(ConnId),
}

/// A menu item: `b`, off with `why_not` on hover unless `enabled`; a click
/// closes the menu.
fn menu_item(ui: &mut Ui, enabled: bool, why_not: &str, b: egui::Button<'_>) -> bool {
    let clicked = ui.add_enabled(enabled, b).on_disabled_hover_text(why_not).clicked();
    if clicked {
        ui.close();
    }
    clicked
}

fn item(ui: &mut Ui, ico: &str, label: &str, shortcut: &str) -> bool {
    gated_item(ui, true, "", ico, label, shortcut)
}

/// A menu item that writes: off, with the reason on hover, when the
/// connection is read-only.
fn write_item(ui: &mut Ui, locked: bool, ico: &str, label: &str, shortcut: &str) -> bool {
    gated_item(ui, !locked, LOCKED, ico, label, shortcut)
}

/// A menu item that is off, with the reason on hover, unless `enabled`.
fn gated_item(ui: &mut Ui, enabled: bool, why_not: &str, ico: &str, label: &str, shortcut: &str) -> bool {
    menu_item(ui, enabled, why_not, egui::Button::new(format!("{ico}  {label}")).shortcut_text(shortcut))
}

/// Why Paste is off, if it is: nothing copied yet, or a read-only target.
fn paste_blocked(can_paste: bool, locked: bool) -> Option<&'static str> {
    if locked {
        Some(LOCKED)
    } else if !can_paste {
        Some("Copy collections first")
    } else {
        None
    }
}

fn write_danger_item(ui: &mut Ui, locked: bool, ico: &str, label: &str) -> bool {
    menu_item(ui, !locked, LOCKED, egui::Button::new(RichText::new(format!("{ico}  {label}")).color(theme::ERROR)))
}

fn server_info_menu(ui: &mut Ui, cid: ConnId, actions: &mut Vec<TreeAction>) {
    ui.menu_button(format!("{}  Server Info", icon::INFO), |ui| {
        for kind in InfoKind::ALL {
            if item(ui, "", kind.title(), "") {
                actions.push(TreeAction::ServerInfo(cid, kind));
            }
        }
    });
}

fn connection_items(ui: &mut Ui, cid: ConnId, actions: &mut Vec<TreeAction>) {
    if item(ui, icon::ARROWS_CLOCKWISE, "Refresh All", &shortcut::label("⌘R")) {
        actions.push(TreeAction::RefreshAll);
    }
    ui.separator();
    if item(ui, icon::PLUGS, "Disconnect", "") {
        actions.push(TreeAction::Disconnect(cid));
    }
    if item(ui, "", "Disconnect Others", "") {
        actions.push(TreeAction::DisconnectOthers(cid));
    }
    if item(ui, "", "Disconnect All", "") {
        actions.push(TreeAction::DisconnectAll);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Selection {
    pub conn: Option<ConnId>,
    pub db: Option<String>,
    pub coll: Option<String>,
}

pub struct Sidebar {
    pub open: bool,
    pub search: String,
    pub case_sensitive: bool,
    pub selection: Selection,
    /// Whether collections are copied and waiting to be pasted (set by the
    /// app each frame; the Paste menu items follow it).
    pub can_paste: bool,
    /// The collection rows with the keyboard focus or a menu open last
    /// frame (drawn even off screen), and those found while drawing this one.
    pinned: Pinned,
    next_pinned: Pinned,
}

impl Default for Sidebar {
    fn default() -> Self {
        Self { open: true, search: String::new(), case_sensitive: false, selection: Selection::default(), can_paste: false, pinned: Pinned::default(), next_pinned: Pinned::default() }
    }
}

/// A collection's row: its connection, database and name.
type RowKey = (ConnId, String, String);

/// Collection rows drawn even when scrolled off screen: the one with the
/// keyboard focus and the one with its right-click menu open, so neither
/// is lost (egui drops the focus of a widget that isn't drawn).
#[derive(Default)]
struct Pinned {
    focus: Option<RowKey>,
    menu: Option<RowKey>,
}

impl Pinned {
    /// The pinned collections of database `db` on `cid`.
    fn names_in(&self, cid: ConnId, db: &str) -> [Option<String>; 2] {
        [&self.focus, &self.menu].map(|k| k.as_ref().filter(|k| k.0 == cid && k.1 == db).map(|k| k.2.clone()))
    }
}

/// The vertical strip with rotated labels, left of the tree.
pub fn strip(ui: &mut Ui, sidebar: &mut Sidebar) {
    let label = "Open connections";
    let font = egui::FontId::proportional(13.0);
    let galley = ui.fonts_mut(|f| f.layout_no_wrap(label.into(), font, theme::TEXT));
    let size = vec2(ui.available_width(), galley.size().x + 24.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let fill = if sidebar.open { theme::RAISED } else if resp.hovered() { theme::BORDER } else { theme::PANEL };
    ui.painter().rect_filled(rect, 0.0, fill);
    let pos = egui::pos2(rect.center().x - galley.size().y / 2.0, rect.bottom() - 12.0);
    ui.painter().add(TextShape::new(pos, galley, theme::TEXT).with_angle(-std::f32::consts::FRAC_PI_2));
    if resp.on_hover_text("Show or hide connections").clicked() {
        sidebar.open = !sidebar.open;
    }
}

/// `order` is the saved connections' order; connections not in it (the
/// demo) come last and can't be dragged.
pub fn tree(
    ui: &mut Ui,
    sidebar: &mut Sidebar,
    conns: &mut BTreeMap<ConnId, OpenConn>,
    order: &[ConnId],
    net: &mut Net,
) -> Vec<TreeAction> {
    let mut actions = Vec::new();
    // Claim exactly the panel's area and draw inside it, so nothing in the
    // tree can widen the panel (a resizable panel grows to fit its content).
    let area = ui.available_rect_before_wrap();
    ui.allocate_rect(area, Sense::hover());
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(area).layout(egui::Layout::top_down(egui::Align::Min)));
    child.set_clip_rect(area.intersect(ui.clip_rect()));
    let ui = &mut child;

    // One row, right to left: the case toggle first, then the search box
    // takes exactly what is left. The row gets an explicit height: a bare
    // right-to-left layout would take the whole panel height.
    let row = vec2(ui.available_width(), ui.spacing().interact_size.y + 2.0);
    ui.allocate_ui_with_layout(row, egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let aa = RichText::new("aA").strong().color(if sidebar.case_sensitive { theme::ACCENT } else { theme::MUTED });
        if ui.selectable_label(sidebar.case_sensitive, aa).on_hover_text("Match case").clicked() {
            sidebar.case_sensitive = !sidebar.case_sensitive;
        }
        let hint = format!("{}  {}", icon::MAGNIFYING_GLASS, shortcut::label("Search open connections (⌘F)"));
        ui.add(egui::TextEdit::singleline(&mut sidebar.search).id(egui::Id::new("tree-search")).hint_text(hint).desired_width(f32::INFINITY));
    });
    ui.add_space(4.0);

    // Where the tree starts (tests check it sits right under the search box).
    let start = egui::Rect::from_min_size(ui.cursor().min, vec2(ui.available_width(), 1.0));
    ui.interact(start, egui::Id::new("tree-first-row"), Sense::hover());
    let filter = Filter::new(&sidebar.search, sidebar.case_sensitive);
    egui::ScrollArea::both().id_salt("tree").auto_shrink([false, false]).show(ui, |ui| {
        if conns.is_empty() {
            ui.add_space(8.0);
            ui.weak("No open connections. Use Connect ▾ to open one.");
        }
        let mut list: Vec<_> = conns.iter_mut().map(|(cid, conn)| (*cid, conn)).collect();
        list.sort_by_key(|(cid, _)| order.iter().position(|o| o == cid).unwrap_or(usize::MAX));
        // Each connection's id, header row and whole node, for dropping.
        let mut nodes = Vec::new();
        for (cid, conn) in list {
            let draggable = order.contains(&cid);
            let (header, node) = conn_node(ui, cid, conn, draggable, sidebar, &filter, net, &mut actions);
            nodes.push((cid, header, node));
        }
        if let Some((id, before)) = drop_target(ui, &nodes) {
            actions.push(TreeAction::MoveConnection { id, before });
        }
    });
    sidebar.pinned = std::mem::take(&mut sidebar.next_pinned);
    actions
}

/// While a connection in `rows` (id, header row, whole node) is dragged,
/// show where it would land: above the first row whose header is below the
/// pointer. Returns `(dragged, before)` when it is released there, `before`
/// being `None` for the end.
pub fn drop_target(ui: &Ui, rows: &[(ConnId, egui::Rect, egui::Rect)]) -> Option<(ConnId, Option<ConnId>)> {
    let dragged = *egui::DragAndDrop::payload::<ConnId>(ui.ctx())?;
    let pos = ui.ctx().pointer_latest_pos().filter(|p| ui.clip_rect().contains(*p))?;
    let last = rows.last()?;
    let slot = rows.iter().filter(|(_, header, _)| header.center().y < pos.y).count();
    let before = rows.get(slot).map(|n| n.0);
    // Next to itself: it would stay where it is.
    if before == Some(dragged) || (slot > 0 && rows[slot - 1].0 == dragged) {
        return None;
    }
    let y = rows.get(slot).map_or(last.2.bottom(), |n| n.2.top());
    ui.painter().hline(ui.clip_rect().x_range(), y, egui::Stroke::new(2.0, theme::ACCENT));
    ui.input(|i| i.pointer.any_released()).then_some((dragged, before))
}

struct Filter {
    needle: String,
    case_sensitive: bool,
}

impl Filter {
    fn new(s: &str, case_sensitive: bool) -> Self {
        let s = s.trim();
        Self { needle: if case_sensitive { s.to_string() } else { s.to_lowercase() }, case_sensitive }
    }

    fn active(&self) -> bool {
        !self.needle.is_empty()
    }

    /// Whether `s` holds the search text. Ignoring case, an ASCII name (the
    /// usual kind) is searched byte by byte without allocating: this runs
    /// for every collection of every open database each frame.
    fn hit(&self, s: &str) -> bool {
        if self.case_sensitive {
            s.contains(&self.needle)
        } else if s.is_ascii() {
            // `needle` is lowercase; one with other letters can't be in an
            // ASCII name. An empty one is in every name (`windows(0)` panics).
            let (hay, needle) = (s.as_bytes(), self.needle.as_bytes());
            needle.is_empty() || (needle.len() <= hay.len() && hay.windows(needle.len()).any(|w| w.iter().zip(needle).all(|(a, b)| a.to_ascii_lowercase() == *b)))
        } else {
            s.to_lowercase().contains(&self.needle)
        }
    }
}

/// A tree row: a coloured icon followed by the label.
fn row_label(ui: &mut Ui, ico: &str, text: impl Into<String>, selected: bool, icon_color: egui::Color32) -> egui::Response {
    row_label_sense(ui, ico, text, selected, icon_color, Sense::click())
}

fn row_label_sense(ui: &mut Ui, ico: &str, text: impl Into<String>, selected: bool, icon_color: egui::Color32, sense: Sense) -> egui::Response {
    let mut job = egui::text::LayoutJob::default();
    let font = egui::FontId::proportional(13.5);
    job.append(ico, 0.0, egui::TextFormat::simple(font.clone(), icon_color));
    job.append(&format!("  {}", text.into()), 0.0, egui::TextFormat::simple(font, theme::TEXT));
    ui.add(egui::Button::selectable(selected, job).frame_when_inactive(false).sense(sense))
}

/// A connection's row: icon and name, with its colour as a tint behind the
/// row and a bar at its left. The tree and the Connection Manager both draw
/// connections with it, so they look the same.
pub fn conn_label(ui: &mut Ui, ico: &str, icon_color: egui::Color32, name: &str, selected: bool, tag: Option<egui::Color32>, sense: Sense) -> egui::Response {
    let tint = ui.painter().add(egui::Shape::Noop);
    let r = row_label_sense(ui, ico, name, selected, icon_color, sense);
    if let Some(c) = tag {
        ui.painter().set(tint, egui::Shape::rect_filled(r.rect, 4.0, c.gamma_multiply(0.22)));
        let bar = egui::Rect::from_min_size(r.rect.left_top(), vec2(3.0, r.rect.height()));
        ui.painter().rect_filled(bar, egui::CornerRadius { nw: 4, sw: 4, ne: 0, se: 0 }, c);
    }
    r
}

/// Returns the header row's rect and the whole node's.
#[allow(clippy::too_many_arguments)]
fn conn_node(
    ui: &mut Ui,
    cid: ConnId,
    conn: &mut OpenConn,
    draggable: bool,
    sidebar: &mut Sidebar,
    filter: &Filter,
    net: &mut Net,
    actions: &mut Vec<TreeAction>,
) -> (egui::Rect, egui::Rect) {
    let id = ui.make_persistent_id(("conn", cid));
    let mut state = CollapsingState::load_with_default_open(ui.ctx(), id, true);
    if filter.active() {
        state.set_open(true);
    }
    let (status_icon, color) = match &conn.status {
        Status::Connecting => (icon::CIRCLE_NOTCH, theme::MUTED),
        Status::Connected(_) => (icon::PLUGS_CONNECTED, theme::OK),
        Status::Failed(_) => (icon::PLUGS, theme::ERROR),
    };
    let selected = sidebar.selection.conn == Some(cid) && sidebar.selection.db.is_none();
    let tag = theme::conn_color(conn.profile.color.as_deref());
    let sense = if draggable { Sense::click_and_drag() } else { Sense::click() };
    let locked = conn.profile.read_only;
    let can_paste = sidebar.can_paste;
    let top = ui.cursor().top();
    let header = state.show_header(ui, |ui| {
        let r = conn_label(ui, status_icon, color, &conn.profile.name, selected, tag, sense);
        if let Status::Failed(e) = &conn.status {
            r.clone().on_hover_text(e);
        }
        if locked {
            ui.label(RichText::new(icon::LOCK_SIMPLE).color(theme::WARN)).on_hover_text(LOCKED);
        }
        r
    });
    let (_, header_resp, _) = header.body(|ui| {
        match &conn.status {
            Status::Connecting => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.weak("Connecting…");
                });
                return;
            }
            Status::Failed(e) => {
                let w = ui.available_width();
                theme::clipped_text(ui, e, theme::ERROR, 3, w);
                if ui.button("Reconnect").clicked() {
                    actions.push(TreeAction::Reconnect(cid));
                }
                return;
            }
            Status::Connected(_) => {}
        }
        if conn.dbs.is_none() && !conn.loading_dbs {
            conn.loading_dbs = true;
            net.send(Origin::Databases(cid), Some(cid), Command::ListDatabases);
        }
        if let Some(info) = conn.info()
            && info.members.len() > 1
            && !filter.active()
        {
            let id = ui.make_persistent_id(("members", cid));
            CollapsingState::load_with_default_open(ui.ctx(), id, false)
                .show_header(ui, |ui| row_label(ui, icon::FOLDER, "Replica Set Members", false, theme::WARN))
                .body(|ui| {
                    for m in &info.members {
                        let c = if m.state == "PRIMARY" { theme::OK } else { theme::MUTED };
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(icon::HARD_DRIVES).color(c));
                            ui.label(&m.host);
                            ui.weak(m.state.to_lowercase());
                        });
                    }
                });
        }
        let Some(dbs) = conn.dbs.as_mut() else {
            ui.spinner();
            return;
        };
        for db in dbs.iter_mut() {
            db_node(ui, cid, locked, db, sidebar, filter, net, actions);
        }
    });
    let resp = header_resp.inner;
    let header_rect = header_resp.response.rect;
    if resp.clicked() {
        sidebar.selection = Selection { conn: Some(cid), db: None, coll: None };
    }
    if resp.drag_started() {
        egui::DragAndDrop::set_payload(ui.ctx(), cid);
    }
    resp.context_menu(|ui| {
        ui.set_min_width(240.0);
        if item(ui, icon::TERMINAL_WINDOW, "Open IntelliShell", &shortcut::label("⌘T")) {
            actions.push(TreeAction::OpenShell(cid, conn.profile.default_db.clone()));
        }
        ui.separator();
        server_info_menu(ui, cid, actions);
        ui.menu_button(format!("{}  Color", icon::PALETTE), |ui| {
            if let Some(c) = theme::color_swatches(ui, conn.profile.color.as_deref()) {
                actions.push(TreeAction::SetColor(cid, c));
                ui.close();
            }
        });
        if locked {
            if item(ui, icon::LOCK_SIMPLE_OPEN, "Allow Writes…", "") {
                actions.push(TreeAction::SetReadOnly(cid, false));
            }
        } else if item(ui, icon::LOCK_SIMPLE, "Make Read-only", "") {
            actions.push(TreeAction::SetReadOnly(cid, true));
        }
        if !conn.profile.is_mock() && item(ui, icon::EXPORT, "Export Connection…", "") {
            actions.push(TreeAction::ExportConnection(cid));
        }
        ui.separator();
        let blocked = paste_blocked(can_paste, locked);
        if gated_item(ui, blocked.is_none(), blocked.unwrap_or(""), icon::CLIPBOARD_TEXT, "Paste as New Database…", "") {
            actions.push(TreeAction::PasteAsDatabase(cid));
        }
        ui.separator();
        if item(ui, icon::ARROWS_CLOCKWISE, "Refresh Selected Item", &shortcut::label("⇧⌘R")) {
            actions.push(TreeAction::Refresh(cid));
        }
        if matches!(conn.status, Status::Failed(_)) && item(ui, icon::PLUG, "Reconnect", "") {
            actions.push(TreeAction::Reconnect(cid));
        }
        connection_items(ui, cid, actions);
    });
    (header_rect, egui::Rect::from_x_y_ranges(header_rect.x_range(), top..=ui.cursor().top()))
}

#[allow(clippy::too_many_arguments)]
fn db_node(
    ui: &mut Ui,
    cid: ConnId,
    locked: bool,
    db: &mut DbNode,
    sidebar: &mut Sidebar,
    filter: &Filter,
    net: &mut Net,
    actions: &mut Vec<TreeAction>,
) {
    let name = db.info.name.clone();
    let can_paste = sidebar.can_paste;
    // A database whose name matches shows all its collections.
    let db_hit = !filter.active() || filter.hit(&name);
    if !db_hit && db.colls.as_ref().is_some_and(|c| !c.iter().any(|c| filter.hit(&c.info.name))) {
        return;
    }
    let id = ui.make_persistent_id(("db", cid, &name));
    let mut state = CollapsingState::load_with_default_open(ui.ctx(), id, false);
    if filter.active() && db.colls.is_some() {
        state.set_open(true);
    }
    let selected = sidebar.selection.conn == Some(cid) && sidebar.selection.db.as_deref() == Some(&name) && sidebar.selection.coll.is_none();
    let header = state.show_header(ui, |ui| row_label(ui, icon::DATABASE, &name, selected, theme::MUTED));
    let (_, header_resp, _) = header.body(|ui| {
        if db.colls.is_none() && !db.loading {
            db.loading = true;
            net.send(Origin::Collections(cid, name.clone()), Some(cid), Command::ListCollections { db: name.clone() });
        }
        let Some(colls) = db.colls.as_mut() else {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak("Loading…");
            });
            return;
        };
        let folder_id = ui.make_persistent_id(("colls", cid, &name));
        let mut folder = CollapsingState::load_with_default_open(ui.ctx(), folder_id, true);
        if filter.active() {
            folder.set_open(true);
        }
        // A closed collection's row is as high as this folder's (the same
        // header, icon and font), measured as drawn.
        let mut row_h = 0.0;
        folder
            .show_header(ui, |ui| {
                let r = row_label(ui, icon::FOLDER, format!("Collections ({})", colls.len()), false, theme::WARN);
                row_h = ui.min_rect().height();
                r
            })
            .body(|ui| collections(ui, cid, locked, &name, colls, folder_id, row_h, db_hit, sidebar, filter, net, actions));
    });
    let resp = header_resp.inner;
    if resp.clicked() {
        sidebar.selection = Selection { conn: Some(cid), db: Some(name.clone()), coll: None };
    }
    resp.context_menu(|ui| {
        ui.set_min_width(240.0);
        if item(ui, icon::TERMINAL_WINDOW, "Open IntelliShell", &shortcut::label("⌘T")) {
            actions.push(TreeAction::OpenShell(cid, Some(name.clone())));
        }
        ui.separator();
        if write_item(ui, locked, icon::PLUS, "Add Collection…", "") {
            actions.push(TreeAction::CreateCollection(cid, name.clone()));
        }
        if write_item(ui, locked, icon::DOWNLOAD_SIMPLE, "Import Data…", "") {
            actions.push(TreeAction::Import(cid, name.clone(), None));
        }
        if item(ui, icon::CLIPBOARD_TEXT, "Copy Name", "") {
            ui.ctx().copy_text(name.clone());
        }
        ui.separator();
        // Collections and views, not the server's own system.* ones.
        let copyable: Option<Vec<String>> = db.colls.as_ref().map(|cs| cs.iter().map(|c| c.info.name.clone()).filter(|n| !n.starts_with("system.")).collect());
        let none_to_copy = match &copyable {
            None => Some("Expand the database to load its collections"),
            Some(c) if c.is_empty() => Some("No collections to copy"),
            Some(_) => None,
        };
        if gated_item(ui, none_to_copy.is_none(), none_to_copy.unwrap_or(""), icon::COPY_SIMPLE, "Copy All Collections", "") {
            actions.push(TreeAction::CopyCollections { conn: cid, db: name.clone(), colls: copyable.unwrap_or_default() });
        }
        let blocked = paste_blocked(can_paste, locked);
        if gated_item(ui, blocked.is_none(), blocked.unwrap_or(""), icon::CLIPBOARD_TEXT, "Paste Collection(s)…", &shortcut::label("⌘V")) {
            actions.push(TreeAction::PasteCollections { conn: cid, db: name.clone() });
        }
        ui.separator();
        if item(ui, icon::CHART_BAR, "Database Statistics", "") {
            actions.push(TreeAction::DbStats(cid, name.clone()));
        }
        server_info_menu(ui, cid, actions);
        ui.separator();
        if item(ui, icon::ARROWS_CLOCKWISE, "Refresh Selected Item", &shortcut::label("⇧⌘R")) {
            db.colls = None;
            db.loading = false;
        }
        if write_danger_item(ui, locked, icon::TRASH, "Drop Database…") {
            actions.push(TreeAction::DropDatabase(cid, name.clone()));
        }
        connection_items(ui, cid, actions);
    });
}

/// A database's collections (those `filter` shows; all of them when
/// `db_hit`). Only the rows on screen are laid out: a closed collection off
/// screen is a row's height of space (`row_h` plus spacing), so a database
/// of thousands of collections costs per frame what its visible rows do.
/// Open ones, the selected one, the pinned ones and the rows next to the
/// focused one are always drawn. The space passed over also skips the
/// widget ids those rows would have taken (one each), so every row keeps
/// the ids it would have with all drawn.
#[allow(clippy::too_many_arguments)]
fn collections(
    ui: &mut Ui,
    cid: ConnId,
    locked: bool,
    db: &str,
    colls: &mut [CollNode],
    folder_id: egui::Id,
    row_h: f32,
    db_hit: bool,
    sidebar: &mut Sidebar,
    filter: &Filter,
    net: &mut Net,
    actions: &mut Vec<TreeAction>,
) {
    let gap = ui.spacing().item_spacing.y;
    let stride = row_h + gap;
    // One row more each way, so arrow keys find the row next to the edge.
    let clip = ui.clip_rect().expand2(vec2(0.0, stride));
    let here = |c: Option<ConnId>, d: Option<&str>| c == Some(cid) && d == Some(db);
    let selected = here(sidebar.selection.conn, sidebar.selection.db.as_deref()).then(|| sidebar.selection.coll.clone()).flatten();
    let [focus, menu] = sidebar.pinned.names_in(cid, db);
    // The focused row and the shown rows either side of it: egui moves
    // focus only among widgets drawn that frame (to the nearest one in the
    // key's direction, at any distance), so the next row must be there
    // however fast the keys come, not only once the tree has scrolled.
    let shown = |c: &CollNode| db_hit || filter.hit(&c.info.name);
    let near_focus = match focus.as_deref().and_then(|f| colls.iter().position(|c| c.info.name == f)) {
        Some(i) => [(0..i).rev().find(|&j| shown(&colls[j])), Some(i), (i + 1..colls.len()).find(|&j| shown(&colls[j]))],
        None => [None; 3],
    };
    // The widest row drawn so far: rows passed over can't widen the tree, so
    // without it the sideways scroll would change as the list scrolls.
    let widest_id = folder_id.with("widest");
    let mut widest: f32 = ui.data(|d| d.get_temp(widest_id)).unwrap_or(0.0);
    let left = ui.max_rect().left();
    let mut passed = 0usize;
    for (i, coll) in colls.iter_mut().enumerate() {
        if !shown(coll) {
            continue;
        }
        let open = *coll.open.get_or_insert_with(|| {
            let id = ui.make_persistent_id(("coll", cid, db, &coll.info.name));
            CollapsingState::load(ui.ctx(), id).is_some_and(|s| s.is_open())
        });
        let top = ui.cursor().top() + passed as f32 * stride;
        let off_screen = top > clip.bottom() || top + row_h < clip.top();
        let keep = |n: &Option<String>| n.as_deref() == Some(coll.info.name.as_str());
        if row_h > 0.0 && off_screen && !open && !keep(&selected) && !keep(&menu) && !near_focus.contains(&Some(i)) {
            passed += 1;
            continue;
        }
        if passed > 0 {
            ui.add_space(passed as f32 * stride);
            ui.skip_ahead_auto_ids(passed);
            passed = 0;
        }
        let right = coll_node(ui, cid, locked, db, coll, sidebar, net, actions);
        widest = widest.max(right - left);
    }
    if passed > 0 {
        // As the rows would have ended: no spacing after the last.
        ui.add_space(passed as f32 * stride - gap);
        ui.skip_ahead_auto_ids(passed);
    }
    ui.set_min_width(widest);
    ui.data_mut(|d| d.insert_temp(widest_id, widest));
}

/// One collection's node; returns where its header row ends on the right.
#[allow(clippy::too_many_arguments)]
fn coll_node(
    ui: &mut Ui,
    cid: ConnId,
    locked: bool,
    db: &str,
    coll: &mut CollNode,
    sidebar: &mut Sidebar,
    net: &mut Net,
    actions: &mut Vec<TreeAction>,
) -> f32 {
    let name = coll.info.name.clone();
    let ns = Namespace::new(db, &name);
    let id = ui.make_persistent_id(("coll", cid, db, &name));
    let ico = match coll.info.kind {
        CollKind::View => icon::EYE,
        CollKind::Timeseries => icon::CHART_LINE,
        CollKind::Collection => icon::TABLE,
    };
    let selected = sidebar.selection.conn == Some(cid)
        && sidebar.selection.db.as_deref() == Some(db)
        && sidebar.selection.coll.as_deref() == Some(&name);
    let header = CollapsingState::load_with_default_open(ui.ctx(), id, false)
        .show_header(ui, |ui| row_label(ui, ico, &name, selected, theme::MUTED));
    let (toggle, header_resp, body) = header.body(|ui| {
        if coll.indexes.is_none() && !coll.loading {
            coll.loading = true;
            net.send(Origin::Indexes(cid, ns.clone()), Some(cid), Command::ListIndexes { ns: ns.clone() });
        }
        // "Indexes (n)" folder, as Studio 3T shows it; clicking it opens the
        // index manager.
        let folder_label = match &coll.indexes {
            Some(list) => format!("Indexes ({})", list.len()),
            None => "Indexes".to_string(),
        };
        let folder_id = ui.make_persistent_id(("indexes", cid, &ns.db, &ns.coll));
        let folder = CollapsingState::load_with_default_open(ui.ctx(), folder_id, false)
            .show_header(ui, |ui| row_label(ui, icon::LIGHTNING, folder_label, false, theme::WARN));
        let (_, folder_resp, _) = folder.body(|ui| match &coll.indexes {
            None => {
                ui.spinner();
            }
            Some(list) if list.is_empty() => {
                ui.weak("No indexes");
            }
            Some(list) => {
                for idx in list {
                    let iname = idx.get_str("name").unwrap_or("?");
                    let keys = idx.get_document("key").map(|k| rongo_core::format::to_shell(&k.clone().into(), rongo_core::format::Style::Display)).unwrap_or_default();
                    let unique = idx.get_bool("unique").unwrap_or(false);
                    let row = ui.horizontal(|ui| {
                        ui.label(RichText::new(icon::LIGHTNING).color(theme::WARN));
                        let r = ui.add(egui::Label::new(iname).sense(Sense::click())).on_hover_text(&keys);
                        if unique {
                            ui.weak("unique");
                        }
                        r
                    });
                    if row.inner.double_clicked() {
                        actions.push(TreeAction::OpenIndexes(cid, ns.clone()));
                    }
                    row.inner.context_menu(|ui| {
                        if item(ui, icon::LIGHTNING, "Open Index Manager", "") {
                            actions.push(TreeAction::OpenIndexes(cid, ns.clone()));
                        }
                        if item(ui, icon::CLIPBOARD_TEXT, "Copy Index Keys", "") {
                            ui.ctx().copy_text(keys.clone());
                        }
                        ui.add_enabled_ui(iname != "_id_", |ui| {
                            if write_danger_item(ui, locked, icon::TRASH, "Drop Index…") {
                                actions.push(TreeAction::DropIndex(cid, ns.clone(), iname.to_string()));
                            }
                        });
                    });
                }
            }
        });
        let folder_resp = folder_resp.inner;
        if folder_resp.clicked() {
            actions.push(TreeAction::OpenIndexes(cid, ns.clone()));
        }
        folder_resp.context_menu(|ui| {
            if item(ui, icon::LIGHTNING, "Open Index Manager", "") {
                actions.push(TreeAction::OpenIndexes(cid, ns.clone()));
            }
            if write_item(ui, locked, icon::PLUS, "Add Index…", "") {
                actions.push(TreeAction::AddIndex(cid, ns.clone()));
            }
        });
        // What rongo saved before it deleted or changed documents here. A
        // view has nothing to write and a time series collection keeps none.
        if coll.info.kind == CollKind::Collection {
            let r = ui.horizontal(|ui| {
                // Lined up with the Indexes folder's icon, past its arrow.
                ui.add_space(ui.spacing().indent);
                row_label(ui, icon::CLOCK_COUNTER_CLOCKWISE, "Collection History", false, theme::ACCENT)
            });
            if r.inner.on_hover_text("Documents rongo saved before deleting or changing them here, to preview and restore").clicked() {
                actions.push(TreeAction::OpenHistory(cid, ns.clone()));
            }
        }
    });
    // Open or still closing, it is more than a row high.
    coll.open = Some(body.is_some() || CollapsingState::load(ui.ctx(), id).is_some_and(|s| s.is_open()));
    let right = header_resp.response.rect.right();
    let resp = header_resp.inner;
    if resp.has_focus() || toggle.has_focus() {
        let key = (cid, db.to_string(), name.clone());
        // The keyboard moved here (arrow keys, Tab), maybe to the row just
        // past the edge: scroll it into view with a row to spare either side
        // (a row hidden just past the top, under the search box, lost the
        // next ArrowUp to that box, the nearest widget upwards), at once
        // rather than animated, which a fast key repeat would outrun.
        // (egui's `gained_focus` misses a move made by arrow keys, which
        // happens between frames.)
        if sidebar.pinned.focus.as_ref() != Some(&key) {
            let row = header_resp.response.rect;
            let around = row.expand2(vec2(0.0, row.height() + ui.spacing().item_spacing.y));
            ui.scroll_to_rect_animation(around, None, egui::style::ScrollAnimation::none());
        }
        sidebar.next_pinned.focus = Some(key);
    }
    // Hover card with the collection's statistics, fetched on first hover.
    let resp = resp.on_hover_ui(|ui| stats_card(ui, cid, &ns, coll, net));
    if resp.clicked() {
        sidebar.selection = Selection { conn: Some(cid), db: Some(db.to_string()), coll: Some(name.clone()) };
    }
    if resp.double_clicked() {
        actions.push(TreeAction::OpenCollection(cid, ns.clone()));
    }
    let menu = resp.context_menu(|ui| {
        ui.set_min_width(260.0);
        if item(ui, icon::TERMINAL_WINDOW, "Open in IntelliShell", "") {
            actions.push(TreeAction::OpenCollectionShell(cid, ns.clone()));
        }
        if item(ui, icon::TABLE, "Open Collection Tab", "") {
            actions.push(TreeAction::OpenCollectionTab(cid, ns.clone()));
        }
        if item(ui, icon::TERMINAL, "Open Empty IntelliShell", &shortcut::label("⌘T")) {
            actions.push(TreeAction::OpenShell(cid, Some(db.to_string())));
        }
        ui.separator();
        if item(ui, icon::EXPORT, "Export Collection…", "") {
            actions.push(TreeAction::Export(cid, ns.clone()));
        }
        if write_item(ui, locked, icon::DOWNLOAD_SIMPLE, "Import Data…", "") {
            actions.push(TreeAction::Import(cid, db.to_string(), Some(name.clone())));
        }
        if item(ui, icon::CLIPBOARD_TEXT, "Copy Name", &shortcut::label("⌥⌘C")) {
            ui.ctx().copy_text(name.clone());
        }
        if item(ui, icon::COPY_SIMPLE, "Copy Collection", &shortcut::label("⌘C")) {
            actions.push(TreeAction::CopyCollections { conn: cid, db: db.to_string(), colls: vec![name.clone()] });
        }
        ui.separator();
        if write_item(ui, locked, icon::PENCIL_SIMPLE, "Rename Collection…", "") {
            actions.push(TreeAction::Rename(cid, ns.clone()));
        }
        if write_item(ui, locked, icon::COPY, "Duplicate Collection…", "") {
            actions.push(TreeAction::Duplicate(cid, ns.clone()));
        }
        if write_danger_item(ui, locked, icon::BROOM, "Clear Collection…") {
            actions.push(TreeAction::Clear(cid, ns.clone()));
        }
        if write_danger_item(ui, locked, icon::TRASH, "Drop Collection…") {
            actions.push(TreeAction::DropCollection(cid, ns.clone()));
        }
        ui.separator();
        if item(ui, icon::LIGHTNING, "Manage Indexes…", "") {
            actions.push(TreeAction::OpenIndexes(cid, ns.clone()));
        }
        if coll.info.kind == CollKind::Collection && item(ui, icon::CLOCK_COUNTER_CLOCKWISE, "Collection History", "") {
            actions.push(TreeAction::OpenHistory(cid, ns.clone()));
        }
        if write_item(ui, locked, icon::PLUS, "Add Index…", "") {
            actions.push(TreeAction::AddIndex(cid, ns.clone()));
        }
        if write_item(ui, locked, icon::SHIELD_CHECK, "Add / Edit Validator…", "") {
            actions.push(TreeAction::Validator(cid, ns.clone()));
        }
        ui.separator();
        if item(ui, icon::CHART_BAR, "Collection Statistics", "") {
            actions.push(TreeAction::CollStats(cid, ns.clone()));
        }
        server_info_menu(ui, cid, actions);
        ui.separator();
        if item(ui, icon::ARROWS_CLOCKWISE, "Refresh Selected Item", &shortcut::label("⇧⌘R")) {
            coll.indexes = None;
            coll.loading = false;
        }
        connection_items(ui, cid, actions);
    });
    if menu.is_some() {
        sidebar.next_pinned.menu = Some((cid, db.to_string(), name));
    }
    right
}

/// The collection hover card: count, sizes and index totals, as Studio 3T
/// shows them, with a Refresh link.
fn stats_card(ui: &mut Ui, cid: ConnId, ns: &Namespace, coll: &mut CollNode, net: &mut Net) {
    if coll.stats.is_none() && !coll.stats_loading {
        coll.stats_loading = true;
        net.send(Origin::HoverStats(cid, ns.clone()), Some(cid), Command::CollStats { ns: ns.clone() });
    }
    ui.set_min_width(300.0);
    let line = |ui: &mut Ui, label: &str, value: String| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{label}:")).strong());
            ui.label(value);
        });
    };
    let bytes = |n: u64| format!("{}  ({})", rongo_core::format::human_bytes(n), crate::result_view::thousands(n));
    line(ui, "Collection", ns.to_string());
    match &coll.stats {
        None => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak("Fetching statistics…");
            });
        }
        Some(Err(e)) => {
            ui.colored_label(theme::ERROR, e);
        }
        Some(Ok((s, fetched))) => {
            line(ui, "Count", crate::result_view::thousands(s.count));
            line(ui, "Size", bytes(s.size));
            line(ui, "Storage Size", bytes(s.storage_size));
            line(ui, "Indexes", s.indexes.to_string());
            line(ui, "Total Index Size", bytes(s.total_index_size));
            line(ui, "Stats Fetched", fetched.clone());
        }
    }
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
        if !coll.stats_loading && ui.link("Refresh").clicked() {
            coll.stats_loading = true;
            net.send(Origin::HoverStats(cid, ns.clone()), Some(cid), Command::CollStats { ns: ns.clone() });
        }
    });
}

/// Breadcrumb text pieces for a connection: user, host and role.
pub fn breadcrumb(ui: &mut Ui, conn: Option<&OpenConn>, db: &str) {
    // One line that is never wider than the tab. A row that doesn't fit
    // widens everything under it, so the right-aligned parts of the tab
    // (status, buttons, view switch) end up past the window's edge.
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), ui.spacing().interact_size.y + 2.0), Sense::hover());
    let mut row = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(egui::Layout::left_to_right(egui::Align::Center)));
    row.set_clip_rect(rect.intersect(ui.clip_rect()));
    {
        let ui = &mut row;
        let Some(conn) = conn else {
            ui.colored_label(theme::ERROR, "Not connected");
            return;
        };
        let (user, host) = match conn.info() {
            Some(i) => (
                i.user.clone().unwrap_or_else(|| "(no auth)".into()),
                format!("{} [{}]", i.host, i.role),
            ),
            None => ("…".into(), conn.profile.summary()),
        };
        let sep = || RichText::new(icon::CARET_RIGHT).color(theme::MUTED);
        if conn.profile.read_only {
            ui.label(RichText::new(icon::LOCK_SIMPLE).color(theme::WARN)).on_hover_text(LOCKED);
        }
        // A coloured connection names itself first, so a tab on production
        // can't be mistaken for one on SIT.
        if let Some(c) = theme::conn_color(conn.profile.color.as_deref()) {
            egui::Frame::new().fill(c.gamma_multiply(0.25)).stroke(egui::Stroke::new(1.0, c)).corner_radius(4).inner_margin(egui::Margin::symmetric(6, 1)).show(ui, |ui| {
                ui.label(RichText::new(&conn.profile.name).color(theme::TEXT).strong());
            });
            ui.label(sep());
        }
        ui.label(RichText::new(format!("{}  {user}", icon::USER)).color(theme::TEXT));
        ui.label(sep());
        // The host gives way (cut with "…", whole on hover) so the
        // database name after it stays in view.
        let db_text = format!("{}  {db}", icon::DATABASE);
        let font = egui::TextStyle::Body.resolve(ui.style());
        let tail = ui.fonts_mut(|f| {
            f.layout_no_wrap(db_text.clone(), font.clone(), theme::TEXT).size().x + f.layout_no_wrap(icon::CARET_RIGHT.to_string(), font, theme::MUTED).size().x
        }) + 2.0 * ui.spacing().item_spacing.x;
        let host_w = ui.available_width() - tail;
        theme::clipped_text(ui, &format!("{}  {host}", icon::HARD_DRIVES), theme::TEXT, 1, host_w);
        ui.label(sep());
        ui.label(RichText::new(db_text).color(theme::TEXT));
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::*;

    /// Connections follow the saved order; dragging one onto another's
    /// upper half asks to move it there, a plain click still selects, and a
    /// coloured connection is tinted.
    #[test]
    fn connections_keep_their_order_and_can_be_dragged() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let core = rongo_core::CoreHandle::start(rt.handle().clone(), Arc::new(|| {}));
        let (mut pending, mut next) = (HashMap::new(), 0);
        let mut sidebar = Sidebar::default();
        // Fixed ids, so the map's own order (by id) is not the saved order.
        let [alpha, bravo, charlie] = [(1, "alpha"), (2, "bravo"), (3, "charlie")].map(|(n, name)| ConnectionProfile { id: uuid::Uuid::from_u128(n), ..ConnectionProfile::new(name) });
        let red = theme::conn_color(Some("red")).unwrap();
        let mut conns = BTreeMap::new();
        for p in [&alpha, &bravo, &charlie] {
            let mut p = p.clone();
            if p.name == "charlie" {
                p.color = Some("red".into());
            }
            conns.insert(p.conn_id(), OpenConn::new(p));
        }
        let order = [charlie.conn_id(), alpha.conn_id(), bravo.conn_id()];
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut frame = |events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0))),
                events,
                ..Default::default()
            };
            let mut actions = Vec::new();
            let mut out = ctx.run_ui(input, |ui| {
                egui::Panel::left("sidebar").default_size(300.0).show(ui, |ui| {
                    let mut net = Net { core: &core, pending: &mut pending, next_view_id: &mut next, keys_enabled: false };
                    actions = tree(ui, &mut sidebar, &mut conns, &order, &mut net);
                });
            });
            out.textures_delta.clear();
            let rows: HashMap<String, egui::Rect> = out
                .shapes
                .iter()
                .filter_map(|s| match &s.shape {
                    egui::Shape::Text(t) => ["alpha", "bravo", "charlie"]
                        .into_iter()
                        .find(|n| t.galley.text().ends_with(&format!("  {n}")))
                        .map(|n| (n.to_string(), t.galley.rect.translate(t.pos.to_vec2()))),
                    _ => None,
                })
                .collect();
            let tinted = out.shapes.iter().any(|s| matches!(&s.shape, egui::Shape::Rect(r) if r.fill == red.gamma_multiply(0.22)));
            (actions, rows, tinted, sidebar.selection.conn)
        };

        let (_, rows, tinted, _) = frame(vec![]);
        assert!(rows["charlie"].top() < rows["alpha"].top() && rows["alpha"].top() < rows["bravo"].top(), "saved order: {rows:?}");
        assert!(tinted, "charlie is red");

        let button = |pos, pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        let from = rows["bravo"].center();
        let to = rows["charlie"].center() - vec2(0.0, 4.0);
        let mut actions = Vec::new();
        frame(vec![egui::Event::PointerMoved(from)]);
        frame(vec![button(from, true)]);
        for i in 1..=10 {
            actions.extend(frame(vec![egui::Event::PointerMoved(from.lerp(to, i as f32 / 10.0))]).0);
        }
        actions.extend(frame(vec![button(to, false)]).0);
        let moves: Vec<_> = actions.iter().filter_map(|a| match a {
            TreeAction::MoveConnection { id, before } => Some((*id, *before)),
            _ => None,
        }).collect();
        assert_eq!(moves, [(bravo.conn_id(), Some(charlie.conn_id()))]);

        let at = rows["alpha"].center();
        frame(vec![egui::Event::PointerMoved(at)]);
        frame(vec![button(at, true)]);
        let (actions, _, _, selected) = frame(vec![button(at, false)]);
        assert!(!actions.iter().any(|a| matches!(a, TreeAction::MoveConnection { .. })), "a click is not a drag");
        assert_eq!(selected, Some(alpha.conn_id()));
    }

    /// With no input, the sidebar must keep its width. (Its search row used to
    /// be a few pixels wider than the panel, so the panel grew every frame.)
    #[test]
    fn sidebar_width_is_stable_without_input() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let core = rongo_core::CoreHandle::start(rt.handle().clone(), Arc::new(|| {}));
        let (mut pending, mut next) = (HashMap::new(), 0);
        let mut sidebar = Sidebar::default();
        let profile = ConnectionProfile::mock();
        let mut conn = OpenConn::new(profile.clone());
        conn.status = Status::Connected(rongo_core::ServerInfo {
            version: "7".into(),
            host: "h:27017".into(),
            role: "Primary".into(),
            replica_set: None,
            members: vec![],
            user: None,
        });
        conn.set_databases(vec![DbInfo { name: "a-database-with-a-rather-long-name".into(), size_on_disk: 1 }]);
        let mut conns = BTreeMap::from([(profile.conn_id(), conn)]);
        let ctx = egui::Context::default();
        // The app's spacing and padding decide whether the row overflows.
        crate::theme::install(&ctx);
        let mut widths = Vec::new();
        for _ in 0..30 {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0))),
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| {
                egui::Panel::left("sidebar")
                    .resizable(true)
                    .default_size(300.0)
                    .size_range(200.0..=600.0)
                    .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(6, 6)))
                    .show(ui, |ui| {
                    let mut net = Net { core: &core, pending: &mut pending, next_view_id: &mut next, keys_enabled: false };
                    tree(ui, &mut sidebar, &mut conns, &[], &mut net);
                });
            });
            out.textures_delta.clear();
            let w = egui::containers::panel::PanelState::load(&ctx, egui::Id::new("sidebar")).map_or(0.0, |s| s.size().x);
            widths.push(w);
        }
        assert!(widths[3..].windows(2).all(|w| (w[0] - w[1]).abs() < 0.5), "sidebar width moved: {widths:?}");

        // The search box is one row at the top, so the tree below it shows.
        // (A right-to-left layout once took the whole height, which put the
        // box in the middle and pushed the tree off the bottom.)
        let search = ctx.read_response(egui::Id::new("tree-search")).expect("search box drawn").rect;
        assert!(search.top() < 30.0 && search.height() < 40.0, "search box at {search:?}");
        let row = ctx.read_response(egui::Id::new("tree-first-row")).expect("tree drawn").rect;
        assert!(row.top() > search.bottom() && row.top() < search.bottom() + 40.0, "first tree row at {row:?}, search at {search:?}");
    }

    /// The search matches anywhere in a name, ignoring case unless asked,
    /// in ASCII names and others alike; an empty search matches everything.
    #[test]
    fn search_matches_names() {
        let any = Filter::new("", false);
        assert!(!any.active());
        assert!(any.hit("orders") && any.hit("") && any.hit("ประวัติ"), "an empty search is in every name");
        assert!(Filter::new("  ", true).hit("orders"));
        let f = Filter::new(" Ord ", false);
        assert!(f.hit("orders") && f.hit("PREORDER") && f.hit("ord") && !f.hit("users"));
        assert!(!f.hit("or"), "longer than the name");
        assert!(Filter::new("คำสั่ง", false).hit("ประวัติ_คำสั่งซื้อ"));
        assert!(Filter::new("k", false).hit("\u{212A}elvin"), "the Kelvin sign lowercases to k");
        let exact = Filter::new("Ord", true);
        assert!(exact.hit("Orders") && !exact.hit("orders"));
    }

    /// A failed connection's error is a few lines under its name, not the
    /// whole paragraph the driver wrote.
    #[test]
    fn failed_connection_error_stays_short() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let core = rongo_core::CoreHandle::start(rt.handle().clone(), Arc::new(|| {}));
        let (mut pending, mut next) = (HashMap::new(), 0);
        let mut sidebar = Sidebar::default();
        let profile = ConnectionProfile::mock();
        let mut conn = OpenConn::new(profile.clone());
        conn.status = Status::Failed(crate::test_support::long_driver_error());
        let mut conns = BTreeMap::from([(profile.conn_id(), conn)]);
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0))), ..Default::default() };
        let mut out = ctx.run_ui(input, |ui| {
            egui::Panel::left("sidebar").exact_size(300.0).show(ui, |ui| {
                let mut net = Net { core: &core, pending: &mut pending, next_view_id: &mut next, keys_enabled: false };
                tree(ui, &mut sidebar, &mut conns, &[], &mut net);
            });
        });
        out.textures_delta.clear();
        let error = out.shapes.iter().find_map(|s| match &s.shape {
            egui::Shape::Text(t) if t.galley.text().starts_with("timed out") => Some(t.galley.clone()),
            _ => None,
        });
        let error = error.expect("the error is shown");
        assert!(error.rows.len() <= 3, "{} rows", error.rows.len());
        assert!(error.elided, "cut with an ellipsis");
    }

    /// A collection's node holds a "Collection History" item that opens its
    /// tab; a time series collection (and a view) keeps no history, so it
    /// has none.
    #[test]
    fn no_history_for_time_series() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let core = rongo_core::CoreHandle::start(rt.handle().clone(), Arc::new(|| {}));
        let (mut pending, mut next) = (HashMap::new(), 0);
        let mut sidebar = Sidebar::default();
        let profile = ConnectionProfile::mock();
        let cid = profile.conn_id();
        let mut conn = OpenConn::new(profile);
        conn.status = Status::Connected(rongo_core::ServerInfo { version: "7".into(), host: "h:27017".into(), role: "Primary".into(), replica_set: None, members: vec![], user: None });
        conn.set_databases(vec![DbInfo { name: "tsdb".into(), size_on_disk: 1 }]);
        let colls = [("plain", CollKind::Collection), ("ticks", CollKind::Timeseries), ("shown", CollKind::View)];
        conn.db_mut("tsdb").unwrap().colls = Some(colls.iter().map(|(n, k)| CollNode::new(CollInfo { name: n.to_string(), kind: k.clone() })).collect());
        let mut conns = BTreeMap::from([(cid, conn)]);
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut frame = |events: Vec<egui::Event>| {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0))), events, ..Default::default() };
            let mut actions = Vec::new();
            let mut out = ctx.run_ui(input, |ui| {
                egui::Panel::left("sidebar").exact_size(300.0).show(ui, |ui| {
                    let mut net = Net { core: &core, pending: &mut pending, next_view_id: &mut next, keys_enabled: false };
                    actions = tree(ui, &mut sidebar, &mut conns, &[], &mut net);
                });
            });
            out.textures_delta.clear();
            (actions, out.shapes)
        };
        let text_rect = |shapes: &[egui::epaint::ClippedShape], label: &str| {
            shapes.iter().find_map(|s| match &s.shape {
                egui::Shape::Text(t) if t.galley.text().ends_with(label) => Some(t.galley.rect.translate(t.pos.to_vec2())),
                _ => None,
            })
        };
        // Opens a node by clicking the arrow at the start of its row.
        let mut open = |label: &str| {
            let (_, shapes) = frame(vec![]);
            let row = text_rect(&shapes, label).unwrap_or_else(|| panic!("{label} is drawn"));
            let arrow = shapes.iter().filter_map(|s| match &s.shape {
                egui::Shape::Path(p) if p.points.len() == 3 => {
                    let c = egui::Rect::from_points(&p.points).center();
                    ((c.y - row.center().y).abs() < 6.0 && c.x < row.left()).then_some(c)
                }
                _ => None,
            }).next().unwrap_or_else(|| panic!("{label} has an arrow"));
            let click = |pressed| egui::Event::PointerButton { pos: arrow, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
            frame(vec![egui::Event::PointerMoved(arrow)]);
            frame(vec![click(true)]);
            frame(vec![click(false)]);
        };
        open("  tsdb");
        for c in ["  plain", "  ticks", "  shown"] {
            open(c);
        }
        let (_, shapes) = frame(vec![]);
        let drawn = |label: &str| shapes.iter().filter(|s| matches!(&s.shape, egui::Shape::Text(t) if t.galley.text().ends_with(label))).count();
        assert_eq!(drawn("Indexes"), 3, "every collection is open");
        assert_eq!(drawn("Collection History"), 1, "only the plain collection has history");
        let item = text_rect(&shapes, "Collection History").unwrap();
        let (plain, ticks) = (text_rect(&shapes, "  plain").unwrap(), text_rect(&shapes, "  ticks").unwrap());
        assert!(item.top() > plain.top() && item.top() < ticks.top(), "under plain");

        let at = item.center();
        let click = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        frame(vec![egui::Event::PointerMoved(at)]);
        frame(vec![click(true)]);
        let (actions, _) = frame(vec![click(false)]);
        assert!(actions.iter().any(|a| matches!(a, TreeAction::OpenHistory(c, ns) if *c == cid && *ns == Namespace::new("tsdb", "plain"))), "clicking it opens the tab");
    }

    /// The tree with one open (demo) connection whose database `big` lists
    /// `n` collections (`coll_00000`, …), drawn in a 300 px sidebar.
    struct BigTree {
        _rt: tokio::runtime::Runtime,
        core: rongo_core::CoreHandle,
        pending: HashMap<rongo_core::RequestId, crate::net::Sent>,
        next: u64,
        sidebar: Sidebar,
        conns: BTreeMap<ConnId, OpenConn>,
        ctx: egui::Context,
        cid: ConnId,
    }

    impl BigTree {
        fn new(n: usize) -> Self {
            let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
            let core = rongo_core::CoreHandle::start(rt.handle().clone(), Arc::new(|| {}));
            let profile = ConnectionProfile::mock();
            let cid = profile.conn_id();
            let mut conn = OpenConn::new(profile);
            conn.status = Status::Connected(rongo_core::ServerInfo { version: "7".into(), host: "h:27017".into(), role: "Primary".into(), replica_set: None, members: vec![], user: None });
            conn.set_databases(vec![DbInfo { name: "big".into(), size_on_disk: 1 }]);
            conn.db_mut("big").unwrap().colls = Some((0..n).map(|i| CollNode::new(CollInfo { name: format!("coll_{i:05}"), kind: CollKind::Collection })).collect());
            let ctx = egui::Context::default();
            crate::theme::install(&ctx);
            Self { _rt: rt, core, pending: HashMap::new(), next: 0, sidebar: Sidebar::default(), conns: BTreeMap::from([(cid, conn)]), ctx, cid }
        }

        /// The sidebar panel with the tree in it, as the app draws it.
        fn draw(&mut self, ui: &mut Ui) -> Vec<TreeAction> {
            let (core, pending, next, sidebar, conns) = (&self.core, &mut self.pending, &mut self.next, &mut self.sidebar, &mut self.conns);
            egui::Panel::left("sidebar")
                .exact_size(300.0)
                .show(ui, |ui| {
                    let mut net = Net { core, pending, next_view_id: next, keys_enabled: false };
                    tree(ui, sidebar, conns, &[], &mut net)
                })
                .inner
        }

        fn frame(&mut self, events: Vec<egui::Event>) -> (Vec<TreeAction>, Vec<egui::epaint::ClippedShape>) {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0))), events, ..Default::default() };
            let mut actions = Vec::new();
            let ctx = self.ctx.clone();
            let mut out = ctx.run_ui(input, |ui| actions = self.draw(ui));
            out.textures_delta.clear();
            (actions, out.shapes)
        }

        /// The collection rows drawn on screen, by name, with where they are.
        fn row_rects(shapes: &[egui::epaint::ClippedShape]) -> Vec<(String, egui::Rect)> {
            shapes.iter().filter_map(|s| match &s.shape {
                egui::Shape::Text(t) if s.clip_rect.intersects(t.galley.rect.translate(t.pos.to_vec2())) => {
                    t.galley.text().split_once("  coll_").map(|(_, n)| (format!("coll_{n}"), t.galley.rect.translate(t.pos.to_vec2())))
                }
                _ => None,
            }).collect()
        }

        /// Where the text ending in `label` is drawn.
        fn find(shapes: &[egui::epaint::ClippedShape], label: &str) -> Option<egui::Rect> {
            shapes.iter().find_map(|s| match &s.shape {
                egui::Shape::Text(t) if t.galley.text().ends_with(label) && s.clip_rect.intersects(t.galley.rect.translate(t.pos.to_vec2())) => Some(t.galley.rect.translate(t.pos.to_vec2())),
                _ => None,
            })
        }

        /// The collection rows drawn on screen, by name.
        fn rows(shapes: &[egui::epaint::ClippedShape]) -> Vec<String> {
            Self::row_rects(shapes).into_iter().map(|(name, _)| name).collect()
        }

        /// A click at `at`, over three frames (move, press, release).
        fn click(&mut self, at: egui::Pos2, button: egui::PointerButton) -> Vec<TreeAction> {
            let press = |pressed| egui::Event::PointerButton { pos: at, button, pressed, modifiers: egui::Modifiers::NONE };
            self.frame(vec![egui::Event::PointerMoved(at)]);
            self.frame(vec![press(true)]);
            self.frame(vec![press(false)]).0
        }

        /// Opens a node by clicking the arrow at the start of its row.
        fn open(&mut self, label: &str) {
            let (_, shapes) = self.frame(vec![]);
            let row = Self::find(&shapes, label).unwrap_or_else(|| panic!("{label} is drawn"));
            let arrow = shapes.iter().filter_map(|s| match &s.shape {
                egui::Shape::Path(p) if p.points.len() == 3 => {
                    let c = egui::Rect::from_points(&p.points).center();
                    ((c.y - row.center().y).abs() < 6.0 && c.x < row.left()).then_some(c)
                }
                _ => None,
            }).next().unwrap_or_else(|| panic!("{label} has an arrow"));
            self.click(arrow, egui::PointerButton::Primary);
        }

        /// The mouse wheel over the tree.
        fn scroll(&mut self, dy: f32) -> Vec<egui::epaint::ClippedShape> {
            let wheel = egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: egui::vec2(0.0, dy), phase: egui::TouchPhase::Move, modifiers: egui::Modifiers::NONE };
            self.frame(vec![egui::Event::PointerMoved(egui::pos2(150.0, 450.0)), wheel]).1
        }
    }

    /// Milliseconds per frame of a tree showing a database of `n`
    /// collections (all of them matching `search`), with no input.
    fn tree_frame_ms(n: usize, search: &str) -> f64 {
        let mut t = BigTree::new(n);
        t.sidebar.search = search.into();
        t.frame(vec![]);
        if search.is_empty() {
            t.open("  big");
        }
        let (_, shapes) = t.frame(vec![]);
        assert!(BigTree::rows(&shapes).contains(&"coll_00000".to_string()), "the collections are drawn");
        // The fastest of several rounds: on a loaded machine another process
        // can slow any one round, but not every one.
        (0..7)
            .map(|_| {
                let start = std::time::Instant::now();
                for _ in 0..5 {
                    t.frame(vec![]);
                }
                start.elapsed().as_secs_f64() * 1000.0 / 5.0
            })
            .fold(f64::INFINITY, f64::min)
    }

    /// A database of 5,000 collections costs about what one of 100 does per
    /// frame, with or without a search that matches them all: only the
    /// rows on screen are laid out.
    #[test]
    fn tree_frame_cost_does_not_grow_with_collections() {
        for search in ["", "coll"] {
            let small = tree_frame_ms(100, search);
            let big = tree_frame_ms(5_000, search);
            assert!(big < small * 3.0 + 5.0, "search {search:?}: {big:.1} ms per frame with 5,000 collections vs {small:.1} ms with 100");
        }
    }

    /// A database of 5,000 collections scrolls to its last one: the rows on
    /// screen follow one another at an even step, wherever the list is, and
    /// the last ends near the bottom of the tree, not above a blank stretch.
    /// A click picks the row under the pointer; a search shows only the
    /// collections that match, from the top.
    #[test]
    fn a_long_database_scrolls_and_picks_the_right_row() {
        let mut t = BigTree::new(5_000);
        t.frame(vec![]);
        t.open("  big");
        let even = |rows: &[(String, egui::Rect)]| {
            let names: Vec<usize> = rows.iter().map(|(n, _)| n[5..].parse().unwrap()).collect();
            assert!(names.len() > 20, "a screenful of rows: {names:?}");
            assert!(names.windows(2).all(|w| w[1] == w[0] + 1), "one after another: {names:?}");
            let steps: Vec<f32> = rows.windows(2).map(|w| w[1].1.top() - w[0].1.top()).collect();
            assert!(steps.iter().all(|s| (s - steps[0]).abs() < 0.5), "an even step: {steps:?}");
        };
        even(&BigTree::row_rects(&t.frame(vec![]).1));
        let mut shapes = Vec::new();
        for _ in 0..400 {
            shapes = t.scroll(-997.0);
        }
        let rows = BigTree::row_rects(&shapes);
        even(&rows);
        let last = rows.last().unwrap();
        assert_eq!(last.0, "coll_04999");
        assert!(last.1.bottom() > 900.0 - 60.0 && last.1.bottom() <= 900.0, "the last row ends at the bottom: {:?}", last.1);
        // Part way up, too, once the scrolling has come to rest.
        for _ in 0..37 {
            t.scroll(1013.0);
        }
        for _ in 0..30 {
            shapes = t.frame(vec![]).1;
        }
        let rows = BigTree::row_rects(&shapes);
        even(&rows);
        let (name, rect) = rows[rows.len() / 2].clone();
        t.click(rect.center(), egui::PointerButton::Primary);
        assert_eq!(t.sidebar.selection, Selection { conn: Some(t.cid), db: Some("big".into()), coll: Some(name.clone()) }, "the row under the pointer");

        t.sidebar.search = "coll_049".into();
        let mut shapes = Vec::new();
        for _ in 0..3 {
            shapes = t.scroll(5_000.0);
        }
        let rows = BigTree::row_rects(&shapes);
        assert_eq!(rows.first().map(|r| r.0.as_str()), Some("coll_04900"), "from the top");
        assert!(rows.iter().all(|(n, _)| n.starts_with("coll_049")));
        even(&rows);
    }

    /// A collection's right-click menu stays open, and acts on that
    /// collection, while the tree scrolls its row out of view (the row is
    /// still drawn, with the ids it had).
    #[test]
    fn a_row_menu_stays_open_while_the_tree_scrolls() {
        let mut t = BigTree::new(5_000);
        t.frame(vec![]);
        t.open("  big");
        let (_, shapes) = t.frame(vec![]);
        let row = BigTree::find(&shapes, "  coll_00003").expect("a row near the top");
        t.click(row.center(), egui::PointerButton::Secondary);
        assert!(BigTree::find(&t.frame(vec![]).1, "Drop Collection…").is_some(), "the menu opened");
        let wheel = egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: egui::vec2(0.0, -2_000.0), phase: egui::TouchPhase::Move, modifiers: egui::Modifiers::NONE };
        // The wheel over the tree, not over the menu.
        t.frame(vec![egui::Event::PointerMoved(egui::pos2(150.0, 850.0)), wheel]);
        let mut shapes = Vec::new();
        for _ in 0..30 {
            shapes = t.frame(vec![]).1;
        }
        assert!(!BigTree::rows(&shapes).contains(&"coll_00003".to_string()), "the row scrolled away");
        let drop = BigTree::find(&shapes, "Drop Collection…").expect("the menu is still open");
        let actions = t.click(drop.center(), egui::PointerButton::Primary);
        let dropped: Vec<&Namespace> = actions.iter().filter_map(|a| match a {
            TreeAction::DropCollection(_, ns) => Some(ns),
            _ => None,
        }).collect();
        assert_eq!(dropped, [&Namespace::new("big", "coll_00003")], "the menu is coll_00003's");
    }

    /// Arrow keys move the keyboard focus one collection per press past the
    /// bottom of the tree and back up past its top, at 60 frames a second
    /// with a press every frame (fast key repeat): the focused row and its
    /// neighbours are always drawn, and the focused row is scrolled into
    /// view at once, so the next press finds the next row rather than
    /// another database's header further down.
    #[test]
    fn arrow_keys_walk_past_both_edges_of_a_long_database() {
        use egui_kittest::kittest::Queryable;
        let mut t = BigTree::new(5_000);
        // Another database below, whose header egui would pick if the next
        // row weren't drawn.
        let cid = t.cid;
        t.conns.get_mut(&cid).unwrap().set_databases(vec![DbInfo { name: "big".into(), size_on_disk: 1 }, DbInfo { name: "zz_after".into(), size_on_disk: 1 }]);
        // A search opens the database (1,000 collections match; zz_after's
        // aren't loaded, so it stays, closed).
        t.sidebar.search = "coll_0".into();
        let mut h = egui_kittest::Harness::builder().with_size(egui::vec2(1400.0, 900.0)).with_step_dt(1.0 / 60.0).build_ui_state(
            |ui, t: &mut BigTree| {
                crate::theme::install(ui.ctx());
                t.draw(ui);
            },
            t,
        );
        h.run_steps(3);
        assert!(h.query_by_label_contains("zz_after").is_some(), "the database below is drawn");
        h.query_by_label_contains("coll_00010").expect("drawn").focus();
        h.run_steps(2);
        assert!(h.get_by_label_contains("coll_00010").is_focused());
        let on_screen = |h: &egui_kittest::Harness<'_, BigTree>, name: &str| {
            let node = h.query_by_label_contains(name).unwrap_or_else(|| panic!("{name} is drawn"));
            assert!(node.is_focused(), "focus is on {name}");
            let r = node.rect();
            assert!(r.top() >= 0.0 && r.bottom() <= 900.0, "{name} is on screen: {r:?}");
        };
        // After the last press: a frame for the row to take the focus and
        // ask to be scrolled to, and one to be drawn there.
        for _ in 0..60 {
            h.key_press(egui::Key::ArrowDown);
            h.run_steps(1);
        }
        h.run_steps(2);
        on_screen(&h, "coll_00070");
        for _ in 0..65 {
            h.key_press(egui::Key::ArrowUp);
            h.run_steps(1);
        }
        h.run_steps(2);
        on_screen(&h, "coll_00005");

        // Scrolled away with the wheel, the focused row and the rows either
        // side of it are still drawn: ArrowDown goes to the next collection
        // (and back into view), not to the nearest row on screen.
        for _ in 0..10 {
            h.event(egui::Event::PointerMoved(egui::pos2(150.0, 450.0)));
            h.event(egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: egui::vec2(0.0, -2_000.0), phase: egui::TouchPhase::Move, modifiers: egui::Modifiers::NONE });
            h.run_steps(1);
        }
        h.run_steps(60);
        let away = h.query_by_label_contains("coll_00005").map(|n| n.rect());
        assert!(away.is_some_and(|r| r.bottom() < 0.0), "coll_00005 scrolled out of view, still drawn: {away:?}");
        h.key_press(egui::Key::ArrowDown);
        h.run_steps(3);
        on_screen(&h, "coll_00006");
    }

    /// Prints the tree's frame times (run with --release --ignored --nocapture).
    #[test]
    #[ignore = "prints timings"]
    fn report_tree_frame_times() {
        for search in ["", "coll"] {
            for n in [100, 5_000] {
                println!("search {search:?}, {n} collections: {:.2} ms per frame", tree_frame_ms(n, search));
            }
        }
    }

    /// Scrolling a long database to places it hasn't been before doesn't
    /// keep adding memory: a row keeps its widget ids wherever the list is
    /// scrolled. Release only: debug builds of egui record every id.
    #[test]
    #[cfg_attr(debug_assertions, ignore = "egui records every id in debug builds; run with --release")]
    fn scrolling_the_tree_does_not_keep_adding_memory() {
        let mut t = BigTree::new(5_000);
        t.frame(vec![]);
        t.open("  big");
        // Every row once, down and back up.
        for (dy, end) in [(-500.0, "coll_04999"), (500.0, "coll_00000")] {
            let mut shapes = Vec::new();
            for _ in 0..300 {
                shapes = t.scroll(dy);
            }
            assert!(BigTree::rows(&shapes).contains(&end.to_string()), "scrolled to {end}");
        }
        let before = crate::heap::live();
        // The same rows again, stopping at new places.
        for dy in [-37.0, 37.0] {
            for _ in 0..1500 {
                t.scroll(dy);
            }
        }
        let grown = crate::heap::live() - before;
        assert!(grown < 100 * 1024, "scrolling added {} KiB", grown / 1024);
    }
}
