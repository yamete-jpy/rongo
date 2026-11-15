//! The Table view: one row per document (or value at the drill level), its columns per drill level.

use std::collections::BTreeSet;

use egui::{Align, Align2, Layout, Modifiers, RichText, Sense, Ui, pos2, vec2};
use egui_extras::{Column, TableBody, TableBuilder, TableRow};
use egui_phosphor::regular as icon;
use rongo_core::bson::Bson;
use rongo_core::format;

use super::drill::{Row, Rows, Step, cell_in, join};
use super::menus::CellCtx;
use super::{ResultView, ViewAction};
use crate::net::Net;
use crate::theme;

static NO_COLUMNS: BTreeSet<String> = BTreeSet::new();

/// Selecting with a dashed box, dragged from a row or from the empty space
/// under them: every row the box touches, on top of `kept` (the selection
/// before, when ⌘ was held). Where it began is kept as a height below the
/// top of the first row, so it moves with the rows when the table scrolls.
pub(super) struct RowDrag {
    start_x: f32,
    start_y: f32,
    kept: BTreeSet<usize>,
}

/// One frame of the table: what it is drawn with, and what drawing its
/// header and rows collected, acted on once they are drawn
/// (`apply_table_input`).
struct TableFrame {
    modifiers: Modifiers,
    /// The drill depth it is drawn at.
    depth: usize,
    /// The space under the last row, which answers a right-click and starts
    /// a drag too.
    background: egui::Response,
    clicked: Option<(usize, String, Modifiers)>,
    double: Option<(usize, String)>,
    header_click: Option<(String, Option<i32>)>,
    /// Header cells as drawn, for dropping a dragged header between them.
    header_rects: Vec<(String, egui::Rect)>,
    drag_started: Option<String>,
    drag_stopped: bool,
    /// A drag over the rows: where it began, and each drawn row's span.
    row_drag_start: Option<(usize, String)>,
    row_spans: Vec<(usize, egui::Rangef)>,
    body_view: egui::Rect,
}

impl ResultView {
    /// The drill level as a key for what is kept per level (column order,
    /// hidden columns): the fields stepped into, not the rows they were
    /// stepped into from, so it holds while a row step follows its document.
    pub(super) fn order_key(&self) -> String {
        let step = |s: &Step| match s {
            Step::Field(f) => format!("{f:?}"),
            Step::Elements { field, .. } => format!("{field:?}[]"),
            Step::Cell { field, .. } => format!("{field:?}{{}}"),
        };
        self.drill.iter().map(step).collect::<Vec<_>>().join(".")
    }

    /// The columns hidden at this drill level. Builds the level's key: read
    /// it once per use, not per column.
    pub(super) fn hidden(&self) -> &BTreeSet<String> {
        self.cols.hidden.get(&self.order_key()).unwrap_or(&NO_COLUMNS)
    }

    pub(super) fn hidden_mut(&mut self) -> &mut BTreeSet<String> {
        let key = self.order_key();
        self.cols.hidden.entry(key).or_default()
    }

    /// `cols` in the order the user dragged them into at this drill level.
    pub(super) fn ordered(&self, mut cols: Vec<String>) -> Vec<String> {
        if let Some(order) = self.cols.order.get(&self.order_key()) {
            cols.sort_by_key(|c| order.iter().position(|o| o == c).unwrap_or(usize::MAX));
        }
        cols
    }

    /// Move column `from` to just before `before` (`None`: to the end).
    pub(super) fn move_column(&mut self, from: &str, before: Option<&str>) {
        let all = self.cols.cache.clone().unwrap_or_default();
        let mut order = self.ordered(all);
        if !order.iter().any(|c| c == from) || before == Some(from) {
            return;
        }
        order.retain(|c| c != from);
        let at = before.and_then(|b| order.iter().position(|c| c == b)).unwrap_or(order.len());
        order.insert(at, from.to_string());
        self.cols.order.insert(self.order_key(), order);
    }

