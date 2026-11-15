//! The Index Manager tab: a collection's indexes with type, properties, size
//! and usage, and the actions MongoDB allows on them.

use std::collections::BTreeSet;

use egui::{Align, Color32, Layout, RichText, Sense, Ui, vec2};
use egui_phosphor::regular as icon;
use rongo_core::bson::{Bson, Document};
use rongo_core::format::{self, Style};
use rongo_core::{Command, ConnId, IndexInfo, Namespace, RequestId};

use crate::net::{Net, Origin};
use crate::result_view::LOCKED;
use crate::sidebar::{self, OpenConn};
use crate::theme;

pub enum IndexAction {
    Add(ConnId, Namespace),
    Drop(ConnId, Namespace, String),
    Edit(ConnId, Namespace, Document),
    View(Document),
    SetHidden(ConnId, Namespace, String, bool),
}

pub struct IndexTab {
    pub tab_id: u64,
    pub conn: ConnId,
    pub ns: Namespace,
    items: Option<Vec<IndexInfo>>,
    /// Each index's cells and details as shown, made when the list arrives
    /// rather than on every repaint.
    rows: Vec<RowText>,
    loading: Option<RequestId>,
    error: Option<String>,
    selected: Option<usize>,
    /// The indexes whose details are shown, by name: a refresh after a
    /// drop or an add moves the rows, and the details stay with theirs.
    expanded: BTreeSet<String>,
    needs_load: bool,
}

/// One index as the table shows it.
struct RowText {
    name: String,
    /// Type, Properties, Size and Usage.
    cells: [String; 4],
    /// Keys and options, shown when the row is expanded.
    details: String,
}

impl RowText {
    fn new(i: &IndexInfo) -> Self {
        let spec = &i.spec;
        let mut details = String::new();
        if let Ok(k) = spec.get_document("key") {
            details.push_str(&format!("key: {}\n", format::to_shell(&Bson::Document(k.clone()), Style::Display)));
        }
        for (k, v) in spec.iter().filter(|(k, _)| !matches!(k.as_str(), "key" | "name" | "v" | "ns")) {
            details.push_str(&format!("{k}: {}\n", format::to_shell(v, Style::Display)));
        }
        Self {
            name: spec.get_str("name").unwrap_or("?").to_string(),
            cells: [index_type(spec).to_string(), index_properties(spec).join(", "), i.size.map_or("n/a".into(), format::human_bytes), usage_text(i)],
            details: details.trim_end().to_string(),
        }
    }
}

/// Studio 3T's "Type" column.
pub fn index_type(spec: &Document) -> &'static str {
    let Ok(keys) = spec.get_document("key") else { return "Regular" };
    if keys.keys().any(|k| k.contains("$**")) {
        return "Wildcard";
    }
    for v in keys.values() {
        match v.as_str() {
            Some("text") => return "Text",
            Some("2dsphere") => return "2dsphere",
            Some("2d") => return "2d",
            Some("hashed") => return "Hashed",
            _ => {}
        }
    }
    "Regular"
}

/// Studio 3T's "Properties" column.
pub fn index_properties(spec: &Document) -> Vec<&'static str> {
    let mut props = Vec::new();
    if spec.get_document("key").is_ok_and(|k| k.len() > 1) {
        props.push("Compound");
    }
    if spec.get_bool("unique").unwrap_or(false) || spec.get_str("name").ok() == Some("_id_") {
        props.push("Unique");
    }
    if spec.get_bool("sparse").unwrap_or(false) {
        props.push("Sparse");
    }
    if spec.contains_key("expireAfterSeconds") {
        props.push("TTL");
    }
    if spec.contains_key("partialFilterExpression") {
        props.push("Partial");
    }
    if spec.contains_key("collation") {
        props.push("Collation");
    }
    if spec.get_bool("hidden").unwrap_or(false) {
        props.push("Hidden");
    }
    props
}

fn usage_text(i: &IndexInfo) -> String {
    match (i.ops, i.since) {
        (Some(ops), Some(since)) => {
            let day = format::iso_date(since).chars().take(10).collect::<String>();
            format!("{} ops since {day}", crate::result_view::thousands(ops.max(0) as u64))
        }
        (Some(ops), None) => format!("{} ops", crate::result_view::thousands(ops.max(0) as u64)),
        _ => "n/a".into(),
    }
}

impl IndexTab {
    pub fn new(tab_id: u64, conn: ConnId, ns: Namespace) -> Self {
        Self { tab_id, conn, ns, items: None, rows: Vec::new(), loading: None, error: None, selected: None, expanded: BTreeSet::new(), needs_load: true }
    }

