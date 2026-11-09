//! The Index Manager tab: a collection's indexes with type, properties, size
//! and usage, and the actions MongoDB allows on them.

use std::collections::BTreeSet;

use egui::{Align, Color32, Layout, RichText, Sense, Ui, vec2};
use egui_phosphor::regular as icon;
use rongo_core::bson::{Bson, Document};
use rongo_core::format::{self, Style};
use rongo_core::{Command, ConnId, IndexInfo, Namespace, RequestId};

use crate::net::{Net, Origin};
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
    loading: Option<RequestId>,
    error: Option<String>,
    selected: Option<usize>,
    expanded: BTreeSet<usize>,
    needs_load: bool,
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
        Self { tab_id, conn, ns, items: None, loading: None, error: None, selected: None, expanded: BTreeSet::new(), needs_load: true }
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
        sidebar::breadcrumb(ui, conn, &self.ns.db);
        ui.label(RichText::new(format!("{}  {}  {}  Indexes", self.ns.coll, icon::CARET_RIGHT, icon::LIGHTNING)).strong());
        ui.add_space(4.0);

        let sel = self.selected_spec().cloned();
        let sel_name = sel.as_ref().and_then(|s| s.get_str("name").ok().map(str::to_string));
        let is_id = sel_name.as_deref() == Some("_id_");
        let (conn_id, ns) = (self.conn, self.ns.clone());
        ui.horizontal(|ui| {
            if ui.add_enabled(self.loading.is_none(), egui::Button::new(icon::ARROWS_CLOCKWISE)).on_hover_text("Refresh").clicked() {
                self.needs_load = true;
            }
            ui.separator();
            if ui.button(format!("{}  Add index", icon::PLUS)).clicked() {
                actions.push(IndexAction::Add(conn_id, ns.clone()));
            }
            let can_drop = sel.is_some() && !is_id;
            if ui.add_enabled(can_drop, egui::Button::new(RichText::new(format!("{}  Drop index", icon::TRASH)).color(if can_drop { theme::ERROR } else { theme::MUTED }))).clicked()
                && let Some(n) = &sel_name
            {
                actions.push(IndexAction::Drop(conn_id, ns.clone(), n.clone()));
            }
            let can_edit = sel.as_ref().is_some_and(|s| s.contains_key("expireAfterSeconds") || !is_id);
            if ui
                .add_enabled(can_edit, egui::Button::new(format!("{}  Edit index", icon::PENCIL_SIMPLE)))
                .on_hover_text("MongoDB can change only the TTL and whether an index is hidden")
                .clicked()
                && let Some(s) = &sel
            {
                actions.push(IndexAction::Edit(conn_id, ns.clone(), s.clone()));
            }
            if ui.add_enabled(sel.is_some(), egui::Button::new(format!("{}  View details", icon::FILE_TEXT))).clicked()
                && let Some(i) = self.selected.and_then(|i| self.items.as_ref()?.get(i))
            {
                actions.push(IndexAction::View(details_doc(i)));
            }
            let hidden = sel.as_ref().is_some_and(|s| s.get_bool("hidden").unwrap_or(false));
            let label = if hidden { format!("{}  Unhide index", icon::EYE) } else { format!("{}  Hide index", icon::EYE_SLASH) };
            if ui
                .add_enabled(sel.is_some() && !is_id, egui::Button::new(label))
                .on_hover_text("A hidden index is kept up to date but not used by queries (MongoDB 4.4+)")
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
        ui.allocate_ui(body, |ui| self.table(ui, &mut actions));
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

    fn table(&mut self, ui: &mut Ui, actions: &mut Vec<IndexAction>) {
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
        let Some(items) = self.items.clone() else {
            ui.add_space(8.0);
            if self.error.is_none() {
                ui.spinner();
            }
            return;
        };
        egui::ScrollArea::vertical().id_salt(("indexes", self.tab_id)).auto_shrink([false, false]).show(ui, |ui| {
            for (i, info) in items.iter().enumerate() {
                let spec = &info.spec;
                let name = spec.get_str("name").unwrap_or("?");
                let (rect, resp) = ui.allocate_exact_size(vec2(width, row_h), Sense::click());
                let fill = if self.selected == Some(i) {
                    theme::SELECTION
                } else if i % 2 == 1 {
                    Color32::from_white_alpha(6)
                } else {
                    Color32::TRANSPARENT
                };
                ui.painter().rect_filled(rect, 0.0, fill);
                let open = self.expanded.contains(&i);
                let cells = [
                    format!("{}  {name}", if open { icon::CARET_DOWN } else { icon::CARET_RIGHT }),
                    index_type(spec).to_string(),
                    index_properties(spec).join(", "),
                    info.size.map_or("n/a".into(), format::human_bytes),
                    usage_text(info),
                ];
                let mut x = rect.left();
                for (text, w) in cells.iter().zip(cols) {
                    let clip = egui::Rect::from_x_y_ranges(x..=x + w - 4.0, rect.y_range());
                    ui.painter_at(clip).text(egui::pos2(x + 6.0, rect.center().y), egui::Align2::LEFT_CENTER, text, egui::FontId::proportional(13.0), theme::TEXT);
                    x += w;
                }
                if resp.clicked() {
                    self.selected = Some(i);
                    // The caret area toggles the details.
                    if resp.interact_pointer_pos().is_some_and(|p| p.x < rect.left() + 24.0) && !self.expanded.remove(&i) {
                        self.expanded.insert(i);
                    }
                }
                if resp.double_clicked() && !self.expanded.remove(&i) {
                    self.expanded.insert(i);
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
                    if name != "_id_" && ui.button(RichText::new(format!("{}  Drop index…", icon::TRASH)).color(theme::ERROR)).clicked() {
                        actions.push(IndexAction::Drop(self.conn, self.ns.clone(), name.to_string()));
                        ui.close();
                    }
                });
                if open {
                    let mut text = String::new();
                    if let Ok(k) = spec.get_document("key") {
                        text.push_str(&format!("key: {}\n", format::to_shell(&Bson::Document(k.clone()), Style::Display)));
                    }
                    for (k, v) in spec.iter().filter(|(k, _)| !matches!(k.as_str(), "key" | "name" | "v" | "ns")) {
                        text.push_str(&format!("{k}: {}\n", format::to_shell(v, Style::Display)));
                    }
                    ui.horizontal(|ui| {
                        ui.add_space(30.0);
                        ui.label(RichText::new(text.trim_end()).font(theme::mono(12.5)).color(theme::MUTED));
                    });
                    ui.add_space(4.0);
                }
            }
        });
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
}