    /// Dotted prefix shared by every row, when the drill path is all fields.
    pub(super) fn shared_prefix(&self) -> Option<String> {
        self.drill.iter().try_fold(String::new(), |acc, s| match s {
            Step::Field(f) => Some(join(&acc, f)),
            _ => None,
        })
    }

    /// While the dashed box is being dragged: select the rows it touches
    /// and draw it. The pointer counts only inside `view` (the rows on
    /// screen); past an edge the table scrolls and the box follows.
    fn drag_rows(&mut self, ui: &Ui, rows: Option<RowGeometry>, view: egui::Rect, len: usize) {
        let Some(drag) = &self.selection.row_drag else { return };
        if !ui.input(|i| i.pointer.primary_down()) {
            self.selection.row_drag = None;
            return;
        }
        let (Some(p), Some(g)) = (ui.ctx().pointer_latest_pos(), rows) else { return };
        let at = pos2(p.x.clamp(view.left(), view.right()), p.y.clamp(view.top(), view.bottom()));
        let (lo, hi) = (drag.start_y.min(at.y - g.top), drag.start_y.max(at.y - g.top));
        let first = (lo / g.pitch).floor().max(0.0) as usize;
        let last = (hi / g.pitch).floor();
        let touched = (last >= 0.0 && first < len).then(|| first..=(last as usize).min(len - 1));
        self.selection.selected = drag.kept.iter().copied().chain(touched.into_iter().flatten()).collect();

        let rect = egui::Rect::from_two_pos(pos2(drag.start_x, g.top + drag.start_y), at);
        let painter = ui.painter().with_clip_rect(view.intersect(ui.clip_rect()));
        painter.rect_filled(rect, 0.0, theme::ACCENT.gamma_multiply(0.08));
        let corners = [rect.left_top(), rect.right_top(), rect.right_bottom(), rect.left_bottom(), rect.left_top()];
        painter.extend(egui::Shape::dashed_line(&corners, egui::Stroke::new(1.0, theme::ACCENT), 4.0, 3.0));
    }

    pub(super) fn click_row(&mut self, i: usize, modifiers: Modifiers) {
        if modifiers.command {
            if !self.selection.selected.remove(&i) {
                self.selection.selected.insert(i);
            }
            self.selection.anchor = Some(i);
        } else if modifiers.shift
            && let Some(a) = self.selection.anchor
        {
            self.selection.selected = (a.min(i)..=a.max(i)).collect();
        } else {
            self.selection.selected = BTreeSet::from([i]);
            self.selection.anchor = Some(i);
        }
    }

    pub(super) fn table(&mut self, ui: &mut Ui, net: &mut Net, actions: &mut Vec<ViewAction>) {
        // Borrow the rows through our own Arc handle, so `self` stays free
        // for the context menu and nothing is cloned per frame.
        let data = self.rows.clone();
        let rows = Rows::new(&data, self.level());
        let all = self.columns(&rows);
        let mut columns = self.ordered(all);
        let hidden = self.hidden();
        columns.retain(|c| !hidden.contains(c));
        let modifiers = ui.input(|i| i.modifiers);
        let depth = self.drill.len();
        // The selection is read where it is used, not copied: after ⌘A it is
        // every row, and a copy per frame grew with the page.
        // Each column's field path, when it can be sorted, and its sort.
        let prefix = self.shared_prefix();
        let sorts: Vec<_> = columns
            .iter()
            .map(|c| {
                let path = prefix.as_ref().filter(|_| c != "(value)").map(|p| join(p, c));
                let dir = path.as_deref().and_then(|p| self.sorted_by(p));
                (path, dir)
            })
            .collect();
        let header_sorted = self.local_sort.is_some();
        // The space under the last row answers a right-click too. Added
        // before the table, so rows and headers drawn over it come first.
        let background = ui.interact(ui.available_rect_before_wrap(), ui.id().with((self.id, "table-bg", depth)), Sense::CLICK | Sense::DRAG);
        let mut frame = TableFrame {
            modifiers,
            depth,
            background,
            clicked: None,
            double: None,
            header_click: None,
            header_rects: Vec::new(),
            drag_started: None,
            drag_stopped: false,
            row_drag_start: None,
            row_spans: Vec::new(),
            body_view: egui::Rect::NOTHING,
        };
        // The table keeps widths by position: a new order starts a new
        // table, with each column at the width it had.
        let order_salt = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            columns.hash(&mut h);
            h.finish()
        };

