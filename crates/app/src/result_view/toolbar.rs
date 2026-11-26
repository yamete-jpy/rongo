//! Around the rows: the toolbar (paging, document buttons, view mode), the drill breadcrumb and the status bar.

use egui::{Align, Layout, RichText, Sense, Ui, vec2};
use egui_phosphor::regular as icon;
use rongo_core::Command;

use super::drill::Step;
use super::menus::locked_why;
use super::{MAX_PAGE_SIZE, Mode, PAGE_SIZES, ResultView, Source, ViewAction};
use crate::net::{Net, Origin, ViewReq};
use crate::{shortcut, theme};

impl ResultView {
    /// Paging, document buttons and the view mode, on one row when they fit.
    /// When the view is too narrow, a group that doesn't fit starts a new
    /// row, so the buttons wrap instead of running off the edge.
    pub(super) fn toolbar(&mut self, ui: &mut Ui, net: &mut Net, actions: &mut Vec<ViewAction>) {
        // Each group's width comes from the last time it was drawn; when one
        // changes, this pass is discarded and redrawn with the new widths.
        let w = self.toolbar_widths;
        // Item spacing either side of a separator.
        let gap = ui.spacing().item_spacing.x * 2.0 + 6.0;
        let rows = toolbar_rows(w, gap, ui.available_width());
        let mut measured = [0.0; 3];
        for r in 0..=rows[1].max(rows[2]) {
            ui.horizontal(|ui| {
                if rows[0] == r {
                    // Wraps inside itself when even alone it is too wide:
                    // a row that doesn't fit widens the whole view, and the
                    // right-aligned parts go past the window's edge.
                    measured[0] = ui.horizontal_wrapped(|ui| self.paging(ui, net)).response.rect.width();
                }
                if rows[1] == r {
                    if rows[0] == r {
                        ui.separator();
                    }
                    measured[1] = ui.horizontal(|ui| self.document_buttons(ui, actions)).response.rect.width();
                }
                if rows[2] == r {
                    measured[2] = ui
                        .with_layout(Layout::right_to_left(Align::Center), |ui| {
                            self.mode_picker(ui);
                            ui.min_rect().width()
                        })
                        .inner;
                }
            });
        }
        if measured.iter().zip(w).any(|(m, w)| (m - w).abs() > 0.5) {
            self.toolbar_widths = measured;
            ui.ctx().request_discard("result view toolbar widths changed");
        }
    }

