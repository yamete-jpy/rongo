//! The JSON view: the page as shell text, only the lines in view laid out, and its find bar.

use std::sync::Arc;

use egui::{Color32, Key, RichText, Ui};
use egui_phosphor::regular as icon;
use rongo_core::format::Style;

use super::find::{MAX_MATCHES, Search};
use super::json_text::{Build, TextLines};
use super::{ResultView, thousands};
use crate::{highlight, shortcut, theme};

impl ResultView {
    pub(super) fn json(&mut self, ui: &mut Ui) {
        // The text is split into lines once and only the lines in view are
        // laid out, so a 50,000 document page scrolls as smoothly as 50.
        if self.json.lines.is_none() {
            match self.json.building.as_ref().map(|b| b.rx.try_recv()) {
                Some(Ok(text)) => {
                    self.json.lines = Some(Arc::new(text));
                    self.json.building = None;
                    // Matches are offsets into the old text: drop them
                    // before anything draws or scrolls with them.
                    if let Some(f) = &mut self.json.find {
                        f.searched = None;
                        f.running = None;
                        f.matches.clear();
                        f.current = 0;
                        f.scroll_to = false;
                    }
                }
                Some(Err(std::sync::mpsc::TryRecvError::Empty)) => {}
                Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => self.json.building = None,
                // Formatting tens of thousands of documents takes a while:
                // do it off the UI thread and show a spinner meanwhile. Not
                // while another page loads: these rows are on their way out.
                None if self.paging.loading.is_none() => {
                    self.json.building = Some(Build::start(self.rows.clone(), self.paging.offset as usize, Style::Display, ui.ctx().clone()));
                    #[cfg(test)]
                    {
                        self.json.builds += 1;
                    }
                }
                None => {}
            }
        }
        let Some(lines) = &self.json.lines else {
            ui.centered_and_justified(|ui| ui.spinner());
            return;
        };
        let font = theme::mono(13.0);
        let row_h = ui.fonts_mut(|f| f.row_height(&font)).round();
        let mut area = egui::ScrollArea::both().id_salt((self.id, "json")).auto_shrink([false, false]);
        if let Some(f) = &mut self.json.find
            && std::mem::take(&mut f.scroll_to)
            && let Some(&(line, at, _)) = f.matches.get(f.current)
        {
            area = area.vertical_scroll_offset((line as f32 * row_h - ui.available_height() / 3.0).max(0.0));
            let prefix = if line < lines.len() { lines.line(line).get(..at).unwrap_or_default() } else { "" };
            let x = ui.fonts_mut(|fo| fo.layout_no_wrap(prefix.to_string(), font.clone(), theme::TEXT).size().x);
            let w = ui.available_width();
            area = area.horizontal_scroll_offset(if x > w - 120.0 { x - w / 2.0 } else { 0.0 });
        }
        let find = self.json.find.as_ref().filter(|f| !f.matches.is_empty());
        let all = self.json.all;
        let out = line_view(ui, area, lines, &font, row_h, all, true, |i, job| {
            if let Some(f) = find {
                let first = f.matches.partition_point(|m| m.0 < i);
                let marks: Vec<_> = f.matches[first..]
                    .iter()
                    .enumerate()
                    .take_while(|(_, m)| m.0 == i)
                    .map(|(k, m)| (m.1..m.2, if first + k == f.current { CURRENT_MATCH } else { MATCH }))
                    .collect();
                highlight::mark(job, &marks);
            }
        });
        let (top, _drawn, _visible) = out.inner;
        self.json.top = top;
        #[cfg(test)]
        {
            self.json.fill = Some((_drawn, _visible));
        }
    }

    pub(super) fn find_id(&self) -> egui::Id {
        egui::Id::new((self.id, "find"))
    }

