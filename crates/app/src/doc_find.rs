//! Find in the document dialogs (Edit / Add Document, View Document): ⌘F
//! shows the JSON view's find bar above the text, the matches are marked
//! behind it, and the current one is scrolled into view.

use std::ops::Range;
use std::sync::Arc;

use egui::epaint::text::{CharIndex, LayoutJob};
use egui::layers::ShapeIdx;
use egui::text::{CCursor, CCursorRange, Galley};
use egui::{Align, Id, Key, Modifiers, Pos2, Rect, Shape, Ui, pos2, vec2};

use crate::result_view::find::{Asked, Bar, Status, find_in_text};
use crate::theme;

/// About the height the bar takes. The dialogs give it from the text's, so
/// they keep their size (and their buttons stay on a short window).
pub const BAR_H: f32 = 30.0;

/// A document dialog's find bar and what it found.
#[derive(Default)]
pub struct DocFind {
    needle: String,
    case: bool,
    /// Focus the field and select its text next frame (⌘F).
    focus: bool,
    /// Where the matches are, as character ranges into the text: egui's
    /// cursors count characters.
    matches: Vec<Range<usize>>,
    current: usize,
    /// The needle, match case and text version `matches` is for.
    searched: Option<(String, bool, u64)>,
    /// The characters in the text searched last.
    chars: usize,
    /// Bring the current match into view.
    scroll_to: bool,
    /// The first character in view, as of the last frame: a new needle's
    /// first match is the first from there.
    top: usize,
    /// Where the rows of the laid-out text start.
    rows: Option<RowStarts>,
    /// Searches run so far.
    #[cfg(test)]
    pub searches: usize,
    /// Walks over every row, to learn where each starts.
    #[cfg(test)]
    pub row_scans: usize,
    /// Rows the last frame looked at.
    #[cfg(test)]
    pub rows_looked_at: usize,
}

/// The first character of each row of a laid-out text, and the end of the
/// text last, worked out once per layout: a frame then finds the rows in
/// view and the row of a match by binary search instead of walking every
/// row above them. Known by the layout's job: the copy of a galley egui
/// makes to paint a selection on shares it, a new layout has its own.
struct RowStarts {
    job: Arc<LayoutJob>,
    starts: Vec<usize>,
}

/// An edit of the text, in characters of the edited text: before `start`
/// it is as it was, and the caret is at `caret`, just after what was
/// typed (or where text was deleted).
#[derive(Clone, Copy)]
pub struct Edit {
    pub start: usize,
    pub caret: usize,
}

/// The find bar of a document dialog, drawn where it is called (above the
/// text). ⌘F opens it, or focuses its field again with what is typed
/// selected. Esc or ✕ closes it and gives the focus to the text, the
/// `TextEdit` `editor`; the dialog sees no Esc. From the bar (its field,
/// its buttons, ✕), the text gets the current match selected; Esc pressed
/// in the text leaves its caret and selection as they were. The keys are
/// left alone while another dialog was over this one last frame. `text` is
/// the document's text; `version` changes whenever it does.
pub fn bar(find: &mut Option<DocFind>, ui: &mut Ui, editor: Id, text: &str, version: u64) {
    let on_top = ui.ctx().memory(|m| m.allows_interaction(ui.layer_id()));
    if on_top && ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::F)) {
        find.get_or_insert_default().focus = true;
    }
    let Some(f) = find else { return };
    let status = if f.needle.is_empty() { Status::Empty } else { Status::Count { current: f.current, total: f.matches.len() } };
    let any = !f.matches.is_empty();
    let Asked { step, close } = Bar { id: editor.with("find"), hint: "Find in this document", text: &mut f.needle, case: &mut f.case, focus: &mut f.focus, any, status }.show(ui);
    // Taken here, before the dialog looks for it.
    let esc = on_top && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
    if on_top && (close || esc) {
        // Esc has taken the focus from whatever had it: ask who had it.
        let in_text = !close && ui.memory(|m| m.had_focus_last_frame(editor));
        if !in_text && let Some(m) = f.matches.get(f.current) {
            let mut state = egui::TextEdit::load_state(ui.ctx(), editor).unwrap_or_default();
            state.cursor.set_char_range(Some(CCursorRange::two(CCursor::new(m.start), CCursor::new(m.end))));
            state.store(ui.ctx(), editor);
        }
        // Before the text is drawn, which keeps its selection only with the focus.
        ui.memory_mut(|m| m.request_focus(editor));
        *find = None;
        return;
    }
    f.update(ui.ctx(), text, version, None);
    if step != 0 && !f.matches.is_empty() {
        f.current = (f.current as i64 + step).rem_euclid(f.matches.len() as i64) as usize;
        f.scroll_to = true;
    }
}