    pub fn refresh(&mut self) {
        self.needs_load = true;
    }

    pub fn on_details(&mut self, req: RequestId, items: Vec<IndexInfo>) {
        if self.loading == Some(req) {
            self.loading = None;
            self.error = None;
            // Keep the selection on the same index name if it still exists.
            let name = self.selected_spec().and_then(|s| s.get_str("name").ok().map(str::to_string));
            self.selected = name.and_then(|n| items.iter().position(|i| i.spec.get_str("name").ok() == Some(n.as_str())));
            self.rows = items.iter().map(RowText::new).collect();
            self.items = Some(items);
        }
    }

    pub fn on_error(&mut self, req: RequestId, e: String) {
        if self.loading == Some(req) {
            self.loading = None;
            self.error = Some(e);
        }
    }

    #[cfg(test)]
    pub fn items_for_test(&self) -> &[IndexInfo] {
        self.items.as_deref().unwrap_or_default()
    }

    fn selected_spec(&self) -> Option<&Document> {
        Some(&self.items.as_ref()?.get(self.selected?)?.spec)
    }

    pub fn ui(&mut self, ui: &mut Ui, net: &mut Net, conn: Option<&OpenConn>) -> Vec<IndexAction> {
        if self.needs_load && self.loading.is_none() {
            self.needs_load = false;
            self.loading = Some(net.send(Origin::IndexTab(self.tab_id), Some(self.conn), Command::IndexDetails { ns: self.ns.clone() }));
        }
        let mut actions = Vec::new();
        let locked = conn.is_some_and(|c| c.profile.read_only);
        sidebar::breadcrumb(ui, conn, &self.ns.db);
        ui.label(RichText::new(format!("{}  {}  {}  Indexes", self.ns.coll, icon::CARET_RIGHT, icon::LIGHTNING)).strong());
        ui.add_space(4.0);

        // What the toolbar needs of the selected index (its spec is cloned
        // only for Edit, when clicked).
        let sel = self.selected_spec();
        let has_sel = sel.is_some();
        let sel_name = sel.and_then(|s| s.get_str("name").ok().map(str::to_string));
        let has_ttl = sel.is_some_and(|s| s.contains_key("expireAfterSeconds"));
        let hidden = sel.is_some_and(|s| s.get_bool("hidden").unwrap_or(false));
        let is_id = sel_name.as_deref() == Some("_id_");
        let (conn_id, ns) = (self.conn, self.ns.clone());
        // Wraps on a narrow tab rather than widening it.
        ui.horizontal_wrapped(|ui| {
            if ui.add_enabled(self.loading.is_none(), egui::Button::new(icon::ARROWS_CLOCKWISE)).on_hover_text("Refresh").clicked() {
                self.needs_load = true;
            }
            ui.separator();
            // What a locked connection turns off says why on hover.
            let why = |r: egui::Response| if locked { r.on_disabled_hover_text(LOCKED) } else { r };
            if why(ui.add_enabled(!locked, egui::Button::new(format!("{}  Add index", icon::PLUS)))).clicked() {
                actions.push(IndexAction::Add(conn_id, ns.clone()));
            }
            let can_drop = has_sel && !is_id && !locked;
            if why(ui.add_enabled(can_drop, egui::Button::new(RichText::new(format!("{}  Drop index", icon::TRASH)).color(theme::ERROR)))).clicked()
                && let Some(n) = &sel_name
            {
                actions.push(IndexAction::Drop(conn_id, ns.clone(), n.clone()));
            }
            let can_edit = !locked && has_sel && (has_ttl || !is_id);
            if why(ui
                .add_enabled(can_edit, egui::Button::new(format!("{}  Edit index", icon::PENCIL_SIMPLE)))
                .on_hover_text("MongoDB can change only the TTL and whether an index is hidden"))
                .clicked()
                && let Some(s) = self.selected_spec()
            {
                actions.push(IndexAction::Edit(conn_id, ns.clone(), s.clone()));
            }
            if ui.add_enabled(has_sel, egui::Button::new(format!("{}  View details", icon::FILE_TEXT))).clicked()
                && let Some(i) = self.selected.and_then(|i| self.items.as_ref()?.get(i))
            {
                actions.push(IndexAction::View(details_doc(i)));
            }
            let label = if hidden { format!("{}  Unhide index", icon::EYE) } else { format!("{}  Hide index", icon::EYE_SLASH) };
            if why(ui
                .add_enabled(has_sel && !is_id && !locked, egui::Button::new(label))
                .on_hover_text("A hidden index is kept up to date but not used by queries (MongoDB 4.4+)"))
                .clicked()
                && let Some(n) = &sel_name
            {
                actions.push(IndexAction::SetHidden(conn_id, ns.clone(), n.clone(), !hidden));
            }
            ui.label(RichText::new(icon::INFO).color(theme::ACCENT)).on_hover_text(
                "To change an index's keys or options, drop it and add it again.\nUsage counts operations since the server started (needs the indexStats privilege).",
            );
            if self.loading.is_some() {
                ui.spinner();
            }
        });
        ui.add_space(4.0);
        if let Some(e) = &self.error {
            ui.colored_label(theme::ERROR, e);
        }

        let footer_h = 26.0;
        let body = vec2(ui.available_width(), (ui.available_height() - footer_h).max(60.0));
        ui.allocate_ui(body, |ui| self.table(ui, locked, &mut actions));
        ui.separator();
        ui.horizontal(|ui| {
            let items = self.items.as_deref().unwrap_or_default();
            ui.label(format!("{} Index{}", items.len(), if items.len() == 1 { "" } else { "es" }));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let total: u64 = items.iter().filter_map(|i| i.size).sum();
                ui.label(format::human_bytes(total));
            });
        });
        actions
    }

    fn table(&mut self, ui: &mut Ui, locked: bool, actions: &mut Vec<IndexAction>) {
        let width = ui.available_width();
        let cols = [0.30, 0.12, 0.28, 0.12, 0.18].map(|f| f * width);
        let row_h = 24.0;
        let header = ["Name", "Type", "Properties", "Size", "Usage"];
        let (rect, _) = ui.allocate_exact_size(vec2(width, row_h), Sense::hover());
        ui.painter().rect_filled(rect, 0.0, theme::RAISED);
        let mut x = rect.left();
        for (title, w) in header.iter().zip(cols) {
            ui.painter().text(egui::pos2(x + 6.0, rect.center().y), egui::Align2::LEFT_CENTER, *title, egui::FontId::proportional(13.0), theme::TEXT);
            x += w;
        }
        // Taken out while drawn (the rows' menus change the selection), not
        // cloned every repaint.
        let Some(items) = self.items.take() else {
            ui.add_space(8.0);
            if self.error.is_none() {
                ui.spinner();
            }
            return;
        };
        let rows = std::mem::take(&mut self.rows);
        egui::ScrollArea::vertical().id_salt(("indexes", self.tab_id)).auto_shrink([false, false]).show(ui, |ui| {
            for (i, (info, text)) in items.iter().zip(&rows).enumerate() {
                let spec = &info.spec;
                let name = text.name.as_str();
                let (rect, resp) = ui.allocate_exact_size(vec2(width, row_h), Sense::click());
                let fill = if self.selected == Some(i) {
                    theme::SELECTION
                } else if i % 2 == 1 {
                    Color32::from_white_alpha(6)
                } else {
                    Color32::TRANSPARENT
                };
                ui.painter().rect_filled(rect, 0.0, fill);
                let open = self.expanded.contains(name);
                let first = format!("{}  {name}", if open { icon::CARET_DOWN } else { icon::CARET_RIGHT });
                let mut x = rect.left();
                for (cell, w) in std::iter::once(first.as_str()).chain(text.cells.iter().map(String::as_str)).zip(cols) {
                    let clip = egui::Rect::from_x_y_ranges(x..=x + w - 4.0, rect.y_range());
                    ui.painter_at(clip).text(egui::pos2(x + 6.0, rect.center().y), egui::Align2::LEFT_CENTER, cell, egui::FontId::proportional(13.0), theme::TEXT);
                    x += w;
                }
                if resp.clicked() {
                    self.selected = Some(i);
                    // The caret area toggles the details.
                    if resp.interact_pointer_pos().is_some_and(|p| p.x < rect.left() + 24.0) && !self.expanded.remove(name) {
                        self.expanded.insert(name.to_string());
                    }
                }
                if resp.double_clicked() && !self.expanded.remove(name) {
                    self.expanded.insert(name.to_string());
                }
                resp.context_menu(|ui| {
                    self.selected = Some(i);
                    if ui.button(format!("{}  View details", icon::FILE_TEXT)).clicked() {
                        actions.push(IndexAction::View(details_doc(info)));
                        ui.close();
                    }
                    if ui.button(format!("{}  Copy keys", icon::CLIPBOARD_TEXT)).clicked() {
                        let keys = spec.get_document("key").map(|k| format::to_shell(&Bson::Document(k.clone()), Style::Display)).unwrap_or_default();
                        ui.ctx().copy_text(keys);
                        ui.close();
                    }
                    if name != "_id_" && ui.add_enabled(!locked, egui::Button::new(RichText::new(format!("{}  Drop index…", icon::TRASH)).color(theme::ERROR))).on_disabled_hover_text(LOCKED).clicked() {
                        actions.push(IndexAction::Drop(self.conn, self.ns.clone(), name.to_string()));
                        ui.close();
                    }
                });
                if open {
                    ui.horizontal(|ui| {
                        ui.add_space(30.0);
                        ui.label(RichText::new(&text.details).font(theme::mono(12.5)).color(theme::MUTED));
                    });
                    ui.add_space(4.0);
                }
            }
        });
        self.items = Some(items);
        self.rows = rows;
    }
}

