//! The shell's code editor: a monospace `TextEdit` with JavaScript colouring,
//! a line-number gutter, the current line highlighted, an error marker, and an
//! autocomplete popup.

use egui::text::{CCursor, CCursorRange};
use egui::{Align2, Area, Frame, Id, Key, Modifiers, Order, Rect, Sense, Shape, TextEdit, Ui, pos2, vec2};
use rongo_shell::complete::{self, Completion, Kind};

use crate::{highlight, theme};

pub struct CodeEditor {
    pub text: String,
    id: Id,
    completion: Option<Popup>,
    /// Cursor position (chars) as of the last frame.
    cursor: usize,
    selection: Option<(usize, usize)>,
    /// 1-based line to mark as failing.
    pub error_line: Option<u32>,
    /// Collection whose field names the open popup is waiting for.
    awaiting_fields: Option<String>,
}

struct Popup {
    start_byte: usize,
    items: Vec<Completion>,
    selected: usize,
}

/// Code to run, and which editor line its first line is.
pub struct RunRequest {
    pub code: String,
    pub line_offset: u32,
}

#[derive(Default)]
pub struct EditorOutput {
    pub run: Option<RunRequest>,
}

impl CodeEditor {
    pub fn new(id: Id, text: impl Into<String>) -> Self {
        Self { text: text.into(), id, completion: None, cursor: 0, selection: None, error_line: None, awaiting_fields: None }
    }

    /// Replace the whole text (Load file, New). The old cursor and
    /// selection don't belong to it: Run would run a slice of the new text.
    pub fn set_text(&mut self, text: String) {
        self.text = text;
        self.cursor = 0;
        self.selection = None;
        self.completion = None;
        self.error_line = None;
    }

    /// 1-based line and column of the cursor.
    pub fn line_col(&self) -> (usize, usize) {
        let before: String = self.text.chars().take(self.cursor).collect();
        let line = before.matches('\n').count() + 1;
        let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
        (line, col)
    }

    /// The selection, or everything when nothing is selected.
    pub fn run_all(&self) -> RunRequest {
        match self.selection {
            Some((a, b)) if a != b => {
                let start = char_to_byte(&self.text, a);
                let end = char_to_byte(&self.text, b);
                let line_offset = self.text[..start].matches('\n').count() as u32;
                RunRequest { code: self.text[start..end].to_string(), line_offset }
            }
            _ => RunRequest { code: self.text.clone(), line_offset: 0 },
        }
    }

    /// The paragraph (lines between blank lines) around the cursor.
    pub fn run_current(&self) -> RunRequest {
        let lines: Vec<&str> = self.text.split('\n').collect();
        let (line, _) = self.line_col();
        let mut cur = (line - 1).min(lines.len().saturating_sub(1));
        // On a blank line, use the statement just above it.
        while cur > 0 && lines[cur].trim().is_empty() {
            cur -= 1;
        }
        let mut first = cur;
        while first > 0 && !lines[first - 1].trim().is_empty() {
            first -= 1;
        }
        let mut last = cur;
        while last + 1 < lines.len() && !lines[last + 1].trim().is_empty() {
            last += 1;
        }
        RunRequest { code: lines[first..=last].join("\n"), line_offset: first as u32 }
    }

