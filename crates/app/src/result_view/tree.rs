//! The Tree view: each row a document (or a value at the drill level), its nodes opened in place.

use egui::{Align, Align2, Color32, Layout, RichText, Sense, Ui, pos2, vec2};
use egui_phosphor::regular as icon;
use rongo_core::bson::Bson;
use rongo_core::format;

use super::drill::{Row, Rows};
use super::menus::CellCtx;
use super::{ResultView, ViewAction};
use crate::net::Net;
use crate::theme;

/// The Tree view's row heights as last drawn, and their running sums, which
/// place the rows in view without adding up every height each frame. The
/// sums are brought up to date only from the first height that changed (a
/// node opened or closed).
pub(super) struct TreeHeights {
    heights: Vec<f32>,
    /// `tops[i]`: the heights of the rows before row `i` added up; one more
    /// than the rows, the last being them all.
    tops: Vec<f32>,
    /// The first entry of `tops` out of date.
    stale: usize,
}

impl Default for TreeHeights {
    fn default() -> Self {
        Self { heights: Vec::new(), tops: vec![0.0], stale: 1 }
    }
}

impl TreeHeights {
    pub(super) fn len(&self) -> usize {
        self.heights.len()
    }

    pub(super) fn clear(&mut self) {
        self.reset(0, 0.0);
    }

    /// `n` rows of height `h`.
    pub(super) fn reset(&mut self, n: usize, h: f32) {
        self.heights = vec![h; n];
        self.tops = vec![0.0; n + 1];
        self.stale = 1;
    }

    /// Row `i` measured `h` tall. Less than a hundredth of a point is
    /// rounding, not a change.
    pub(super) fn set(&mut self, i: usize, h: f32) {
        if let Some(old) = self.heights.get_mut(i)
            && (*old - h).abs() > 0.01
        {
            *old = h;
            self.stale = self.stale.min(i + 1);
        }
    }

    /// Where each row starts (see `tops`).
    pub(super) fn tops(&mut self) -> &[f32] {
        for k in self.stale..self.tops.len() {
            self.tops[k] = self.tops[k - 1] + self.heights[k - 1];
        }
        self.stale = self.tops.len();
        &self.tops
    }
}

impl ResultView {
    pub(super) fn tree(&mut self, ui: &mut Ui, net: &mut Net, actions: &mut Vec<ViewAction>) {
        let data = self.rows.clone();
        let rows = Rows::new(&data, self.level());
        let mut clicked = None;
        let modifiers = ui.input(|i| i.modifiers);
        let header_rect = ui.available_rect_before_wrap();
        let key_w = (header_rect.width() * 0.35).clamp(160.0, 360.0);
        ui.horizontal(|ui| {
            ui.allocate_ui_with_layout(vec2(key_w, 20.0), Layout::left_to_right(Align::Center), |ui| {
                ui.set_min_width(key_w);
                ui.label(RichText::new("Key").strong());
            });
            ui.label(RichText::new("Value").strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_sized([100.0, 20.0], egui::Label::new(RichText::new("Type").strong()));
            });
        });
        ui.separator();
        // Only rows in view are drawn. Each row's height is remembered once
        // drawn (open nodes are taller), which places the rest.
        let default_h = 20.0 + ui.spacing().item_spacing.y;
        if self.tree_heights.len() != rows.len() {
            self.tree_heights.reset(rows.len(), default_h);
        }
        egui::ScrollArea::vertical().id_salt((self.id, "tree", self.drill.len())).auto_shrink([false, false]).show_viewport(ui, |ui, viewport| {
            // The first row reaching into view, and where it starts.
            let tops = self.tree_heights.tops();
            ui.set_min_height(tops[rows.len()]);
            let mut i = tops[1..].partition_point(|&t| t < viewport.min.y);
            let mut y = tops[i];
            let left = ui.max_rect().left();
            ui.add_space(y);
            while i < rows.len() && y < viewport.max.y {
                let Some(Row { origin, value }) = rows.get(i) else { break };
                let label = match value.as_document().and_then(|d| d.get("_id")) {
                    Some(id) => format!("({}) {}", self.paging.offset as usize + i + 1, format::cell_text(id)),
                    None => format!("({})", self.paging.offset as usize + i + 1),
                };
                // Keyed by the document (its place in the whole result) and
                // the value's path in it, not the row: an open node stays
                // with its document when a header sort or, in a static
                // result, a page turn moves it.
                let id = ui.make_persistent_id((self.id, "node", self.place(origin), rows.path(i)));
                let top = ui.cursor().top();
                // Every other document striped across the full width, so the
                // eye can follow a key to its type on the far right.
                let stripe = ui.painter().add(egui::Shape::Noop);
                let r = tree_node(ui, id, &label, value, left, key_w, self.selection.selected.contains(&i));
                if r.clicked() || (r.secondary_clicked() && !self.selection.selected.contains(&i)) {
                    clicked = Some(i);
                }
                if r.secondary_clicked() {
                    self.menu_gen = Some(self.rows_gen);
                }
                r.context_menu(|ui| {
                    ui.set_min_width(230.0);
                    self.cell_menu(ui, net, CellCtx { row: i, origin, col: None, path: &rows.path(i), cell: None }, actions);
                });
                let h = (ui.cursor().top() - top).max(1.0);
                if i % 2 == 1 {
                    let band = egui::Rect::from_x_y_ranges(left..=ui.max_rect().right(), top..=top + h - ui.spacing().item_spacing.y);
                    ui.painter().set(stripe, egui::Shape::rect_filled(band, 0.0, ui.visuals().faint_bg_color));
                }
                self.tree_heights.set(i, h);
                y += h;
                i += 1;
            }
        });
        if let Some(i) = clicked {
            self.click_row(i, modifiers);
        }
    }
}