impl DocFind {
    /// Tests: an open bar with `needle` typed in it.
    #[cfg(test)]
    pub fn typed(needle: &str) -> Self {
        Self { needle: needle.into(), ..Default::default() }
    }

    /// Tests: what is typed in the bar, to change as typing would.
    #[cfg(test)]
    pub fn needle_mut(&mut self) -> &mut String {
        &mut self.needle
    }

    /// Finds again when the needle, match case or the text (`version`)
    /// changed since the last search, and only then. A new needle starts at
    /// the first match in view or below it, and scrolls to it. The same
    /// needle in edited text (`edit`, where it was edited) stays on the
    /// match it was on, without scrolling: an edit after that match leaves
    /// its place in the count, one before it moves it with the text, and
    /// one inside it, which may unmake it, goes on to the first match from
    /// the caret.
    pub fn update(&mut self, ctx: &egui::Context, text: &str, version: u64, edit: Option<Edit>) {
        let same_needle = self.searched.as_ref().is_some_and(|(n, c, _)| *n == self.needle && *c == self.case);
        if same_needle && self.searched.as_ref().is_some_and(|s| s.2 == version) {
            return;
        }
        let was = self.matches.get(self.current).cloned().filter(|_| same_needle);
        let (index, chars_before) = (self.current, self.chars);
        (self.matches, self.chars) = char_ranges(text, find_in_text(text, &self.needle, self.case));
        let first_from = |at: usize| self.matches.iter().position(|m| m.start >= at).unwrap_or(0);
        self.current = match (was, edit) {
            (None, _) => first_from(self.top),
            (Some(m), None) => first_from(m.start),
            (Some(m), Some(e)) => {
                // Where the edit ended in the text before it.
                let grew = self.chars as i64 - chars_before as i64;
                let old_end = (e.caret as i64 - grew).max(e.start as i64) as usize;
                if m.end <= e.start {
                    index.min(self.matches.len().saturating_sub(1))
                } else if m.start >= old_end {
                    first_from((m.start as i64 + grew).max(0) as usize)
                } else {
                    first_from(e.caret)
                }
            }
        };
        self.scroll_to = !same_needle && !self.matches.is_empty();
        self.searched = Some((self.needle.clone(), self.case, version));
        // The bar said its count before this search: say the new one.
        ctx.request_repaint();
        #[cfg(test)]
        {
            self.searches += 1;
        }
    }

    /// Works out where the rows of `galley` start, for a new layout only.
    fn learn_rows(&mut self, galley: &Galley) {
        let known = self.rows.as_ref().is_some_and(|r| Arc::ptr_eq(&r.job, &galley.job) && r.starts.len() == galley.rows.len() + 1);
        if !known {
            let mut starts = Vec::with_capacity(galley.rows.len() + 1);
            let mut at = 0;
            starts.push(at);
            for row in &galley.rows {
                at += row.char_count_including_newline().0;
                starts.push(at);
            }
            self.rows = Some(RowStarts { job: galley.job.clone(), starts });
            #[cfg(test)]
            {
                self.row_scans += 1;
            }
        }
    }
}