        egui::ScrollArea::horizontal().id_salt((self.id, "hscroll", depth)).auto_shrink([false, false]).show(ui, |ui| {
            let mut table = TableBuilder::new(ui)
                .id_salt((self.id, "table", depth, order_salt))
                .striped(true)
                .resizable(true)
                // Clicks and drags, but not keyboard focus: egui remembers every
                // focusable widget until the arrow keys move focus, and a cell's
                // id changes as the table scrolls, so the list only grew. The
                // table handles its own keys.
                .sense(Sense::CLICK | Sense::DRAG)
                .cell_layout(Layout::left_to_right(Align::Center))
                .auto_shrink([false, false])
                .min_scrolled_height(0.0);
            for c in &columns {
                let w = self.cols.widths.get(c).copied().unwrap_or(if c == "_id" { 210.0 } else { 160.0 });
                table = table.column(Column::initial(w).at_least(40.0).clip(true).resizable(true));
            }
            table = table.column(Column::remainder().at_least(20.0));
            table.header(24.0, |mut header| table_header(&mut header, &columns, &sorts, header_sorted, &mut frame)).body(|body| self.table_body(body, net, &rows, &columns, &mut frame, actions));
        });
        self.apply_table_input(ui, net, &rows, frame, actions);
    }

    /// The table's rows (only those in view are drawn), with their clicks,
    /// drags and right-click menus.
    fn table_body(&mut self, mut body: TableBody<'_>, net: &mut Net, rows: &Rows, columns: &[String], frame: &mut TableFrame, actions: &mut Vec<ViewAction>) {
        frame.body_view = body.ui_mut().clip_rect();
        if self.selection.row_drag.is_some() {
            scroll_near_edges(body.ui_mut());
        }
        body.rows(22.0, rows.len(), |mut row| {
            let i = row.index();
            let Some(Row { origin, value }) = rows.get(i) else { return };
            let is_selected = self.selection.selected.contains(&i);
            row.set_selected(is_selected);
            for c in columns {
                let cell = cell_in(value, c);
                let outline = is_selected && self.selection.selected_col.as_deref() == Some(c);
                let (_, resp) = row.col(|ui| {
                    if let Some(v) = cell {
                        cell_ui(ui, v);
                    }
                    // The cell's own painter: clipped to the table, so a
                    // row just past the bottom doesn't draw over the footer.
                    if outline {
                        ui.painter().rect_stroke(ui.max_rect().shrink(1.0), 0.0, egui::Stroke::new(1.0, theme::ACCENT), egui::StrokeKind::Inside);
                    }
                });
                if resp.drag_started_by(egui::PointerButton::Primary) {
                    frame.row_drag_start = Some((i, c.clone()));
                }
                if resp.double_clicked() {
                    frame.double = Some((i, c.clone()));
                } else if resp.clicked() {
                    frame.clicked = Some((i, c.clone(), frame.modifiers));
                } else if resp.secondary_clicked() && !is_selected {
                    frame.clicked = Some((i, c.clone(), Modifiers::NONE));
                }
                if resp.secondary_clicked() {
                    self.menu_gen = Some(self.rows_gen);
                }
                resp.context_menu(|ui| {
                    ui.set_min_width(230.0);
                    self.cell_menu(ui, net, CellCtx { row: i, origin, col: Some(c), path: &rows.path(i), cell }, actions);
                });
            }
            let (_, resp) = row.col(|_| {});
            if resp.drag_started_by(egui::PointerButton::Primary) {
                frame.row_drag_start = Some((i, String::new()));
            }
            if resp.clicked() {
                frame.clicked = Some((i, String::new(), frame.modifiers));
            }
            frame.row_spans.push((i, resp.rect.y_range()));
        });
    }

    /// After the table is drawn: what its header and rows collected this
    /// frame (sorting, column widths and moves, menus under the rows,
    /// clicks, the drag box, double-clicks).
    fn apply_table_input(&mut self, ui: &Ui, net: &mut Net, rows: &Rows, frame: TableFrame, actions: &mut Vec<ViewAction>) {
        let TableFrame { modifiers, depth, background, clicked, double, header_click, header_rects, drag_started, drag_stopped, mut row_drag_start, row_spans, body_view } = frame;
        if let Some((path, dir)) = header_click {
            self.sort_by(path, dir);
        }
        // Remember widths (by name) so a moved column keeps its own.
        if !ui.is_sizing_pass() {
            for (c, r) in &header_rects {
                match self.cols.widths.get_mut(c) {
                    Some(w) => *w = r.width(),
                    None => {
                        self.cols.widths.insert(c.clone(), r.width());
                    }
                }
            }
        }
        if let Some(c) = drag_started {
            self.cols.dragging = Some(c);
        }
        #[cfg(test)]
        {
            self.header_rects = header_rects.clone();
        }
        if let Some(from) = self.cols.dragging.clone() {
            // Drop before the first header whose middle is right of the pointer.
            let x = ui.ctx().pointer_latest_pos().map(|p| p.x);
            let before = x.and_then(|x| header_rects.iter().find(|(_, r)| r.center().x > x)).map(|(c, _)| c.clone());
            if drag_stopped || !ui.input(|i| i.pointer.any_down()) {
                self.cols.dragging = None;
                self.move_column(&from, before.as_deref());
            } else if let Some(first) = header_rects.first().map(|(_, r)| *r) {
                // Where it would land: a line between two headers.
                let line_x = match &before {
                    Some(b) => header_rects.iter().find(|(c, _)| c == b).map(|(_, r)| r.left()),
                    None => header_rects.last().map(|(_, r)| r.right()),
                };
                if let Some(lx) = line_x {
                    let painter = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new((self.id, "col-drop"))));
                    painter.line_segment([pos2(lx, first.top()), pos2(lx, first.bottom())], egui::Stroke::new(2.0, theme::ACCENT));
                    if let Some(p) = ui.ctx().pointer_latest_pos() {
                        painter.text(p + vec2(12.0, 8.0), Align2::LEFT_TOP, &from, egui::FontId::proportional(13.0), theme::TEXT);
                    }
                }
            }
        }
        if background.secondary_clicked() {
            self.menu_gen = Some(self.rows_gen);
        }
        background.context_menu(|ui| {
            ui.set_min_width(230.0);
            self.background_menu(ui, net, rows, actions);
        });
        if let Some((i, col, m)) = clicked {
            self.click_row(i, m);
            self.selection.selected_col = (!col.is_empty()).then_some(col);
        }
        let geometry = RowGeometry::from_spans(&row_spans);
        // From the empty space under the rows, too.
        if row_drag_start.is_none() && background.drag_started_by(egui::PointerButton::Primary) {
            row_drag_start = Some((usize::MAX, String::new()));
        }
        if let Some((i, col)) = row_drag_start
            && let (Some(p), Some(g)) = (ui.input(|i| i.pointer.press_origin()), geometry)
        {
            let kept = if modifiers.command { self.selection.selected.clone() } else { BTreeSet::new() };
            self.selection.row_drag = Some(RowDrag { start_x: p.x, start_y: p.y - g.top, kept });
            // ⇧-click extends from the row the drag began on (or the last row).
            self.selection.anchor = Some(i.min(rows.len().saturating_sub(1)));
            self.selection.selected_col = (!col.is_empty()).then_some(col);
        }
        self.drag_rows(ui, geometry, body_view, rows.len());
        if let Some((i, col)) = double {
            self.selection.selected = BTreeSet::from([i]);
            self.selection.selected_col = Some(col.clone());
            // Containers drill in; other cells open the document for editing.
            if (col == "(value)" || !self.step_into(i, &col, false))
                && let Some(a) = rows.get(i).and_then(|r| r.value.as_document()).filter(|_| depth == 0).and_then(|doc| self.edit(doc.clone()))
            {
                actions.push(a);
            }
        }
    }
}