/// One tree row (and its children when open). Returns the row's response.
fn tree_node(ui: &mut Ui, id: egui::Id, key: &str, v: &Bson, left: f32, key_w: f32, selected: bool) -> egui::Response {
    let container = matches!(v, Bson::Document(_) | Bson::Array(_));
    let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 20.0), Sense::click());
    if selected {
        ui.painter().rect_filled(rect, 0.0, theme::SELECTION);
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, 0.0, Color32::from_white_alpha(8));
    }
    let painter = ui.painter_at(rect);
    let text_y = rect.center().y;
    if container {
        let arrow = if state.is_open() { icon::CARET_DOWN } else { icon::CARET_RIGHT };
        painter.text(pos2(rect.left() + 2.0, text_y), Align2::LEFT_CENTER, arrow, egui::FontId::proportional(12.0), theme::MUTED);
        if resp.clicked() || resp.double_clicked() {
            state.toggle(ui);
        }
    }
    let kind = format::kind(v);
    painter.text(pos2(rect.left() + 18.0, text_y), Align2::LEFT_CENTER, format!("{}  {key}", theme::type_icon(kind)), egui::FontId::proportional(13.0), theme::TEXT);
    let value_x = left + key_w;
    let type_x = rect.right() - 100.0;
    let value_clip = egui::Rect::from_x_y_ranges(value_x..=type_x - 8.0, rect.y_range());
    ui.painter_at(value_clip).text(pos2(value_x, text_y), Align2::LEFT_CENTER, format::cell_text(v), theme::mono(12.5), theme::type_color(kind));
    painter.text(pos2(type_x, text_y), Align2::LEFT_CENTER, kind.label(), egui::FontId::proportional(12.0), theme::MUTED);
    state.store(ui.ctx());
    let resp = match v {
        Bson::ObjectId(oid) => resp.on_hover_text(format::object_id_details(oid)),
        _ => resp,
    };
    // The children only of an open node, their keys borrowed.
    if container && state.is_open() {
        ui.indent(id.with("children"), |ui| match v {
            Bson::Document(d) => {
                for (i, (k, child)) in d.iter().enumerate() {
                    tree_node(ui, id.with(i), k, child, left, key_w, false);
                }
            }
            Bson::Array(a) => {
                for (i, child) in a.iter().enumerate() {
                    tree_node(ui, id.with(i), &format!("[{i}]"), child, left, key_w, false);
                }
            }
            _ => {}
        });
    }
    resp
}