    fn paging(&mut self, ui: &mut Ui, net: &mut Net) {
        let loading = self.paging.loading.is_some();
        if ui.add_enabled(!loading, egui::Button::new(icon::ARROWS_CLOCKWISE)).on_hover_text(shortcut::label("Refresh view (⌘R)")).clicked() {
            self.fetch(net);
        }
        if loading
            && ui.button(icon::STOP).on_hover_text("Cancel").clicked()
            && let Some(id) = self.paging.loading.take()
        {
            net.send(Origin::None, None, Command::Cancel(id));
        }
        ui.separator();
        let at_start = self.paging.offset == 0;
        let page = self.paging.page_size as u64;
        if ui.add_enabled(!at_start && !loading, egui::Button::new(icon::CARET_LINE_LEFT)).on_hover_text("First page").clicked() {
            self.paging.offset = 0;
            self.fetch(net);
        }
        if ui.add_enabled(!at_start && !loading, egui::Button::new(icon::CARET_LEFT)).on_hover_text("Previous page").clicked() {
            self.paging.offset = self.paging.offset.saturating_sub(page);
            self.fetch(net);
        }
        if ui.add_enabled(self.paging.has_more && !loading, egui::Button::new(icon::CARET_RIGHT)).on_hover_text("Next page").clicked() {
            self.paging.offset += page;
            self.fetch(net);
        }
        let last_hint = if self.paging.total.is_some() { "Last page" } else { "Last page (counts the documents first)" };
        if ui.add_enabled(self.can_go_last(), egui::Button::new(icon::CARET_LINE_RIGHT)).on_hover_text(last_hint).clicked() {
            self.go_last(net);
        }
        // Page size: type any number (Enter applies) or pick one from ▾.
        let mut size = self.paging.page_size;
        let field_id = egui::Id::new((self.id, "page-size"));
        if !ui.memory(|m| m.has_focus(field_id)) {
            self.paging.page_text = self.paging.page_size.to_string();
        }
        // One frame in the text-field look: the number, then ▾ inside it.
        let visuals = ui.visuals().clone();
        let focused = ui.memory(|m| m.has_focus(field_id));
        let stroke = if focused { visuals.selection.stroke } else { visuals.widgets.inactive.bg_stroke };
        let height = ui.spacing().interact_size.y;
        let mut field = None;
        egui::Frame::new().fill(visuals.extreme_bg_color).stroke(stroke).corner_radius(visuals.widgets.inactive.corner_radius).inner_margin(egui::Margin::symmetric(2, 0)).show(ui, |ui| {
            let inner = height - 2.0 * stroke.width;
            ui.set_min_height(inner);
            ui.spacing_mut().interact_size.y = inner;
            ui.spacing_mut().button_padding.y = 0.0;
            ui.spacing_mut().item_spacing.x = 0.0;
            field = Some(
                ui.add(egui::TextEdit::singleline(&mut self.paging.page_text).id(field_id).frame(egui::Frame::NONE).margin(egui::Margin::symmetric(4, 2)).desired_width(36.0).horizontal_align(Align::RIGHT))
                    .on_hover_text(format!("Documents per page (1–{MAX_PAGE_SIZE}); press Enter to apply")),
            );
            // A gap, then ▾ on a small raised square of its own, like
            // Studio 3T's "200 [▾]". (Right-aligned, the number ends at the
            // field's edge, so the whole gap is this space.)
            ui.add_space(6.0);
            let side = inner - 4.0;
            let (rect, caret) = ui.allocate_exact_size(egui::vec2(side, side), egui::Sense::click());
            let v = ui.style().interact(&caret);
            ui.painter().rect(rect, 3.0, v.weak_bg_fill, v.bg_stroke, egui::StrokeKind::Inside);
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, icon::CARET_DOWN, egui::FontId::proportional(11.0), v.text_color());
            let caret = caret.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Page sizes");
            egui::Popup::menu(&caret).show(|ui| {
                for s in PAGE_SIZES {
                    if ui.selectable_label(s == self.paging.page_size, s.to_string()).clicked() {
                        size = s;
                        ui.close();
                    }
                }
            });
        });
        let field = field.expect("the page size field is drawn");
        self.paging.page_text.retain(|c| c.is_ascii_digit());
        if field.lost_focus() {
            size = parse_page_size(&self.paging.page_text, self.paging.page_size);
            self.paging.page_text = size.to_string();
        }
        if size != self.paging.page_size {
            self.paging.page_size = size;
            self.paging.offset = self.paging.offset / size as u64 * size as u64;
            self.fetch(net);
        }
        let shown = self.rows.len() as u64;
        // Whole: on a narrow view it moves to the next line rather than
        // breaking after "Documents".
        let range = if shown == 0 { "No documents".to_string() } else { format!("Documents {} to {}", self.paging.offset + 1, self.paging.offset + shown) };
        ui.add(egui::Label::new(range).wrap_mode(egui::TextWrapMode::Extend));
    }

    fn document_buttons(&mut self, ui: &mut Ui, actions: &mut Vec<ViewAction>) {
        // Every frame: look at the documents only for a single selection
        // (⌘A on 50,000 rows would otherwise parse them all), and copy it
        // only for the button pressed.
        let one = self.single_selected();
        let any = !self.selection.selected.is_empty();
        let can = self.editable().is_some();
        // Why a button is off: a locked connection says so.
        let locked = self.read_only;
        if locked_why(locked, ui.add_enabled(can, egui::Button::new(icon::FILE_PLUS)).on_hover_text(shortcut::label("Insert document (⌘D)"))).clicked()
            && let Some(a) = self.insert()
        {
            actions.push(a);
        }
        if locked_why(locked, ui.add_enabled(can && one.is_some(), egui::Button::new(icon::PENCIL_SIMPLE)).on_hover_text(shortcut::label("Edit document (⌘J)"))).clicked()
            && let Some(a) = one.cloned().and_then(|doc| self.edit(doc))
        {
            actions.push(a);
        }
        if locked_why(locked, ui.add_enabled(can && one.is_some(), egui::Button::new(icon::COPY)).on_hover_text("Clone document")).clicked()
            && let Some(a) = one.cloned().and_then(|doc| self.clone_doc(doc))
        {
            actions.push(a);
        }
        if locked_why(locked, ui.add_enabled(can, egui::Button::new(icon::PENCIL_LINE)).on_hover_text(shortcut::label("Update dialog (⌘U)"))).clicked()
            && let Some(a) = self.bulk(false, self.selected_ids(), self.selected_field())
        {
            actions.push(a);
        }
        let del = egui::Button::new(RichText::new(icon::TRASH).color(if any { theme::ERROR } else { theme::MUTED }));
        if locked_why(locked, ui.add_enabled(can && any, del).on_hover_text(shortcut::label("Delete selected documents (⇧⌫)"))).clicked()
            && let Some(a) = self.delete(self.selected_ids())
        {
            actions.push(a);
        }
        if locked_why(locked, ui.add_enabled(can, egui::Button::new(icon::BROOM)).on_hover_text("Delete dialog (F8)")).clicked()
            && let Some(a) = self.bulk(true, self.selected_ids(), None)
        {
            actions.push(a);
        }
    }

    /// Table, Tree or JSON. A page on its way shows in the status bar, not here.
    fn mode_picker(&mut self, ui: &mut Ui) {
        egui::ComboBox::from_id_salt((self.id, "mode")).width(110.0).selected_text(self.mode.label()).show_ui(ui, |ui| {
            for m in [Mode::Table, Mode::Tree, Mode::Json] {
                ui.selectable_value(&mut self.mode, m, m.label());
            }
        });
    }

    /// The breadcrumb's parts, as Studio 3T's: the title, each drill step,
    /// then the field of the selected cell (the Table view's, when it is a
    /// field's: not `(value)` or `DOC_ID`). The title is always strong, and
    /// otherwise the last part is.
    fn crumbs(&self) -> Vec<Crumb> {
        let n = self.drill.len();
        let mut crumbs = vec![Crumb { text: self.title.clone(), back_to: (n > 0).then_some(0), strong: true }];
        for (i, step) in self.drill.iter().enumerate() {
            let text = match step {
                Step::Field(f) => f.clone(),
                Step::Elements { row, field, .. } => format!("{field} [row {}]", row + 1),
                Step::Cell { row, field, .. } => format!("{field} (row {})", row + 1),
            };
            // The current level's step is where you are: no link.
            crumbs.push(Crumb { text, back_to: (i + 1 < n).then_some(i + 1), strong: i + 1 == n });
        }
        let field = (self.mode == Mode::Table && self.current_row().is_some()).then(|| self.selected_column()).flatten().filter(|c| *c != "(value)" && !self.is_doc_id(c));
        if let Some(c) = field {
            if n > 0
                && let Some(last) = crumbs.last_mut()
            {
                last.strong = false;
            }
            crumbs.push(Crumb { text: c.to_string(), back_to: None, strong: true });
        }
        crumbs
    }

    /// The breadcrumb: `crumbs`, with carets between them. Its row has a
    /// fixed height and the view's width: a long path or field name is cut
    /// short there, never widening the view (which would push the status
    /// bar's right-hand items off screen) nor moving the rows down.
    pub(super) fn breadcrumb(&mut self, ui: &mut Ui) {
        // The level's columns, which the Table view works out next anyway:
        // the selected cell's field is named only while it is one of them.
        if self.mode == Mode::Table && self.cols.cache.is_none() {
            let data = self.rows.clone();
            let rows = super::drill::Rows::new(&data, self.level());
            self.columns(&rows);
        }
        let height = ui.spacing().interact_size.y.max(ui.text_style_height(&egui::TextStyle::Body));
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
        let mut row = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(Layout::left_to_right(Align::Center)));
        row.set_clip_rect(rect.intersect(ui.clip_rect()));
        let mut cut = None;
        for (i, crumb) in self.crumbs().into_iter().enumerate() {
            if i > 0 {
                row.add(egui::Label::new(RichText::new(icon::CARET_RIGHT).color(theme::MUTED)).wrap_mode(egui::TextWrapMode::Extend));
            }
            let text = if crumb.strong { RichText::new(crumb.text).strong() } else { RichText::new(crumb.text) };
            match crumb.back_to {
                Some(depth) => {
                    let link = row.link(text);
                    let link = if depth == 0 { link.on_hover_text("Back to the top level (Esc steps out one level)") } else { link };
                    if link.clicked() {
                        cut = Some(depth);
                    }
                }
                None => {
                    row.add(egui::Label::new(text).truncate());
                }
            }
        }
        if let Some(c) = cut {
            let mut d = self.drill.clone();
            d.truncate(c);
            self.set_drill(d);
        }
    }

    pub(super) fn status_bar(&mut self, ui: &mut Ui, net: &mut Net) {
        ui.separator();
        ui.horizontal(|ui| {
            // What this view is waiting for, once it has taken more than a
            // moment: the query, a count or a document refresh.
            let now = std::time::Instant::now();
            let waiting = [self.paging.loading, self.paging.counting]
                .into_iter()
                .flatten()
                .chain(self.refreshing.iter().map(|(r, _)| *r))
                .filter_map(|id| net.pending.get(&id).map(|s| (id, s.what.clone(), s.started)))
                .min_by_key(|(_, _, t)| *t);
            let shown = waiting.clone().filter(|(_, _, t)| now.saturating_duration_since(*t) >= theme::SHOW_BUSY_AFTER);
            match (&shown, &waiting) {
                (Some(_), _) => ui.ctx().request_repaint_after(std::time::Duration::from_millis(33)),
                (None, Some((_, _, t))) => ui.ctx().request_repaint_after((*t + theme::SHOW_BUSY_AFTER).saturating_duration_since(now)),
                _ => {}
            }
            let mut discard = None;
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(ms) = self.paging.elapsed_ms {
                    ui.label(format!("{}  {}", icon::TIMER, elapsed_text(ms)));
                    ui.separator();
                }
                match (&self.source, self.paging.total, self.paging.counting) {
                    (_, _, Some(_)) if shown.is_none() => {
                        ui.spinner();
                    }
                    (_, _, Some(_)) => {}
                    (Source::Find { conn, spec, .. }, total, None) => {
                        let label = match total {
                            Some(t) => format!("{}  {} documents", icon::HASH, thousands(t)),
                            None => format!("{}  Count Documents", icon::HASH),
                        };
                        if ui.button(label).clicked() {
                            let cmd = Command::Count { ns: spec.ns.clone(), filter: spec.filter.clone() };
                            self.paging.counting = Some(net.send(Origin::View(self.id, ViewReq::Count), Some(*conn), cmd));
                        }
                    }
                    (Source::Static { .. } | Source::Raw { .. }, Some(t), None) => {
                        ui.label(format!("{}  {} documents", icon::HASH, thousands(t)));
                    }
                    _ => {}
                }
                // Right to left: drawn as ⊗ ▬▬ Running find on db.c… 4s.
                if let Some((id, what, started)) = &shown {
                    ui.separator();
                    ui.label(RichText::new(theme::elapsed(now.saturating_duration_since(*started))).color(theme::MUTED));
                    ui.label(format!("{}…", what.as_deref().unwrap_or("Working")));
                    theme::busy_bar(ui, 80.0);
                    if theme::discard_button(ui) {
                        discard = Some(*id);
                    }
                }
                // The notes on the left get what the right side leaves, cut
                // short rather than drawn under its buttons.
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    let selected = match self.selection.selected.len() {
                        0 => RichText::new("0 documents selected").weak(),
                        1 => RichText::new("1 document selected"),
                        n => RichText::new(format!("{n} documents selected")),
                    };
                    ui.add(egui::Label::new(selected).truncate());
                    if self.json.copy.is_some() {
                        ui.separator();
                        ui.spinner();
                        ui.add(egui::Label::new("Copying…").truncate());
                    }
                    let hidden = self.hidden().len();
                    if hidden > 0 {
                        ui.separator();
                        if ui.link(format!("{} hidden column{} — show", hidden, if hidden == 1 { "" } else { "s" })).clicked() {
                            self.hidden_mut().clear();
                        }
                    }
                    if let Source::Static { truncated: true, .. } | Source::Raw { truncated: true, .. } = &self.source {
                        let note = format!("{} showing the first {} documents", icon::WARNING, self.paging.total.unwrap_or(0));
                        ui.add(egui::Label::new(RichText::new(note).color(theme::WARN)).truncate());
                    }
                });
            });
            if let Some(id) = discard {
                net.send(Origin::None, None, Command::Cancel(id));
                self.on_cancelled(id);
            }
        });
    }
}