/// The header row: each column's name and sort arrow; a click sorts by it
/// (ascending, descending, then unsorted), a drag moves it.
fn table_header(header: &mut TableRow<'_, '_>, columns: &[String], sorts: &[(Option<String>, Option<i32>)], header_sorted: bool, frame: &mut TableFrame) {
    for (c, (path, dir)) in columns.iter().zip(sorts) {
        let mut resp = None;
        let mut rect = egui::Rect::NOTHING;
        header.col(|ui| {
            // The whole cell, at the column's width (what
            // `col` returns is only the part used).
            rect = ui.max_rect();
            ui.add(egui::Label::new(RichText::new(c).color(theme::TEXT).strong()).selectable(false));
            if let Some(d) = dir {
                ui.label(RichText::new(if *d > 0 { icon::CARET_UP } else { icon::CARET_DOWN }).color(theme::ACCENT));
            }
            // Click sorts, drag moves the column. (The column
            // resize handles are added later, so they win at
            // the edges.)
            resp = Some(ui.interact(ui.max_rect(), ui.id().with(("header", c)), Sense::click_and_drag()));
        });
        frame.header_rects.push((c.clone(), rect));
        let Some(resp) = resp else { continue };
        if resp.drag_started() {
            frame.drag_started = Some(c.clone());
        }
        if resp.drag_stopped() {
            frame.drag_stopped = true;
        }
        let cursor = if resp.dragged() { egui::CursorIcon::Grabbing } else if path.is_some() { egui::CursorIcon::PointingHand } else { egui::CursorIcon::Grab };
        let resp = resp.on_hover_cursor(cursor);
        // Click: ascending, then descending, then unsorted.
        if let Some(p) = path
            && resp.clicked()
        {
            let next = match dir {
                None => Some(1),
                // The query's own sort, not a header's: turn it round.
                Some(d) if !header_sorted => Some(-*d),
                Some(d) if *d > 0 => Some(-1),
                Some(_) => None,
            };
            frame.header_click = Some((p.clone(), next));
        }
    }
    header.col(|_| {});
}