/// After the text is drawn (`out`, in a scroll area's `ui`): the matches on
/// the rows in view marked behind the text, in `slot` (a `Shape::Noop`
/// added before the text), and the current one scrolled into view when it
/// changed, to the middle unless it is already in view. Only the rows in
/// view are looked at.
pub fn paint(find: &mut Option<DocFind>, ui: &Ui, slot: ShapeIdx, out: &egui::text_edit::TextEditOutput) {
    let Some(f) = find else { return };
    let galley = &out.galley;
    let origin = out.galley_pos - vec2(galley.rect.left(), 0.0);
    let clip = ui.clip_rect();
    f.learn_rows(galley);
    let starts: &[usize] = f.rows.as_ref().map_or(&[], |r| &r.starts);
    let rows = &galley.rows;
    let last = rows.len().saturating_sub(1).min(starts.len().saturating_sub(1));
    // The first row in view, and the first match not ended before it.
    let first = rows.partition_point(|r| origin.y + r.max_y() < clip.top()).min(last);
    let mut k = f.matches.partition_point(|m| m.end <= starts.get(first).copied().unwrap_or(0));
    let mut marks = Vec::new();
    #[cfg(test)]
    let mut looked_at = 0;
    for (i, row) in rows.iter().enumerate().take(last + 1).skip(first) {
        if origin.y + row.min_y() > clip.bottom() {
            break;
        }
        #[cfg(test)]
        {
            looked_at += 1;
        }
        let (at, n) = (starts[i], row.glyphs.len());
        while k < f.matches.len() && f.matches[k].end <= at {
            k += 1;
        }
        for (j, m) in f.matches.iter().enumerate().skip(k).take_while(|(_, m)| m.start < at + n) {
            if let Some(r) = part(galley, origin, i, at, m) {
                marks.push(Shape::rect_filled(r, 0.0, if j == f.current { theme::FIND_CURRENT } else { theme::FIND_MATCH }));
            }
        }
    }
    let top = starts.get(first).copied().unwrap_or(0);
    // The current match's place, on the row it starts on, when it is to be brought into view.
    let target = f.matches.get(f.current).filter(|_| f.scroll_to).and_then(|m| {
        let i = starts.partition_point(|&s| s <= m.start).saturating_sub(1).min(last);
        part(galley, origin, i, *starts.get(i)?, m)
    });
    ui.painter().set(slot, Shape::Vec(marks));
    f.top = top;
    f.scroll_to = false;
    #[cfg(test)]
    {
        f.rows_looked_at = looked_at;
    }
    if let Some(r) = target.filter(|r| !clip.contains_rect(*r)) {
        ui.scroll_to_rect(r, Some(Align::Center));
    }
}

/// Where the part of match `m` on row `i` (its first character `at`) is,
/// if any of it is: from the galley's own glyph positions.
fn part(galley: &Galley, origin: Pos2, i: usize, at: usize, m: &Range<usize>) -> Option<Rect> {
    let row = galley.rows.get(i)?;
    let n = row.glyphs.len();
    let (a, b) = (m.start.max(at) - at, m.end.min(at + n).saturating_sub(at));
    if a >= b {
        return None;
    }
    let rect = row.rect().translate(origin.to_vec2());
    let (x0, x1) = (row.x_offset(CharIndex(a)), row.glyphs[b - 1].max_x());
    Some(Rect::from_min_max(pos2(rect.left() + x0, rect.top()), pos2(rect.left() + x1, rect.bottom())))
}

/// `bytes`, ranges of byte offsets into `text` in order and apart, as
/// ranges of character offsets; and the characters in `text`.
fn char_ranges(text: &str, bytes: Vec<Range<usize>>) -> (Vec<Range<usize>>, usize) {
    let (mut byte, mut char) = (0, 0);
    let mut to_char = |b: usize| {
        char += text[byte..b].chars().count();
        byte = b;
        char
    };
    let ranges = bytes.into_iter().map(|r| to_char(r.start)..to_char(r.end)).collect();
    let chars = to_char(text.len());
    (ranges, chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_ranges_become_char_ranges() {
        let text = "ไทย apple ไทย Apple";
        let found = find_in_text(text, "apple", false);
        assert_eq!(char_ranges(text, found), (vec![4..9, 14..19], 19));
        assert_eq!(char_ranges(text, Vec::new()).0.len(), 0);
    }
}