    /// The find bar over the JSON view: the text, match case, "n of m", and
    /// previous/next. ↩ and ⇧↩ step through matches, Esc closes it.
    pub(super) fn find_bar(&mut self, ui: &mut Ui) {
        let id = self.find_id();
        let Some(f) = &mut self.json.find else { return };
        let (mut step, mut close) = (0i64, false);
        ui.horizontal(|ui| {
            ui.label(RichText::new(icon::MAGNIFYING_GLASS).color(theme::MUTED));
            let r = ui.add(egui::TextEdit::singleline(&mut f.text).id(id).hint_text("Find in these documents").desired_width(280.0));
            if std::mem::take(&mut f.focus) {
                r.request_focus();
            }
            if r.lost_focus() {
                if ui.input(|i| i.key_pressed(Key::Enter)) {
                    step = if ui.input(|i| i.modifiers.shift) { -1 } else { 1 };
                    r.request_focus();
                } else if ui.input(|i| i.key_pressed(Key::Escape)) {
                    close = true;
                }
            }
            if ui.selectable_label(f.case, "Aa").on_hover_text("Match case").clicked() {
                f.case = !f.case;
            }
            let any = !f.matches.is_empty();
            if ui.add_enabled(any, egui::Button::new(icon::CARET_UP)).on_hover_text(shortcut::label("Previous match (⇧↩)")).clicked() {
                step = -1;
            }
            if ui.add_enabled(any, egui::Button::new(icon::CARET_DOWN)).on_hover_text(shortcut::label("Next match (↩)")).clicked() {
                step = 1;
            }
            if f.text.is_empty() {
            } else if self.json.lines.is_none() || f.running.is_some() {
                ui.weak("Searching…");
            } else if !any {
                ui.colored_label(theme::ERROR, "No matches");
            } else {
                let more = if f.matches.len() >= MAX_MATCHES { "+" } else { "" };
                ui.weak(format!("{} of {}{more}", thousands(f.current as u64 + 1), thousands(f.matches.len() as u64)));
            }
            if ui.button(icon::X).on_hover_text("Close (Esc)").clicked() {
                close = true;
            }
        });
        if close {
            self.json.find = None;
            return;
        }
        let key = (f.text.clone(), f.case);
        if let Some(lines) = &self.json.lines
            && f.searched.as_ref() != Some(&key)
            && f.running.as_ref().map(|r| &r.key) != Some(&key)
        {
            f.running = Some(Search::start(lines.clone(), key, ui.ctx().clone()));
        }
        if let Some(r) = &f.running
            && let Ok(matches) = r.rx.try_recv()
        {
            let key = f.running.take().map(|r| r.key.clone()).unwrap_or_default();
            f.matches = matches;
            f.current = f.matches.iter().position(|m| m.0 >= self.json.top).unwrap_or(0);
            f.scroll_to = !f.matches.is_empty();
            f.searched = Some(key);
        }
        if step != 0 && !f.matches.is_empty() {
            let n = f.matches.len() as i64;
            f.current = (f.current as i64 + step).rem_euclid(n) as usize;
            f.scroll_to = true;
        }
    }
}

/// Lines in a scroll area, as the JSON view and the shell's raw output show
/// them: only the lines in view are laid out, each exactly `row_h` tall,
/// selectable (a drag selects across lines). `all` marks every line
/// selected, for a select-all (⌘A) the caller copies itself: lines off
/// screen aren't laid out, so egui can't select them. `coloured` colours
/// the lines as JavaScript; `mark` adds marks to line `i`'s job. Its inner
/// value: the first line in view, and the bottoms of the last line drawn
/// and of the view.
#[allow(clippy::too_many_arguments)]
pub(crate) fn line_view(
    ui: &mut Ui,
    area: egui::ScrollArea,
    lines: &TextLines,
    font: &egui::FontId,
    row_h: f32,
    all: bool,
    coloured: bool,
    mut mark: impl FnMut(usize, &mut egui::text::LayoutJob),
) -> egui::scroll_area::ScrollAreaOutput<(usize, f32, f32)> {
    // show_rows sizes rows as row_h plus this spacing, so it must be zero
    // here, not just inside, or the rows drawn fall short of the bottom.
    ui.spacing_mut().item_spacing.y = 0.0;
    area.show_rows(ui, row_h, lines.len(), |ui, range| {
        let top = range.start;
        for i in range {
            let line = lines.line(i);
            // A blank line still needs a glyph to get a height.
            let text = if line.is_empty() { " " } else { line };
            let mut job = if coloured { highlight::layout_job(text, font.clone(), f32::INFINITY) } else { egui::text::LayoutJob::simple_singleline(text.to_owned(), font.clone(), theme::TEXT) };
            // A fixed line height, so every row is exactly row_h even when a
            // fallback font (Thai, emoji) is taller.
            for s in &mut job.sections {
                s.format.line_height = Some(row_h);
            }
            if all {
                highlight::mark(&mut job, &[(0..text.len(), theme::SELECTION)]);
            }
            mark(i, &mut job);
            ui.add(egui::Label::new(job).extend().selectable(true));
        }
        (top, ui.min_rect().bottom(), ui.clip_rect().bottom())
    })
}

const MATCH: Color32 = Color32::from_rgba_premultiplied(0x5a, 0x4a, 0x14, 0xa0);
const CURRENT_MATCH: Color32 = Color32::from_rgb(0xb0, 0x7a, 0x10);
