//! The shell's code editor: a monospace `TextEdit` with JavaScript colouring,
//! a line-number gutter, the current line highlighted, an error marker, an
//! autocomplete popup, and a find bar (⌘F).

use std::sync::Arc;

use egui::text::{CCursor, CCursorRange};
use egui::{Align2, Area, Frame, Galley, Id, Key, Modifiers, Order, Rect, Sense, Shape, TextEdit, Ui, pos2, vec2};
use rongo_shell::complete::{self, Completion, Kind};

use crate::doc_find::{self, DocFind};
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
    /// The text as last laid out, reused while it stays the same.
    laid: Option<Laid>,
    /// The find bar (⌘F), while it is open.
    find: Option<DocFind>,
    /// Counts the edits of `text`: the find bar searches it again.
    version: u64,
}

/// The editor's text as laid out (coloured, wrapped at `wrap`), kept while
/// the text and the wrap width stay the same: colouring and laying out a
/// long script every frame took milliseconds.
struct Laid {
    text: String,
    wrap: f32,
    galley: Arc<Galley>,
    /// A one-character text laid out next to it each frame. egui hands back
    /// this same galley for it for as long as its galley cache keeps what it
    /// laid out; another one means the cache was emptied (a new font atlas,
    /// another scale), and `galley` may point at glyphs that have moved.
    probe: Arc<Galley>,
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
        Self { text: text.into(), id, completion: None, cursor: 0, selection: None, error_line: None, awaiting_fields: None, laid: None, find: None, version: 0 }
    }

    /// The find bar, drawn where it is called (above the text). ⌘F opens it
    /// while the text or the bar's field has the keyboard focus and `keys`
    /// (no dialog open), and Esc there closes it; with the suggestions open,
    /// Esc is theirs.
    pub fn find_bar(&mut self, ui: &mut Ui, keys: bool) {
        let keys = doc_find::Keys::Focus { enabled: keys, text_esc: self.completion.is_some() };
        doc_find::bar(&mut self.find, ui, self.id, "Find in this script", keys, &self.text, self.version);
    }

    /// Whether the text, or its find bar's field, has the keyboard focus:
    /// ⌘F finds in the text then.
    pub fn has_keys(&self, ctx: &egui::Context) -> bool {
        ctx.memory(|m| m.has_focus(self.id) || m.has_focus(doc_find::field_id(self.id)))
    }

    /// Tests: the find bar, while it is open.
    #[cfg(test)]
    pub fn find(&self) -> Option<&DocFind> {
        self.find.as_ref()
    }

    /// Tests: whether the suggestions are open.
    #[cfg(test)]
    pub fn completing(&self) -> bool {
        self.completion.is_some()
    }

    /// Replace the whole text (Load file, New). The old cursor and
    /// selection don't belong to it: Run would run a slice of the new text.
    pub fn set_text(&mut self, text: String) {
        self.text = text;
        self.version += 1;
        if let Some(f) = &mut self.find {
            f.restart();
        }
        self.cursor = 0;
        self.selection = None;
        self.completion = None;
        self.error_line = None;
    }

    /// 1-based line and column of the cursor, counted without copying.
    pub fn line_col(&self) -> (usize, usize) {
        self.text.chars().take(self.cursor).fold((1, 1), |(line, col), c| if c == '\n' { (line + 1, 1) } else { (line, col + 1) })
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
            _ => self.run_everything(),
        }
    }

    /// All of the text, whatever is selected.
    fn run_everything(&self) -> RunRequest {
        RunRequest { code: self.text.clone(), line_offset: 0 }
    }

    /// The paragraph (lines between blank lines) around the cursor.
    pub fn run_current(&self) -> RunRequest {
        self.run_statement_at(self.cursor)
    }

    /// The paragraph (lines between blank lines) around character `at`.
    fn run_statement_at(&self, at: usize) -> RunRequest {
        let lines: Vec<&str> = self.text.split('\n').collect();
        let line = 1 + self.text.chars().take(at).filter(|&c| c == '\n').count();
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

    /// `collections` gives the database's collection names and `fields` a
    /// collection's field names when they are known; both are asked only
    /// when completing (a database can hold thousands of collections).
    pub fn ui(&mut self, ui: &mut Ui, collections: &dyn Fn() -> Vec<String>, fields: &dyn Fn(&str) -> Option<Vec<String>>) -> EditorOutput {
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

        // Keys that must not reach the TextEdit. They run from the find bar's
        // field too, where the text shows no selection or caret: ⌘↩ (F5)
        // runs all of it, ⇧⌘↩ the statement holding the current match (the
        // caret's when nothing matches).
        let in_find = self.find.is_some() && ui.memory(|m| m.has_focus(doc_find::field_id(self.id)));
        if has_focus || in_find {
            if ui.input_mut(|i| i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Enter)) {
                let at = self.find.as_ref().and_then(DocFind::current).filter(|_| in_find).map_or(self.cursor, |m| m.start);
                out.run = Some(self.run_statement_at(at));
            } else if ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::Enter)) || ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::F5)) {
                out.run = Some(if in_find { self.run_everything() } else { self.run_all() });
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
        let line_count = memchr::memchr_iter(b'\n', self.text.as_bytes()).count() + 1;
        let gutter_w = (line_count.to_string().len().max(2) as f32) * char_w + 18.0;

        let keys_kept = self.keys_kept();
        // Taken out for the frame, so the closures below can borrow the rest.
        let mut laid = self.laid.take();
        let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, wrap: f32| {
            let probe = ui.fonts_mut(|f| f.layout_no_wrap("0".into(), theme::mono(13.5), theme::TEXT));
            if let Some(l) = laid.as_ref()
                && l.wrap == wrap
                && Arc::ptr_eq(&l.probe, &probe)
                && l.text == buf.as_str()
            {
                return l.galley.clone();
            }
            let job = highlight::layout_job(buf.as_str(), theme::mono(13.5), wrap);
            let galley = ui.fonts_mut(|f| f.layout_job(job));
            laid = Some(Laid { text: buf.as_str().to_owned(), wrap, galley: galley.clone(), probe });
            galley
        };

        let scroll = egui::ScrollArea::both().id_salt(self.id.with("scroll")).auto_shrink([false, false]).show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                let (gutter, _) = ui.allocate_exact_size(vec2(gutter_w, row_height), Sense::hover());
                let background = ui.painter().add(Shape::Noop);
                // The find bar's matches, over the line marks and behind the text.
                let marks = ui.painter().add(Shape::Noop);
                let edit_from = doc_find::edit_from(&self.find, ui.ctx(), self.id);
                let output = TextEdit::multiline(&mut self.text)
                    .id(self.id)
                    .font(font.clone())
                    .code_editor()
                    .frame(egui::Frame::NONE)
                    .event_filter(keys_kept)
                    .desired_width(f32::INFINITY)
                    .desired_rows((ui.available_height() / row_height).max(4.0) as usize)
                    .layouter(&mut layouter)
                    .show(ui);

                if let Some(range) = output.cursor_range {
                    self.cursor = range.primary.index.0;
                    let (a, b) = (range.primary.index.0, range.secondary.index.0);
                    self.selection = Some((a.min(b), a.max(b)));
                }
                if output.response.changed() {
                    self.version += 1;
                    // Found again at once: the marks are for this text.
                    doc_find::edited(&mut self.find, ui.ctx(), &self.text, self.version, edit_from, &output);
                }
                doc_find::paint(&mut self.find, ui, marks, &output);

                // Gutter numbers, current line and error line, for the rows
                // in view: the rows above are only counted.
                let painter = ui.painter();
                let full_width = ui.clip_rect().right() - gutter.left();
                let (cur_line, _) = self.line_col();
                let rows = &output.galley.rows;
                let clip = ui.clip_rect().y_range();
                let first = rows.partition_point(|r| output.galley_pos.y + r.max_y() < clip.min);
                let mut line = 1 + rows[..first].iter().filter(|r| r.ends_with_newline).count();
                let mut starts_line = first == 0 || rows[first - 1].ends_with_newline;
                let mut highlight = Vec::new();
                for row in &rows[first..] {
                    let y = output.galley_pos.y + row.min_y();
                    if y > clip.max {
                        break;
                    }
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
        self.laid = laid;

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
        // The suggestions opened or closed after the text was drawn: from
        // the next frame's Esc on, as they are now.
        ui.memory_mut(|m| m.set_focus_lock_filter(self.id, self.keys_kept()));
        out
    }

    /// The keys the text keeps while it has the focus: the arrows, Tab (it
    /// indents), and Esc while the suggestions are open, which Esc closes.
    /// egui moves the focus away on any other: Esc then leaves the text.
    fn keys_kept(&self) -> egui::EventFilter {
        egui::EventFilter { horizontal_arrows: true, vertical_arrows: true, tab: true, escape: self.completion.is_some() }
    }

    fn refresh_completion(&mut self, collections: &dyn Fn() -> Vec<String>, fields: &dyn Fn(&str) -> Option<Vec<String>>, forced: bool) {
        let cursor_byte = char_to_byte(&self.text, self.cursor);
        let before = &self.text[..cursor_byte];
        let last = before.chars().last();
        let typing = last.is_some_and(|c| matches!(c, '.' | '_' | '$' | '"' | '\'') || c.is_alphanumeric());
        if !typing && !forced {
            self.completion = None;
            return;
        }
        let c = complete::complete(before, &collections(), fields);
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
        self.version += 1;
        if let Some(f) = &mut self.find {
            let start = self.text[..p.start_byte].chars().count();
            f.update(ctx, &self.text, self.version, Some(doc_find::Edit { start, caret: new_cursor }));
        }
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
                e.ui(ui, &Vec::new, &fields);
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
                e.ui(ui, &Vec::new, &fields);
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

    /// The collection names are asked for only when completing: a repaint
    /// with nothing typed doesn't copy a database's thousands of names.
    #[test]
    fn collection_names_are_read_only_when_completing() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut e = CodeEditor::new(Id::new("ed"), "");
        let asked = std::cell::Cell::new(0);
        let collections = || {
            asked.set(asked.get() + 1);
            vec!["users".to_string(), "orders".to_string()]
        };
        let fields = |_: &str| None;
        let frame = |e: &mut CodeEditor, events: Vec<egui::Event>| {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 500.0))), events, ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| {
                ui.memory_mut(|m| m.request_focus(e.id));
                e.ui(ui, &collections, &fields);
            });
            out.textures_delta.clear();
        };
        for _ in 0..5 {
            frame(&mut e, vec![]);
        }
        assert_eq!(asked.get(), 0, "not asked while nothing is typed");
        frame(&mut e, vec![egui::Event::Text("db.u".into())]);
        let popup = e.completion.as_ref().expect("suggestions opened");
        assert!(popup.items.iter().any(|i| i.label == "users"), "the collections are offered");
        assert!(asked.get() > 0);
    }

    #[test]
    fn selection_runs_with_offset() {
        let mut e = editor("x\ny\nz", 0);
        e.selection = Some((2, 3));
        let r = e.run_all();
        assert_eq!((r.code.as_str(), r.line_offset), ("y", 1));
        assert_eq!(editor("ab\ncd", 4).line_col(), (2, 2));
    }

    /// A 5,000-line script lays out line numbers only for the lines in
    /// view, the right ones, also scrolled far down.
    #[test]
    fn gutter_numbers_only_for_lines_in_view() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let script: String = (0..5_000).map(|i| format!("print({i})\n")).collect();
        let mut e = CodeEditor::new(Id::new("ed"), script);
        let fields = |_: &str| None;
        let frame = |e: &mut CodeEditor, events: Vec<egui::Event>| -> Vec<usize> {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 500.0))), events, ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| {
                e.ui(ui, &Vec::new, &fields);
            });
            out.textures_delta.clear();
            // Line numbers are the texts that are only digits.
            out.shapes
                .iter()
                .filter_map(|s| match &s.shape {
                    egui::Shape::Text(t) => t.galley.text().parse::<usize>().ok(),
                    _ => None,
                })
                .collect()
        };
        frame(&mut e, vec![]);
        let numbers = frame(&mut e, vec![]);
        assert!(!numbers.is_empty() && numbers.len() < 100, "{} line numbers laid out", numbers.len());
        assert_eq!(numbers[0], 1);
        assert!(numbers.windows(2).all(|w| w[1] == w[0] + 1), "{numbers:?}");
        let wheel = egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Page, delta: egui::vec2(0.0, -200.0), modifiers: Modifiers::NONE, phase: egui::TouchPhase::Move };
        frame(&mut e, vec![egui::Event::PointerMoved(egui::pos2(400.0, 250.0))]);
        frame(&mut e, vec![wheel]);
        let numbers = frame(&mut e, vec![]);
        assert!(!numbers.is_empty() && numbers.len() < 100, "{} line numbers laid out", numbers.len());
        assert!(numbers[0] > 1_000 && numbers.windows(2).all(|w| w[1] == w[0] + 1), "scrolled down: {numbers:?}");
    }

    /// The laid-out text is kept while it stays the same, and laid out
    /// again when it changes or egui's fonts start over (another scale):
    /// the old galley's glyphs would be in the wrong places.
    #[test]
    fn the_layout_is_kept_until_the_text_or_the_fonts_change() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut e = CodeEditor::new(Id::new("ed"), "db.users.find({ name: \"สมชาย\" })\nprint(1)");
        let fields = |_: &str| None;
        let frame = |e: &mut CodeEditor, events: Vec<egui::Event>| {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 500.0))), events, ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| {
                ui.memory_mut(|m| m.request_focus(e.id));
                e.ui(ui, &Vec::new, &fields);
            });
            out.textures_delta.clear();
        };
        let galley = |e: &CodeEditor| e.laid.as_ref().expect("laid out").galley.clone();
        frame(&mut e, vec![]);
        let first = galley(&e);
        for _ in 0..3 {
            frame(&mut e, vec![]);
        }
        assert!(Arc::ptr_eq(&first, &galley(&e)), "kept while nothing changed");
        frame(&mut e, vec![egui::Event::Text("x".into())]);
        let typed = galley(&e);
        assert!(!Arc::ptr_eq(&first, &typed) && typed.text().contains('x'), "laid out again after typing");
        ctx.set_pixels_per_point(2.0);
        frame(&mut e, vec![]);
        frame(&mut e, vec![]);
        let scaled = galley(&e);
        assert!(!Arc::ptr_eq(&typed, &scaled), "laid out again for the new scale");
        assert_eq!(scaled.pixels_per_point, 2.0);
    }

    /// With the find bar open, the matches in view are marked behind the
    /// script and its layout is kept: frames with nothing changed lay
    /// nothing out, search nothing, work out where the rows start once,
    /// and look only at the rows in view, also scrolled to the end.
    #[test]
    fn the_layout_is_kept_with_the_find_bar_open() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let script: String = (0..5_000).map(|i| format!("print({i})\n")).collect();
        let mut e = CodeEditor::new(Id::new("ed"), script);
        e.find = Some(DocFind::typed("print"));
        let fields = |_: &str| None;
        // A quarter of a second a frame: scrolling to a match is animated.
        let time = std::cell::Cell::new(0.0);
        let frame = |e: &mut CodeEditor| -> usize {
            time.set(time.get() + 0.25);
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 500.0))), time: Some(time.get()), ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| {
                e.find_bar(ui, true);
                e.ui(ui, &Vec::new, &fields);
            });
            out.textures_delta.clear();
            let mut marks = 0;
            let mut count = |s: &egui::Shape| if let egui::Shape::Rect(r) = s && (r.fill == theme::FIND_MATCH || r.fill == theme::FIND_CURRENT) { marks += 1 };
            for c in &out.shapes {
                match &c.shape {
                    egui::Shape::Vec(v) => v.iter().for_each(&mut count),
                    s => count(s),
                }
            }
            marks
        };
        for _ in 0..3 {
            frame(&mut e);
        }
        let galley = e.laid.as_ref().expect("laid out").galley.clone();
        let f = e.find.as_ref().expect("the bar is open");
        let (searches, scans) = (f.searches, f.row_scans);
        for _ in 0..5 {
            let marks = frame(&mut e);
            assert!(marks > 0 && marks < 100, "{marks} matches marked, the ones in view");
        }
        let f = e.find.as_ref().unwrap();
        assert!(Arc::ptr_eq(&galley, &e.laid.as_ref().unwrap().galley), "the script was laid out again");
        assert_eq!((f.searches, f.row_scans), (searches, scans), "searched again, or the rows walked again");
        assert!(f.rows_looked_at < 100, "{} rows looked at in a frame", f.rows_looked_at);
        // A needle found only at the end scrolls there.
        *e.find.as_mut().unwrap().needle_mut() = "print(4999)".into();
        for _ in 0..3 {
            frame(&mut e);
        }
        let f = e.find.as_ref().unwrap();
        assert_eq!(f.row_scans, scans, "the rows walked again");
        assert!(f.rows_looked_at < 100, "{} rows looked at in a frame, at the end", f.rows_looked_at);
        assert_eq!(frame(&mut e), 1, "the match at the end is in view");
    }

    /// From the find bar's field, ⌘↩ runs the whole script, never a
    /// selection the script no longer shows, and ⇧⌘↩ the statement holding
    /// the current match (the caret's when nothing matches).
    #[test]
    fn run_keys_from_the_find_field() {
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let text = "db.a.find()\n\ndb.b.find({ x: \"pie\" })\n\nprint(1)";
        let mut e = CodeEditor::new(Id::new("ed"), text);
        let fields = |_: &str| None;
        let frame = |e: &mut CodeEditor, events: Vec<egui::Event>| -> Option<RunRequest> {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 500.0))), events, ..Default::default() };
            let mut run = None;
            let mut out = ctx.run_ui(input, |ui| {
                e.find_bar(ui, true);
                run = e.ui(ui, &Vec::new, &fields).run;
            });
            out.textures_delta.clear();
            run
        };
        let key = |key, modifiers| egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers };
        frame(&mut e, vec![]);
        // "db.a" selected in the script.
        ctx.memory_mut(|m| m.request_focus(e.id));
        let mut state = TextEdit::load_state(&ctx, e.id).unwrap_or_default();
        state.cursor.set_char_range(Some(CCursorRange::two(CCursor::new(0), CCursor::new(4))));
        state.store(&ctx, e.id);
        frame(&mut e, vec![]);
        frame(&mut e, vec![]);
        assert_eq!(e.run_all().code, "db.a", "from the script, the selection runs");
        frame(&mut e, vec![key(Key::F, Modifiers::COMMAND)]);
        frame(&mut e, vec![egui::Event::Text("pie".into())]);
        frame(&mut e, vec![]);
        assert!(ctx.memory(|m| m.has_focus(doc_find::field_id(e.id))), "the find field has the focus");
        let run = frame(&mut e, vec![key(Key::Enter, Modifiers::COMMAND)]).expect("⌘↩ ran");
        assert_eq!(run.code, text, "the whole script, not the selection the script no longer shows");
        let run = frame(&mut e, vec![key(Key::F5, Modifiers::NONE)]).expect("F5 ran");
        assert_eq!(run.code, text, "F5 as ⌘↩");
        let run = frame(&mut e, vec![key(Key::Enter, Modifiers::COMMAND | Modifiers::SHIFT)]).expect("⇧⌘↩ ran");
        assert_eq!((run.code.as_str(), run.line_offset), ("db.b.find({ x: \"pie\" })", 2), "the current match's statement");
        *e.find.as_mut().unwrap().needle_mut() = "zzz".into();
        frame(&mut e, vec![]);
        let run = frame(&mut e, vec![key(Key::Enter, Modifiers::COMMAND | Modifiers::SHIFT)]).expect("⇧⌘↩ ran");
        assert_eq!((run.code.as_str(), run.line_offset), ("db.a.find()", 0), "no match: the caret's statement");
    }

    /// Line and column count characters (Thai is one column a character),
    /// from 1.
    #[test]
    fn line_and_column_of_the_cursor() {
        assert_eq!(editor("", 0).line_col(), (1, 1));
        assert_eq!(editor("ab\n", 3).line_col(), (2, 1));
        assert_eq!(editor("ไทย\nสวัสดี x", 9).line_col(), (2, 6));
        assert_eq!(editor("a\n\nb", 3).line_col(), (3, 1));
    }

    /// What a frame of the editor costs with a 5,000-line script: idle (the
    /// cursor blinking, the pointer moving), and typing a character a frame.
    /// Run with `cargo test -p rongo --release -- --ignored --nocapture report_editor_frame_time`.
    #[test]
    #[ignore = "timing; run explicitly in release mode"]
    fn report_editor_frame_time() {
        use std::time::Instant;
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let script: String = (0..5_000).map(|i| format!("db.getCollection(\"games\").updateOne({{ _id: {i} }}, {{ $set: {{ name: \"สล็อต {i}\", rtp: {}.5 }} }}) // line {i}\n", i % 97)).collect();
        let lines = script.lines().count();
        let mut e = CodeEditor::new(Id::new("ed"), script);
        let fields = |_: &str| None;
        let mut frame = |e: &mut CodeEditor, events: Vec<egui::Event>| {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 800.0))), events, ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| {
                ui.memory_mut(|m| m.request_focus(e.id));
                e.ui(ui, &Vec::new, &fields);
                std::hint::black_box(e.line_col());
            });
            out.textures_delta.clear();
        };
        for _ in 0..3 {
            frame(&mut e, vec![]);
        }
        e.set_cursor(&ctx, e.text.chars().count() / 2);
        frame(&mut e, vec![]);
        let best = |e: &mut CodeEditor, frame: &mut dyn FnMut(&mut CodeEditor, Vec<egui::Event>), typing: bool| {
            (0..5)
                .map(|_| {
                    let t = Instant::now();
                    for k in 0..10 {
                        let events = if typing { vec![egui::Event::Text(if k % 2 == 0 { "x".into() } else { "y".into() })] } else { vec![egui::Event::PointerMoved(egui::pos2(300.0 + k as f32, 300.0))] };
                        frame(e, events);
                    }
                    t.elapsed().as_secs_f64() * 100.0
                })
                .fold(f64::INFINITY, f64::min)
        };
        let idle = best(&mut e, &mut frame, false);
        let typing = best(&mut e, &mut frame, true);
        eprintln!("editor, {lines} lines: {idle:.2} ms/frame idle, {typing:.2} ms/frame typing");
    }
}