    /// `fields` returns a collection's field names when they are known.
    pub fn ui(&mut self, ui: &mut Ui, collections: &[String], fields: &dyn Fn(&str) -> Option<Vec<String>>) -> EditorOutput {
        let mut out = EditorOutput::default();
        let has_focus = ui.memory(|m| m.has_focus(self.id));
        // Field names arrived while the popup was open: refresh it.
        if let Some(coll) = &self.awaiting_fields
            && fields(coll).is_some()
        {
            self.awaiting_fields = None;
            if self.completion.is_some() {
                self.refresh_completion(collections, fields, false);
            }
        }

        // Keys that must not reach the TextEdit.
        if has_focus {
            if ui.input_mut(|i| i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Enter)) {
                out.run = Some(self.run_current());
            } else if ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter)) || ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::F5)) {
                out.run = Some(self.run_all());
            }
        }
        let mut accept = None;
        if has_focus && let Some(p) = &mut self.completion {
            if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowDown)) {
                p.selected = (p.selected + 1) % p.items.len();
            }
            if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::ArrowUp)) {
                p.selected = (p.selected + p.items.len() - 1) % p.items.len();
            }
            if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter) || i.consume_key(Modifiers::NONE, Key::Tab)) {
                accept = Some(p.selected);
            }
            if ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
                self.completion = None;
            }
        }
        let force_complete = has_focus && ui.input_mut(|i| i.consume_key(Modifiers::CTRL, Key::Space));
        if let Some(i) = accept {
            self.accept_completion(ui.ctx(), i);
        }

        let font = theme::mono(13.5);
        let row_height = ui.fonts_mut(|f| f.row_height(&font));
        let char_w = ui.fonts_mut(|f| f.glyph_width(&font, '0'));
        let line_count = self.text.split('\n').count().max(1);
        let gutter_w = (line_count.to_string().len().max(2) as f32) * char_w + 18.0;

        let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, wrap: f32| {
            let job = highlight::layout_job(buf.as_str(), theme::mono(13.5), wrap);
            ui.fonts_mut(|f| f.layout_job(job))
        };

        let scroll = egui::ScrollArea::both().id_salt(self.id.with("scroll")).auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                let (gutter, _) = ui.allocate_exact_size(vec2(gutter_w, row_height), Sense::hover());
                let background = ui.painter().add(Shape::Noop);
                let output = TextEdit::multiline(&mut self.text)
                    .id(self.id)
                    .font(font.clone())
                    .code_editor()
                    .frame(egui::Frame::NONE)
                    .lock_focus(true)
                    .desired_width(f32::INFINITY)
                    .desired_rows((ui.available_height() / row_height).max(4.0) as usize)
                    .layouter(&mut layouter)
                    .show(ui);

                if let Some(range) = output.cursor_range {
                    self.cursor = range.primary.index.0;
                    let (a, b) = (range.primary.index.0, range.secondary.index.0);
                    self.selection = Some((a.min(b), a.max(b)));
                }

                // Gutter numbers, current line and error line.
                let painter = ui.painter();
                let full_width = ui.clip_rect().right() - gutter.left();
                let (cur_line, _) = self.line_col();
                let mut line = 1usize;
                let mut starts_line = true;
                let mut highlight = Vec::new();
                for row in &output.galley.rows {
                    let y = output.galley_pos.y + row.min_y();
                    if starts_line {
                        let color = if line == cur_line { theme::TEXT } else { theme::MUTED };
                        painter.text(
                            pos2(gutter.right() - 10.0, y),
                            Align2::RIGHT_TOP,
                            line.to_string(),
                            theme::mono(12.0),
                            color,
                        );
                    }
                    let row_rect = Rect::from_min_size(pos2(gutter.left(), y), vec2(full_width, row.max_y() - row.min_y()));
                    if self.error_line == Some(line as u32) {
                        highlight.push(Shape::rect_filled(row_rect, 0.0, theme::ERROR.gamma_multiply(0.18)));
                    } else if line == cur_line && has_focus {
                        highlight.push(Shape::rect_filled(row_rect, 0.0, theme::CURRENT_LINE));
                    }
                    starts_line = row.ends_with_newline;
                    if row.ends_with_newline {
                        line += 1;
                    }
                }
                painter.set(background, Shape::Vec(highlight));

                // Autocomplete: open or refresh after typing.
                if output.response.changed() || force_complete {
                    self.refresh_completion(collections, fields, force_complete);
                }
                if output.response.clicked() || !has_focus {
                    self.completion = None;
                }
                output.galley.pos_from_cursor(CCursor::new(self.cursor)).translate(output.galley_pos.to_vec2())
            })
            .inner
        });

        if let Some(p) = &self.completion
            && has_focus
        {
            let anchor = scroll.inner.left_bottom() + vec2(0.0, 2.0);
            let mut clicked = None;
            let selected = p.selected;
            // No fade-in: suggestions should be there the moment you type.
            Area::new(self.id.with("completion")).order(Order::Foreground).fixed_pos(anchor).fade_in(false).show(ui.ctx(), |ui| {
                Frame::popup(ui.style()).inner_margin(4.0).show(ui, |ui| {
                    ui.set_min_width(300.0);
                    egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                        // The chosen row's background is painted after the rows
                        // are laid out, so it spans the list's full width and is
                        // solid (selectable()'s own is faint on this popup).
                        let background = ui.painter().add(Shape::Noop);
                        let mut chosen = None;
                        for (i, item) in p.items.iter().enumerate() {
                            let r = ui.add(egui::Button::selectable(false, suggestion_label(item)).frame_when_inactive(false));
                            if r.clicked() {
                                clicked = Some(i);
                            }
                            if i == selected {
                                r.scroll_to_me(None);
                                chosen = Some(r.rect);
                            }
                        }
                        if let Some(rect) = chosen {
                            let width = ui.min_rect().x_range();
                            ui.painter().set(background, Shape::rect_filled(Rect::from_x_y_ranges(width, rect.y_range()), 3.0, theme::SELECTION));
                        }
                    });
                });
            });
            if let Some(i) = clicked {
                self.accept_completion(ui.ctx(), i);
                ui.memory_mut(|m| m.request_focus(self.id));
            }
        }
        out
    }

    fn refresh_completion(&mut self, collections: &[String], fields: &dyn Fn(&str) -> Option<Vec<String>>, forced: bool) {
        let cursor_byte = char_to_byte(&self.text, self.cursor);
        let before = &self.text[..cursor_byte];
        let last = before.chars().last();
        let typing = last.is_some_and(|c| matches!(c, '.' | '_' | '$' | '"' | '\'') || c.is_alphanumeric());
        if !typing && !forced {
            self.completion = None;
            return;
        }
        let c = complete::complete(before, collections, fields);
        self.awaiting_fields = c.needs_fields;
        self.completion = (!c.items.is_empty()).then_some(Popup { start_byte: c.start, items: c.items, selected: 0 });
    }

    /// Field names autocomplete is waiting for (reported once per request).
    pub fn wanted_fields(&self) -> Option<&str> {
        self.awaiting_fields.as_deref()
    }

    fn accept_completion(&mut self, ctx: &egui::Context, index: usize) {
        let Some(p) = self.completion.take() else { return };
        let Some(item) = p.items.get(index) else { return };
        let cursor_byte = char_to_byte(&self.text, self.cursor);
        if p.start_byte > cursor_byte {
            return;
        }
        self.text.replace_range(p.start_byte..cursor_byte, &item.insert);
        let new_cursor = self.text[..p.start_byte + item.insert.len()].chars().count();
        self.set_cursor(ctx, new_cursor);
    }

    pub fn set_cursor(&mut self, ctx: &egui::Context, char_index: usize) {
        self.cursor = char_index;
        if let Some(mut state) = TextEdit::load_state(ctx, self.id) {
            state.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(char_index))));
            state.store(ctx, self.id);
        }
    }
}