/// Where the table's rows are: the top of the first row (on screen or not)
/// and the distance from one row to the next.
#[derive(Clone, Copy)]
struct RowGeometry {
    top: f32,
    pitch: f32,
}

impl RowGeometry {
    /// From the rows drawn this frame (all the same height).
    fn from_spans(spans: &[(usize, egui::Rangef)]) -> Option<Self> {
        let (i0, r0) = spans.first()?;
        let pitch = match spans.get(1) {
            Some((i1, r1)) if i1 > i0 => (r1.min - r0.min) / (i1 - i0) as f32,
            _ => r0.span(),
        };
        (pitch > 0.0).then_some(Self { top: r0.min - *i0 as f32 * pitch, pitch })
    }
}

/// While dragging a selection, a pointer near or past the top or bottom of
/// the table's body scrolls it that way, faster the further out it is.
fn scroll_near_edges(ui: &mut Ui) {
    let Some(p) = ui.ctx().pointer_latest_pos() else { return };
    let view = ui.clip_rect();
    let edge = 10.0;
    let out = if p.y < view.top() + edge {
        view.top() + edge - p.y
    } else if p.y > view.bottom() - edge {
        -(p.y - (view.bottom() - edge))
    } else {
        return;
    };
    let dt = ui.input(|i| i.stable_dt).min(0.1);
    let speed = (out.abs() * 15.0).clamp(60.0, 2400.0) * out.signum();
    ui.scroll_with_delta_animation(vec2(0.0, speed * dt), egui::style::ScrollAnimation::none());
    ui.ctx().request_repaint();
}

fn cell_ui(ui: &mut Ui, v: &Bson) {
    let kind = format::kind(v);
    ui.label(RichText::new(theme::type_icon(kind)).color(theme::MUTED).size(12.0));
    let r = ui.add(egui::Label::new(RichText::new(format::cell_text(v)).color(theme::type_color(kind))).truncate().selectable(false));
    if let Bson::ObjectId(oid) = v {
        r.on_hover_text(format::object_id_details(oid));
    }
}
