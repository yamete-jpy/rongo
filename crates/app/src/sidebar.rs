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
}

impl CollNode {
    pub fn new(info: CollInfo) -> Self {
        Self { info, indexes: None, loading: false, stats: None, stats_loading: false }
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
    DropIndex(ConnId, Namespace, String),
    Validator(ConnId, Namespace),
    CollStats(ConnId, Namespace),
    DbStats(ConnId, String),
    ServerInfo(ConnId, InfoKind),
    /// A connection was dragged to just before another, or (`None`) to the end.
    MoveConnection { id: ConnId, before: Option<ConnId> },
    SetColor(ConnId, Option<String>),
}

fn item(ui: &mut Ui, ico: &str, label: &str, shortcut: &str) -> bool {
    let b = egui::Button::new(format!("{ico}  {label}")).shortcut_text(shortcut);
    let clicked = ui.add(b).clicked();
    if clicked {
        ui.close();
    }
    clicked
}

fn danger_item(ui: &mut Ui, ico: &str, label: &str) -> bool {
    let clicked = ui.button(RichText::new(format!("{ico}  {label}")).color(theme::ERROR)).clicked();
    if clicked {
        ui.close();
    }
    clicked
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
    if item(ui, icon::ARROWS_CLOCKWISE, "Refresh All", "⌘R") {
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

#[derive(Clone, PartialEq, Eq, Default)]
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
}

impl Default for Sidebar {
    fn default() -> Self {
        Self { open: true, search: String::new(), case_sensitive: false, selection: Selection::default() }
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
        let hint = format!("{}  Search open connections (⌘F)", icon::MAGNIFYING_GLASS);
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

    fn hit(&self, s: &str) -> bool {
        if self.case_sensitive { s.contains(&self.needle) } else { s.to_lowercase().contains(&self.needle) }
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
    let top = ui.cursor().top();
    let header = state.show_header(ui, |ui| {
        let r = conn_label(ui, status_icon, color, &conn.profile.name, selected, tag, sense);
        if let Status::Failed(e) = &conn.status {
            r.clone().on_hover_text(e);
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
                ui.colored_label(theme::ERROR, e);
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
            db_node(ui, cid, db, sidebar, filter, net, actions);
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
        if item(ui, icon::TERMINAL_WINDOW, "Open IntelliShell", "⌘T") {
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
        ui.separator();
        if item(ui, icon::ARROWS_CLOCKWISE, "Refresh Selected Item", "⇧⌘R") {
            actions.push(TreeAction::Refresh(cid));
        }
        if matches!(conn.status, Status::Failed(_)) && item(ui, icon::PLUG, "Reconnect", "") {
            actions.push(TreeAction::Reconnect(cid));
        }
        connection_items(ui, cid, actions);
    });
    (header_rect, egui::Rect::from_x_y_ranges(header_rect.x_range(), top..=ui.cursor().top()))
}

fn db_node(
    ui: &mut Ui,
    cid: ConnId,
    db: &mut DbNode,
    sidebar: &mut Sidebar,
    filter: &Filter,
    net: &mut Net,
    actions: &mut Vec<TreeAction>,
) {
    let name = db.info.name.clone();
    if filter.active()
        && !filter.hit(&name)
        && db.colls.as_ref().is_some_and(|c| !c.iter().any(|c| filter.hit(&c.info.name)))
    {
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
        let shown: Vec<usize> = (0..colls.len()).filter(|i| !filter.active() || filter.hit(&colls[*i].info.name) || filter.hit(&name)).collect();
        folder
            .show_header(ui, |ui| row_label(ui, icon::FOLDER, format!("Collections ({})", colls.len()), false, theme::WARN))
            .body(|ui| {
                for i in shown {
                    coll_node(ui, cid, &name, &mut colls[i], sidebar, net, actions);
                }
            });
    });
    let resp = header_resp.inner;
    if resp.clicked() {
        sidebar.selection = Selection { conn: Some(cid), db: Some(name.clone()), coll: None };
    }
    resp.context_menu(|ui| {
        ui.set_min_width(240.0);
        if item(ui, icon::TERMINAL_WINDOW, "Open IntelliShell", "⌘T") {
            actions.push(TreeAction::OpenShell(cid, Some(name.clone())));
        }
        ui.separator();
        if item(ui, icon::PLUS, "Add Collection…", "") {
            actions.push(TreeAction::CreateCollection(cid, name.clone()));
        }
        if item(ui, icon::DOWNLOAD_SIMPLE, "Import Data…", "") {
            actions.push(TreeAction::Import(cid, name.clone(), None));
        }
        if item(ui, icon::CLIPBOARD_TEXT, "Copy Name", "") {
            ui.ctx().copy_text(name.clone());
        }
        ui.separator();
        if item(ui, icon::CHART_BAR, "Database Statistics", "") {
            actions.push(TreeAction::DbStats(cid, name.clone()));
        }
        server_info_menu(ui, cid, actions);
        ui.separator();
        if item(ui, icon::ARROWS_CLOCKWISE, "Refresh Selected Item", "⇧⌘R") {
            db.colls = None;
            db.loading = false;
        }
        if danger_item(ui, icon::TRASH, "Drop Database…") {
            actions.push(TreeAction::DropDatabase(cid, name.clone()));
        }
        connection_items(ui, cid, actions);
    });
}

fn coll_node(
    ui: &mut Ui,
    cid: ConnId,
    db: &str,
    coll: &mut CollNode,
    sidebar: &mut Sidebar,
    net: &mut Net,
    actions: &mut Vec<TreeAction>,
) {
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
    let (_, header_resp, _) = header.body(|ui| {
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
                            if danger_item(ui, icon::TRASH, "Drop Index…") {
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
            if item(ui, icon::PLUS, "Add Index…", "") {
                actions.push(TreeAction::AddIndex(cid, ns.clone()));
            }
        });
    });
    let resp = header_resp.inner;
    // Hover card with the collection's statistics, fetched on first hover.
    let resp = resp.on_hover_ui(|ui| stats_card(ui, cid, &ns, coll, net));
    if resp.clicked() {
        sidebar.selection = Selection { conn: Some(cid), db: Some(db.to_string()), coll: Some(name.clone()) };
    }
    if resp.double_clicked() {
        actions.push(TreeAction::OpenCollection(cid, ns.clone()));
    }
    resp.context_menu(|ui| {
        ui.set_min_width(260.0);
        if item(ui, icon::TERMINAL_WINDOW, "Open in IntelliShell", "") {
            actions.push(TreeAction::OpenCollectionShell(cid, ns.clone()));
        }
        if item(ui, icon::TABLE, "Open Collection Tab", "") {
            actions.push(TreeAction::OpenCollectionTab(cid, ns.clone()));
        }
        if item(ui, icon::TERMINAL, "Open Empty IntelliShell", "⌘T") {
            actions.push(TreeAction::OpenShell(cid, Some(db.to_string())));
        }
        ui.separator();
        if item(ui, icon::EXPORT, "Export Collection…", "") {
            actions.push(TreeAction::Export(cid, ns.clone()));
        }
        if item(ui, icon::DOWNLOAD_SIMPLE, "Import Data…", "") {
            actions.push(TreeAction::Import(cid, db.to_string(), Some(name.clone())));
        }
        if item(ui, icon::CLIPBOARD_TEXT, "Copy Name", "⌥⌘C") {
            ui.ctx().copy_text(name.clone());
        }
        ui.separator();
        if item(ui, icon::PENCIL_SIMPLE, "Rename Collection…", "") {
            actions.push(TreeAction::Rename(cid, ns.clone()));
        }
        if item(ui, icon::COPY, "Duplicate Collection…", "") {
            actions.push(TreeAction::Duplicate(cid, ns.clone()));
        }
        if danger_item(ui, icon::BROOM, "Clear Collection…") {
            actions.push(TreeAction::Clear(cid, ns.clone()));
        }
        if danger_item(ui, icon::TRASH, "Drop Collection…") {
            actions.push(TreeAction::DropCollection(cid, ns.clone()));
        }
        ui.separator();
        if item(ui, icon::LIGHTNING, "Manage Indexes…", "") {
            actions.push(TreeAction::OpenIndexes(cid, ns.clone()));
        }
        if item(ui, icon::PLUS, "Add Index…", "") {
            actions.push(TreeAction::AddIndex(cid, ns.clone()));
        }
        if item(ui, icon::SHIELD_CHECK, "Add / Edit Validator…", "") {
            actions.push(TreeAction::Validator(cid, ns.clone()));
        }
        ui.separator();
        if item(ui, icon::CHART_BAR, "Collection Statistics", "") {
            actions.push(TreeAction::CollStats(cid, ns.clone()));
        }
        server_info_menu(ui, cid, actions);
        ui.separator();
        if item(ui, icon::ARROWS_CLOCKWISE, "Refresh Selected Item", "⇧⌘R") {
            coll.indexes = None;
            coll.loading = false;
        }
        connection_items(ui, cid, actions);
    });

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
    ui.horizontal(|ui| {
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
        ui.label(RichText::new(format!("{}  {host}", icon::HARD_DRIVES)).color(theme::TEXT));
        ui.label(sep());
        ui.label(RichText::new(format!("{}  {db}", icon::DATABASE)).color(theme::TEXT));
    });
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
}