/// A popup row: a kind letter (as Studio 3T shows f / o), then the label
/// with the typed part highlighted.
fn suggestion_label(item: &Completion) -> egui::text::LayoutJob {
    let (letter, color) = match item.kind {
        Kind::Field => ("f", theme::CONTAINER),
        Kind::Operator => ("o", theme::NUMBER),
        Kind::Collection => ("c", theme::STRING),
        Kind::Method => ("m", theme::FUNCTION),
        Kind::Global => ("g", theme::DATE),
        Kind::Keyword => ("k", theme::MUTED),
    };
    let font = theme::mono(13.0);
    let fmt = |c| egui::TextFormat::simple(font.clone(), c);
    let mut job = egui::text::LayoutJob::default();
    job.append(letter, 0.0, fmt(color));
    job.append("  ", 0.0, fmt(theme::TEXT));
    match item.matched {
        Some((a, b)) if b <= item.label.len() && item.label.is_char_boundary(a) && item.label.is_char_boundary(b) => {
            job.append(&item.label[..a], 0.0, fmt(theme::TEXT));
            job.append(&item.label[a..b], 0.0, fmt(theme::ACCENT));
            job.append(&item.label[b..], 0.0, fmt(theme::TEXT));
        }
        _ => job.append(&item.label, 0.0, fmt(theme::TEXT)),
    }
    job
}