/// One part of the breadcrumb.
struct Crumb {
    text: String,
    /// The drill depth a click on it steps back out to (a link), if any.
    back_to: Option<usize>,
    strong: bool,
}

/// The row each toolbar group (paging, document buttons, view mode) goes
/// on: the first of these layouts that fits.
///   one row:    paging | buttons      mode
///   two rows:   paging                mode
///               buttons
///   two rows:   paging
///               buttons               mode
///   three rows, one group each.
fn toolbar_rows([paging, buttons, mode]: [f32; 3], gap: f32, available: f32) -> [usize; 3] {
    let fits = |w: f32| w <= available;
    if fits(paging + gap + buttons + gap + mode) {
        [0, 0, 0]
    } else if fits(paging + gap + mode) && fits(buttons) {
        [0, 1, 0]
    } else if fits(buttons + gap + mode) {
        [0, 1, 1]
    } else {
        [0, 1, 2]
    }
}

/// A typed page size, capped at the maximum; anything else keeps `current`.
pub(super) fn parse_page_size(text: &str, current: i64) -> i64 {
    match text.trim().parse::<i64>() {
        Ok(n) if n > 0 => n.min(MAX_PAGE_SIZE),
        _ => current,
    }
}

pub(super) fn elapsed_text(ms: u64) -> String {
    format!("{:02}:{:02}:{:02}.{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}