/// Everything known about one index, for View details.
fn details_doc(i: &IndexInfo) -> Document {
    let mut d = i.spec.clone();
    if let Some(s) = i.size {
        d.insert("size", format!("{} ({s} bytes)", format::human_bytes(s)));
    }
    d.insert("usage", usage_text(i));
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use rongo_core::bson::doc;

    #[test]
    fn types_and_properties_like_studio_3t() {
        assert_eq!(index_type(&doc! { "key": { "_id": 1 } }), "Regular");
        assert_eq!(index_type(&doc! { "key": { "body": "text" } }), "Text");
        assert_eq!(index_type(&doc! { "key": { "loc": "2dsphere" } }), "2dsphere");
        assert_eq!(index_type(&doc! { "key": { "u": "hashed" } }), "Hashed");
        assert_eq!(index_type(&doc! { "key": { "$**": 1 } }), "Wildcard");
        assert_eq!(index_properties(&doc! { "name": "_id_", "key": { "_id": 1 } }), ["Unique"]);
        assert_eq!(index_properties(&doc! { "name": "a", "key": { "chatID": 1, "messageID": 1 }, "unique": true }), ["Compound", "Unique"]);
        assert_eq!(index_properties(&doc! { "name": "t", "key": { "ingestedAt": 1 }, "expireAfterSeconds": 60 }), ["TTL"]);
        assert_eq!(index_properties(&doc! { "name": "h", "key": { "x": 1 }, "hidden": true, "sparse": true }), ["Sparse", "Hidden"]);
    }

    /// An index's expanded details stay with that index when the list comes
    /// again without one above it (after Drop index): they used to stay at
    /// the position, on whichever index moved up into it.
    #[test]
    fn expanded_details_follow_their_index() {
        let info = |name: &str| IndexInfo { spec: doc! { "name": format!("{name}_1"), "key": { name: 1 } }, size: None, ops: None, since: None };
        let mut tab = IndexTab::new(1, ConnId(uuid::Uuid::nil()), Namespace::new("db", "c"));
        tab.loading = Some(1);
        tab.on_details(1, vec![info("alpha"), info("bravo"), info("charlie")]);
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let frame = |tab: &mut IndexTab, events: Vec<egui::Event>| {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, vec2(1000.0, 700.0))), events, ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| tab.table(ui, false, &mut Vec::new()));
            out.textures_delta.clear();
            out.shapes.iter().filter_map(|s| match &s.shape {
                egui::Shape::Text(t) => Some((t.galley.text().to_string(), t.galley.rect.translate(t.pos.to_vec2()))),
                _ => None,
            }).collect::<Vec<_>>()
        };
        let texts = frame(&mut tab, vec![]);
        let bravo = texts.iter().find(|(t, _)| t.ends_with("  bravo_1")).expect("bravo's row").1;
        // A click on its caret opens the details.
        let at = egui::pos2(bravo.left() + 4.0, bravo.center().y);
        let click = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        frame(&mut tab, vec![egui::Event::PointerMoved(at)]);
        frame(&mut tab, vec![click(true)]);
        frame(&mut tab, vec![click(false)]);
        let details = |texts: &[(String, egui::Rect)]| texts.iter().filter(|(t, _)| t.starts_with("key:")).map(|(t, _)| t.clone()).collect::<Vec<_>>();
        let shown = details(&frame(&mut tab, vec![]));
        assert!(shown.len() == 1 && shown[0].contains("bravo"), "bravo's details: {shown:?}");

        tab.loading = Some(2);
        tab.on_details(2, vec![info("bravo"), info("charlie")]);
        let shown = details(&frame(&mut tab, vec![]));
        assert!(shown.len() == 1 && shown[0].contains("bravo"), "still bravo's details: {shown:?}");
    }
}