fn char_to_byte(s: &str, chars: usize) -> usize {
    s.char_indices().nth(chars).map_or(s.len(), |(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Typing opens the suggestions; ↓ moves to the next one, and the
    /// chosen one is drawn highlighted (it used to move invisibly).
    #[test]
    fn arrow_down_moves_a_visible_highlight() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut e = CodeEditor::new(egui::Id::new("ed"), "db.getCollection(\"p\").find({})");
        e.cursor = 30;
        let fields = |_: &str| Some(vec!["brand".to_string(), "createdTime".to_string(), "password".to_string()]);
        let frame = |e: &mut CodeEditor, events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 500.0))),
                events,
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| {
                ui.memory_mut(|m| m.request_focus(e.id));
                e.ui(ui, &[], &fields);
            });
            out.textures_delta.clear();
            out.shapes
        };
        frame(&mut e, vec![]);
        frame(&mut e, vec![egui::Event::Text("a".into())]);
        frame(&mut e, vec![]);
        assert!(e.completion.as_ref().is_some_and(|p| p.items.len() >= 2), "suggestions opened");
        let down = egui::Event::Key { key: Key::ArrowDown, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE };
        frame(&mut e, vec![down]);
        let shapes = frame(&mut e, vec![]);
        assert_eq!(e.completion.as_ref().unwrap().selected, 1);
        let highlighted = shapes.iter().any(|c| matches!(&c.shape, egui::epaint::Shape::Rect(r) if r.fill == theme::SELECTION));
        assert!(highlighted, "the chosen suggestion has the selection background");
    }

    /// After the text is replaced (Load file), Run runs all of it, not the
    /// old selection's range of the new text.
    #[test]
    fn replacing_the_text_forgets_the_selection() {
        let mut e = CodeEditor::new(egui::Id::new("e"), "db.a.find()\ndb.b.find()");
        e.selection = Some((0, 11));
        assert_eq!(e.run_all().code, "db.a.find()");
        e.set_text("print(1)\nprint(2)".into());
        assert_eq!(e.run_all().code, "print(1)\nprint(2)");
    }

    fn editor(text: &str, cursor: usize) -> CodeEditor {
        let mut e = CodeEditor::new(Id::new("t"), text);
        e.cursor = cursor;
        e
    }

    #[test]
    fn current_statement_is_the_paragraph() {
        let text = "a()\nb()\n\nc()\nd()\n\n";
        let r = editor(text, 9).run_current();
        assert_eq!((r.code.as_str(), r.line_offset), ("c()\nd()", 3));
        let r = editor(text, 0).run_current();
        assert_eq!((r.code.as_str(), r.line_offset), ("a()\nb()", 0));
        // A blank line after a statement runs that statement.
        let r = editor(text, text.len()).run_current();
        assert_eq!(r.code, "c()\nd()");
    }

    /// Typing inside a query opens the popup on its own, with the
    /// collection's fields first and operators after, like Studio 3T.
    #[test]
    fn typing_in_a_query_suggests_fields_and_operators() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let text = "db.getCollection(\"launch_domain_daily\").find({})";
        let mut e = CodeEditor::new(Id::new("ed"), text);
        let fields = |c: &str| (c == "launch_domain_daily").then(|| vec!["launches".to_string(), "embeddable".to_string(), "expireAt".to_string()]);
        let frame = |e: &mut CodeEditor, events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 500.0))),
                events,
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| {
                e.ui(ui, &[], &fields);
            });
            out.textures_delta.clear();
        };
        frame(&mut e, vec![]);
        ctx.memory_mut(|m| m.request_focus(Id::new("ed")));
        let inside_braces = text.chars().count() - 2;
        e.set_cursor(&ctx, inside_braces);
        frame(&mut e, vec![]);
        frame(&mut e, vec![egui::Event::Text("l".into())]);
        assert!(e.text.contains("find({l})"), "{}", e.text);
        let popup = e.completion.as_ref().expect("popup opened while typing");
        let labels: Vec<&str> = popup.items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(&labels[..2], ["launches", "embeddable"]);
        assert!(labels.contains(&"$limit"));
        // Enter accepts the highlighted suggestion.
        frame(&mut e, vec![egui::Event::Key { key: Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE }]);
        assert!(e.text.contains("find({launches})"), "{}", e.text);
        assert!(e.completion.is_none());
    }

    #[test]
    fn selection_runs_with_offset() {
        let mut e = editor("x\ny\nz", 0);
        e.selection = Some((2, 3));
        let r = e.run_all();
        assert_eq!((r.code.as_str(), r.line_offset), ("y", 1));
        assert_eq!(editor("ab\ncd", 4).line_col(), (2, 2));
    }
}
