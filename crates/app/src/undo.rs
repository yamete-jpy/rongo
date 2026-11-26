//! Undo for the two text editors, IntelliShell's script and the document
//! editor (Edit / Add document): ⌘Z steps back about a word at a time,
//! ⇧⌘Z or ⌘Y forward again. egui's own undo saved a point only after a
//! second's pause, so ⌘Z after half a minute of typing took all of it away.

use std::collections::VecDeque;
use std::path::PathBuf;

use egui::text::{CCursor, CCursorRange};
use egui::text_edit::TextEditOutput;
use egui::{Event, Id, ImeEvent, Key, Modifiers, TextEdit, Ui};

/// A selection in characters (egui's cursors count characters): where it
/// was started and where the caret is. Equal when nothing is selected.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Sel {
    pub anchor: usize,
    pub caret: usize,
}

impl Sel {
    /// Just a caret.
    pub fn at(caret: usize) -> Self {
        Self { anchor: caret, caret }
    }

    fn of(r: CCursorRange) -> Self {
        Self { anchor: r.secondary.index.0, caret: r.primary.index.0 }
    }

    fn range(self) -> CCursorRange {
        CCursorRange::two(CCursor::new(self.anchor), CCursor::new(self.caret))
    }
}

/// How a frame's edit was made, from the events the text saw.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum How {
    /// Typed: characters, ↩, Tab.
    Typed,
    /// ⌫ pressed this many times.
    Back(usize),
    /// ⌦ pressed this many times.
    Forward(usize),
    /// Anything else (paste, cut, a word deleted, a suggestion accepted,
    /// the whole text replaced): a step of its own.
    Other,
}

impl How {
    /// From the events of a frame in which the text changed.
    fn of(events: &[Event]) -> Self {
        let (mut typed, mut back, mut forward, mut other) = (false, 0, 0, false);
        for e in events {
            match e {
                Event::Text(t) => typed |= !t.is_empty(),
                Event::Ime(ImeEvent::Preedit { .. } | ImeEvent::Commit(_)) => typed = true,
                Event::Ime(ImeEvent::DeleteSurrounding { .. }) | Event::Paste(_) | Event::Cut => other = true,
                Event::Key { key, pressed: true, modifiers: m, .. } => {
                    let plain = !(m.alt || m.ctrl || m.command || m.mac_cmd);
                    match key {
                        Key::Enter if plain => typed = true,
                        Key::Tab if plain && !m.shift => typed = true,
                        Key::Backspace if plain => back += 1,
                        // ⌃H on macOS.
                        Key::H if m.ctrl && !(m.alt || m.command || m.mac_cmd) => back += 1,
                        Key::Delete if plain => forward += 1,
                        // A word, a line, or less indentation at once.
                        Key::Backspace | Key::Delete | Key::Tab => other = true,
                        Key::K | Key::U | Key::W if m.ctrl => other = true,
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        match (typed, back, forward, other) {
            (true, 0, 0, false) => How::Typed,
            (false, n, 0, false) if n > 0 => How::Back(n),
            (false, 0, n, false) if n > 0 => How::Forward(n),
            _ => How::Other,
        }
    }
}

/// An undo or redo: the edit it made, in characters of the text as it is
/// now (before `start` the text is as it was, and what was put back ends
/// at `end`), the selection put back, and the file the text belongs to
/// when the step changed it (IntelliShell's Load file and New).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Undone {
    pub start: usize,
    pub end: usize,
    pub sel: Sel,
    pub file: Option<Option<PathBuf>>,
}

/// No edit for this long (seconds) and the next one begins a step.
const PAUSE: f64 = 1.0;
/// The most steps kept; the oldest go first.
const MAX_STEPS: usize = 10_000;
/// About the most text the steps keep, in bytes; the oldest go first (the
/// newest step stays, however large).
const MAX_BYTES: usize = 64 << 20;

/// The steps that can be undone and redone in one editor's text. Each is
/// what changed, found by comparing the text with the copy the history
/// keeps of it (common start and end), never a copy of the whole text.
pub struct History {
    steps: VecDeque<Step>,
    /// `steps[..done]` are done; the rest were undone and can be redone.
    done: usize,
    /// The text as the history last saw it.
    seen: String,
    /// A byte offset into `seen` and its character index: characters are
    /// counted from here, so near an edit they cost what lies between.
    mark: (usize, usize),
    /// What the last step done is still growing by, if it may.
    open: Option<Grow>,
    /// The selection after the last edit, undo or move.
    sel: Option<Sel>,
    /// When the text was last edited (seconds).
    last_edit: f64,
    /// Bytes the steps keep.
    stored: usize,
    /// At most this many steps and about this many bytes kept.
    limits: (usize, usize),
    /// egui's own undo was made to keep as little as it can.
    egui_trimmed: bool,
}

/// What an open step grows by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Grow {
    Typing,
    Back,
    Forward,
}

/// One step. Done, `seen[at..at + len]` is what it put in and `kept` what
/// it took out; undone, the other way round: `kept` holds whichever of the
/// two the text doesn't.
struct Step {
    at: usize,
    len: usize,
    kept: String,
    before: Sel,
    after: Sel,
    /// The file the text belongs to, when the step changed it.
    file: Option<Box<FileChange>>,
}

/// A step's change of the file its text belongs to: Load file (from the
/// file before to the one loaded) and New (to none).
struct FileChange {
    before: Option<PathBuf>,
    after: Option<PathBuf>,
}

impl History {
    pub fn new(text: &str) -> Self {
        Self::with_limits(text, MAX_STEPS, MAX_BYTES)
    }

    fn with_limits(text: &str, steps: usize, bytes: usize) -> Self {
        Self { steps: VecDeque::new(), done: 0, seen: text.to_owned(), mark: (0, 0), open: None, sel: None, last_edit: f64::NEG_INFINITY, stored: 0, limits: (steps, bytes), egui_trimmed: false }
    }

    /// ⌘Z (undo), ⇧⌘Z and ⌘Y (redo) while the text `id` has the keyboard
    /// focus, taken before its `TextEdit` is drawn: egui's own undo never
    /// sees them, also when there is nothing to undo or redo. Undoes or
    /// redoes in `text`, in the order they were pressed, and puts the
    /// selection back in the `TextEdit`.
    pub fn keys(&mut self, ui: &Ui, id: Id, text: &mut String) -> Option<Undone> {
        if !ui.memory(|m| m.has_focus(id)) {
            return None;
        }
        // Taken in the order pressed (true: redo), as egui would match them:
        // ⇧⌘Z first, since ⌘Z matches with ⇧ held too.
        let presses = ui.input_mut(|i| {
            let mut presses = Vec::new();
            i.events.retain(|e| {
                let Event::Key { key, pressed: true, modifiers: m, .. } = e else { return true };
                let redo = (*key == Key::Z && m.matches_logically(Modifiers::COMMAND | Modifiers::SHIFT)) || (*key == Key::Y && m.matches_logically(Modifiers::COMMAND));
                if redo || (*key == Key::Z && m.matches_logically(Modifiers::COMMAND)) {
                    presses.push(redo);
                    return false;
                }
                true
            });
            presses
        });
        let mut done: Option<Undone> = None;
        for redo in presses {
            // One with nothing to do leaves the next to go on.
            let Some(u) = (if redo { self.redo(text) } else { self.undo(text) }) else { continue };
            // Several in a frame (a held key): the text before the first is
            // as it was, and the last file change stands.
            done = Some(match done {
                Some(d) => Undone { start: d.start.min(u.start), end: u.end, sel: u.sel, file: u.file.or(d.file) },
                None => u,
            });
        }
        let u = done?;
        let mut state = TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
        state.cursor.set_char_range(Some(u.sel.range()));
        state.store(ui.ctx(), id);
        Some(u)
    }

    /// Before the `TextEdit` `id` is drawn: its selection. The first time,
    /// egui's own undo (fed the text every frame, though it no longer gets
    /// the keys) is made to keep one copy of the text, not a hundred.
    pub fn before(&mut self, ctx: &egui::Context, id: Id) -> Sel {
        let state = TextEdit::load_state(ctx, id);
        if !self.egui_trimmed {
            self.egui_trimmed = true;
            let mut state = state.clone().unwrap_or_default();
            // One is the fewest that works: with none it would copy the text twice a frame.
            let settings = egui::util::undoer::Settings { max_undos: 1, ..Default::default() };
            state.set_undoer(egui::util::undoer::Undoer::with_settings(settings));
            state.store(ctx, id);
        }
        state.and_then(|s| s.cursor.char_range()).map(Sel::of).unwrap_or_default()
    }

    /// After the `TextEdit` is drawn (`out`), `text` as it left it: an edit
    /// this frame recorded, else a move of the caret noted. `before` is
    /// what `before` said.
    pub fn after(&mut self, ui: &Ui, text: &str, before: Sel, out: &TextEditOutput) {
        let sel = out.cursor_range.map(Sel::of);
        if out.response.changed() {
            let how = ui.input(|i| How::of(&i.events));
            self.edited(text, before, sel.unwrap_or(before), how, ui.input(|i| i.time));
        } else if let Some(sel) = sel {
            self.moved(sel);
        }
    }

    /// `text` was edited by the user at `now` (seconds), the selection
    /// `before` the edit and `after` it. It joins the last step when it
    /// goes on with it, where it ended and within a second of it.
    pub fn edited(&mut self, text: &str, before: Sel, after: Sel, how: How, now: f64) {
        let still = self.sel == Some(before) && now - self.last_edit < PAUSE;
        self.last_edit = now;
        self.record(text, before, after, how, still);
    }

    /// `text` was changed in one go, not typed (a suggestion accepted, the
    /// whole text replaced): a step of its own.
    pub fn replaced(&mut self, text: &str, before: Sel, after: Sel) {
        self.record(text, before, after, How::Other, false);
    }

    /// As `replaced`, for a text that belongs to a file (IntelliShell's
    /// Load file and New): the step also takes the file from `from` to
    /// `to`, and its undo puts `from` back. A change of file alone, the
    /// text as it was, is a step too.
    pub fn replaced_file(&mut self, text: &str, before: Sel, after: Sel, from: Option<PathBuf>, to: Option<PathBuf>) {
        let same = self.seen == text;
        self.record(text, before, after, How::Other, false);
        if from == to {
            return;
        }
        if same {
            self.steps.truncate(self.done);
            self.push(Step { at: 0, len: 0, kept: String::new(), before, after, file: None });
            self.open = None;
            self.trim();
        }
        if let Some(last) = self.steps.back_mut() {
            last.file = Some(Box::new(FileChange { before: from, after: to }));
        }
    }

    /// The text's file changed with the text as it is (saved under a new
    /// name): undoing back to this text, or redoing forward to it, gives
    /// that file from now on.
    pub fn file_named(&mut self, file: Option<PathBuf>) {
        if let Some(f) = self.steps.iter_mut().take(self.done).rev().find_map(|s| s.file.as_deref_mut()) {
            f.after = file.clone();
        }
        if let Some(f) = self.steps.iter_mut().skip(self.done).find_map(|s| s.file.as_deref_mut()) {
            f.before = file;
        }
    }

    /// The selection changed with the text as it was: the next edit
    /// begins a step.
    pub fn moved(&mut self, sel: Sel) {
        if self.sel != Some(sel) {
            self.open = None;
            self.sel = Some(sel);
        }
    }

    /// Undoes the last step in `text`, if any.
    pub fn undo(&mut self, text: &mut String) -> Option<Undone> {
        self.sync(text);
        self.done = self.done.checked_sub(1)?;
        Some(self.swap(self.done, text, true))
    }

    /// Redoes the last step undone in `text`, if any.
    pub fn redo(&mut self, text: &mut String) -> Option<Undone> {
        self.sync(text);
        if self.done == self.steps.len() {
            return None;
        }
        self.done += 1;
        Some(self.swap(self.done - 1, text, false))
    }

    /// A change of `text` the history wasn't told of becomes a step, so
    /// the steps' places are in the text as it is.
    fn sync(&mut self, text: &str) {
        if self.seen != text {
            let sel = self.sel.unwrap_or_default();
            self.record(text, sel, sel, How::Other, false);
        }
    }

    /// Step `i` undone (`undo`) or redone in `text`: what it holds put in
    /// place of what the text holds there, and the selection and file from
    /// before it (undone) or after it.
    fn swap(&mut self, i: usize, text: &mut String, undo: bool) -> Undone {
        let (at, len) = (self.steps[i].at, self.steps[i].len);
        let start = self.char_at(at);
        let step = &mut self.steps[i];
        let out = text[at..at + len].to_owned();
        text.replace_range(at..at + len, &step.kept);
        self.seen.replace_range(at..at + len, &step.kept);
        let end = start + step.kept.chars().count();
        self.stored = self.stored + out.len() - step.kept.len();
        step.len = step.kept.len();
        step.kept = out;
        let sel = if undo { step.before } else { step.after };
        let file = step.file.as_ref().map(|f| if undo { f.before.clone() } else { f.after.clone() });
        self.mark = (at, start);
        self.open = None;
        self.sel = Some(sel);
        Undone { start, end, sel, file }
    }

    /// The character index of byte `b` of `seen` (a character boundary),
    /// counted from the mark.
    fn char_at(&self, b: usize) -> usize {
        let (mb, mc) = self.mark;
        if b >= mb { mc + self.seen[mb..b].chars().count() } else { mc - self.seen[b..mb].chars().count() }
    }

    /// The byte offset in `seen` of character `c`, counted from the mark;
    /// None past the end.
    fn byte_at(&self, c: usize) -> Option<usize> {
        let (mb, mc) = self.mark;
        if c >= mc {
            let rest = &self.seen[mb..];
            rest.char_indices().map(|(i, _)| i).chain(std::iter::once(rest.len())).nth(c - mc).map(|i| mb + i)
        } else {
            self.seen[..mb].char_indices().rev().nth(mc - c - 1).map(|(i, _)| i)
        }
    }

    /// The change from `seen` to `text`, as a new step or as part of the
    /// open one when `still` (no pause, no move since).
    fn record(&mut self, text: &str, before: Sel, after: Sel, how: How, still: bool) {
        self.sel = Some(after);
        let Some((mut p, mut old_end, mut new_end)) = diff(&self.seen, text) else { return };
        // Typed over a selection: all of it was replaced, also where what
        // was typed begins or ends as it did (the comparison alone would
        // take that character for one kept, and see a deletion).
        if how == How::Typed && before.anchor != before.caret {
            let (lo, hi) = (before.anchor.min(before.caret), before.anchor.max(before.caret));
            if let (Some(lb), Some(hb)) = (self.byte_at(lo), self.byte_at(hi))
                && lb <= p
                && old_end <= hb
            {
                (p, old_end, new_end) = (lb, hb, text.len() - (self.seen.len() - hb));
            }
        }
        let (k, l) = (old_end - p, new_end - p);
        let pc = self.char_at(p);
        // The change is found as far on as it can be, but where text
        // repeats (a space typed before a space) it may have been made
        // earlier: where egui left the caret, after what was put in, says.
        let (t, tc) = match after.caret.checked_sub(text[p..new_end].chars().count()) {
            Some(c) if c < pc && (k == 0) != (l == 0) => {
                let (s, n) = if k == 0 { (text, l) } else { (self.seen.as_str(), k) };
                match self.seen[..p].char_indices().rev().nth(pc - c - 1) {
                    Some((t, _)) if slides(s, t, p, n) => (t, c),
                    _ => (p, pc),
                }
            }
            _ => (p, pc),
        };
        // A new edit: what was undone can't be redone.
        self.steps.truncate(self.done);
        let open = if still { self.open.filter(|_| !self.steps.is_empty()) } else { None };
        let removed = &self.seen[t..t + k];
        let caret_only = before.anchor == before.caret;
        let last = self.steps.back();
        if k == 0 && how == How::Typed {
            let join = open == Some(Grow::Typing) && last.is_some_and(|s| s.at + s.len == t);
            self.typed(text, t, tc, l, String::new(), before, after, join);
        } else if how == How::Typed && open == Some(Grow::Typing) && last.is_some_and(|s| s.at <= t && t + k == s.at + s.len) {
            // An input method's composition going on, over what it typed.
            let last = self.steps.back_mut().expect("the open step");
            last.len = last.len - k + l;
            last.after = after;
            // Taken back whole, it leaves nothing to undo.
            if last.len == 0 && last.kept.is_empty() {
                self.steps.pop_back();
                self.done = self.steps.len();
                self.open = None;
            }
        } else if how == How::Typed && l > 0 {
            // Typed over a selection: a step that the rest of the word joins.
            let kept = removed.to_owned();
            self.typed(text, t, tc, l, kept, before, after, false);
        } else if l == 0 && caret_only && how == How::Back(removed.chars().count()) {
            if open == Some(Grow::Back) && last.is_some_and(|s| t + k == s.at) {
                let removed = removed.to_owned();
                let last = self.steps.back_mut().expect("the open step");
                last.kept.insert_str(0, &removed);
                last.at = t;
                last.after = after;
                self.stored += k;
            } else {
                let kept = removed.to_owned();
                self.push(Step { at: t, len: 0, kept, before, after, file: None });
            }
            self.open = Some(Grow::Back);
        } else if l == 0 && caret_only && how == How::Forward(removed.chars().count()) {
            if open == Some(Grow::Forward) && last.is_some_and(|s| t == s.at) {
                let removed = removed.to_owned();
                let last = self.steps.back_mut().expect("the open step");
                last.kept.push_str(&removed);
                last.after = after;
                self.stored += k;
            } else {
                let kept = removed.to_owned();
                self.push(Step { at: t, len: 0, kept, before, after, file: None });
            }
            self.open = Some(Grow::Forward);
        } else {
            let kept = removed.to_owned();
            self.push(Step { at: t, len: l, kept, before, after, file: None });
            self.open = None;
        }
        self.seen.replace_range(t..t + k, &text[t..t + l]);
        self.mark = (t, tc);
        self.trim();
    }

    /// `l` bytes typed at byte `t` (character `tc`) of `text`, over `kept`
    /// (what they replaced): joined to the open step when `join`, up to the
    /// first whitespace typed after something else, or the first ↩; each of
    /// those begins a step.
    #[allow(clippy::too_many_arguments)]
    fn typed(&mut self, text: &str, t: usize, tc: usize, l: usize, mut kept: String, before: Sel, after: Sel, join: bool) {
        let typed = &text[t..t + l];
        // Where each new step begins, in bytes and characters of `typed`.
        let mut starts = Vec::new();
        if !join {
            starts.push((0, 0));
        }
        let mut prev = text[..t].chars().next_back();
        let mut chars = 0;
        for (i, c) in typed.char_indices() {
            if (i > 0 || join) && (c == '\n' || (c.is_whitespace() && prev.is_some_and(|p| !p.is_whitespace()))) {
                starts.push((i, chars));
            }
            prev = Some(c);
            chars += 1;
        }
        let ends = |j: usize| starts.get(j).copied().unwrap_or((l, chars));
        let (first, first_chars) = ends(0);
        if first > 0 {
            let last = self.steps.back_mut().expect("the open step");
            last.len += first;
            last.after = if starts.is_empty() { after } else { Sel::at(tc + first_chars) };
        }
        for j in 0..starts.len() {
            let ((b, c), (e, ec)) = (starts[j], ends(j + 1));
            let step_before = if j == 0 && !join { before } else { Sel::at(tc + c) };
            let step_after = if j + 1 == starts.len() { after } else { Sel::at(tc + ec) };
            let kept = std::mem::take(&mut kept);
            self.push(Step { at: t + b, len: e - b, kept, before: step_before, after: step_after, file: None });
        }
        self.open = Some(Grow::Typing);
    }

    fn push(&mut self, step: Step) {
        self.stored += step.kept.len();
        self.steps.push_back(step);
        self.done = self.steps.len();
    }

    /// The oldest steps dropped while there are too many or they keep too
    /// much; the newest always stays.
    fn trim(&mut self) {
        let (steps, bytes) = self.limits;
        while self.steps.len() > 1 && (self.steps.len() > steps || self.stored > bytes) {
            let Some(old) = self.steps.pop_front() else { break };
            self.stored -= old.kept.len();
            self.done = self.done.saturating_sub(1);
        }
    }
}

/// After an undo or redo, in the text's scroll area: the caret (character
/// `caret`) brought into view, as egui does after typing.
pub fn show_caret(ui: &Ui, out: &TextEditOutput, caret: usize) {
    let origin = out.galley_pos - egui::vec2(out.galley.rect.left(), 0.0);
    ui.scroll_to_rect(out.galley.pos_from_cursor(CCursor::new(caret)).translate(origin.to_vec2()), None);
}

/// Where `new` differs from `old`, if it does: the first byte that differs
/// and the end of the difference in each, all on character boundaries.
/// The difference is as short as it can be, and as far on.
fn diff(old: &str, new: &str) -> Option<(usize, usize, usize)> {
    let (a, b) = (old.as_bytes(), new.as_bytes());
    let mut p = common_prefix(a, b);
    if p == a.len() && p == b.len() {
        return None;
    }
    // A character both start alike but that differs begins before `p`.
    while !old.is_char_boundary(p) {
        p -= 1;
    }
    let mut s = common_suffix(&a[p..], &b[p..]);
    while !old.is_char_boundary(a.len() - s) {
        s -= 1;
    }
    Some((p, a.len() - s, b.len() - s))
}

/// Compared a block at a time (`memcmp`), then a byte at a time.
const BLOCK: usize = 128;

fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    let n = a.len().min(b.len());
    let mut i = 0;
    while i + BLOCK <= n && a[i..i + BLOCK] == b[i..i + BLOCK] {
        i += BLOCK;
    }
    i + a[i..n].iter().zip(&b[i..n]).take_while(|(x, y)| x == y).count()
}

fn common_suffix(a: &[u8], b: &[u8]) -> usize {
    let n = a.len().min(b.len());
    let (a, b) = (&a[a.len() - n..], &b[b.len() - n..]);
    let mut i = 0;
    while i + BLOCK <= n && a[n - i - BLOCK..n - i] == b[n - i - BLOCK..n - i] {
        i += BLOCK;
    }
    i + a[..n - i].iter().rev().zip(b[..n - i].iter().rev()).take_while(|(x, y)| x == y).count()
}

/// Whether `n` bytes put in (or taken out of) `s` at byte `p` could as
/// well have been at byte `t` before it: the text between repeats.
fn slides(s: &str, t: usize, p: usize, n: usize) -> bool {
    s.is_char_boundary(t) && s.is_char_boundary(t + n) && s.as_bytes()[t..p] == s.as_bytes()[t + n..p + n]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A text being typed into, with its history: `t` seconds a step.
    struct Typist {
        text: String,
        sel: Sel,
        h: History,
        now: f64,
    }

    impl Typist {
        fn new(text: &str, caret: usize) -> Self {
            Self { text: text.into(), sel: Sel::at(caret), h: History::new(text), now: 10.0 }
        }

        fn byte(&self, chars: usize) -> usize {
            self.text.char_indices().nth(chars).map_or(self.text.len(), |(i, _)| i)
        }

        /// The selection's characters replaced by `s`, as one edit made `how`.
        fn put(&mut self, s: &str, how: How) {
            let (a, b) = (self.sel.anchor.min(self.sel.caret), self.sel.anchor.max(self.sel.caret));
            let (ab, bb) = (self.byte(a), self.byte(b));
            let before = self.sel;
            self.text.replace_range(ab..bb, s);
            self.sel = Sel::at(a + s.chars().count());
            self.h.edited(&self.text, before, self.sel, how, self.now);
        }

        /// Types `s` a character at a time, `dt` seconds apart.
        fn type_slowly(&mut self, s: &str, dt: f64) {
            for c in s.chars() {
                self.now += dt;
                self.put(&c.to_string(), How::Typed);
            }
        }

        fn type_in(&mut self, s: &str) {
            self.type_slowly(s, 0.15);
        }

        /// ⌫ (`back`) or ⌦ pressed once, 0.1 s on.
        fn delete(&mut self, back: bool) {
            self.now += 0.1;
            let before = self.sel;
            let c = self.sel.caret;
            let (from, to) = if back { (c.saturating_sub(1), c) } else { (c, (c + 1).min(self.text.chars().count())) };
            let (fb, tb) = (self.byte(from), self.byte(to));
            self.text.replace_range(fb..tb, "");
            self.sel = Sel::at(from);
            let how = if back { How::Back(1) } else { How::Forward(1) };
            self.h.edited(&self.text, before, self.sel, how, self.now);
        }

        /// The caret or selection moved, 0.1 s on.
        fn select(&mut self, anchor: usize, caret: usize) {
            self.now += 0.1;
            self.sel = Sel { anchor, caret };
            self.h.moved(self.sel);
        }

        /// ⌘Z: the text and the selection after it.
        fn undo(&mut self) -> Option<(String, Sel)> {
            let u = self.h.undo(&mut self.text)?;
            self.sel = u.sel;
            Some((self.text.clone(), u.sel))
        }

        fn redo(&mut self) -> Option<(String, Sel)> {
            let u = self.h.redo(&mut self.text)?;
            self.sel = u.sel;
            Some((self.text.clone(), u.sel))
        }

        /// ⌘Z until nothing is left: each text on the way.
        fn undo_all(&mut self) -> Vec<String> {
            std::iter::from_fn(|| self.undo().map(|(t, _)| t)).collect()
        }
    }

    fn texts(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// Typed without a pause, a query comes off a word at a time: a step
    /// is a run of spaces and the word after it (punctuation doesn't split
    /// one), and the caret goes back to the end of what is left. Redo
    /// walks forward again, the caret at the end of each step.
    #[test]
    fn typing_steps_back_a_word_at_a_time() {
        let mut t = Typist::new("", 0);
        t.type_in("db.a.find({ name: 1 })");
        let chars = |s: &str| Sel::at(s.chars().count());
        let want = ["db.a.find({ name: 1", "db.a.find({ name:", "db.a.find({", ""];
        for w in want {
            assert_eq!(t.undo(), Some((w.to_string(), chars(w))));
        }
        assert_eq!(t.undo(), None, "nothing left to undo");
        assert_eq!(t.text, "");
        for w in ["db.a.find({", "db.a.find({ name:", "db.a.find({ name: 1", "db.a.find({ name: 1 })"] {
            assert_eq!(t.redo(), Some((w.to_string(), chars(w))));
        }
        assert_eq!(t.redo(), None, "nothing left to redo");
    }

    /// ↩ always begins a step, spaces after it or not; so does a pause of
    /// a second, even in the middle of a word.
    #[test]
    fn enter_and_a_pause_begin_steps() {
        let mut t = Typist::new("", 0);
        t.type_in("print(1)\nprint(2)  \n  x");
        assert_eq!(t.undo_all(), texts(&["print(1)\nprint(2)  ", "print(1)\nprint(2)", "print(1)", ""]));

        let mut t = Typist::new("", 0);
        t.type_in("db.coll");
        t.now += 1.0;
        t.type_in("ection");
        assert_eq!(t.undo_all(), texts(&["db.coll", ""]));
    }

    /// A run of ⌫ is one step, a run of ⌦ another; switching between
    /// typing and deleting, or between ⌫ and ⌦, begins a new one.
    #[test]
    fn delete_runs_are_steps_and_switching_begins_one() {
        let mut t = Typist::new("abcdef", 6);
        t.delete(true);
        t.delete(true);
        t.type_in("XY");
        t.delete(true);
        assert_eq!(t.text, "abcdX");
        assert_eq!(t.undo(), Some(("abcdXY".into(), Sel::at(6))));
        assert_eq!(t.undo(), Some(("abcd".into(), Sel::at(4))));
        assert_eq!(t.undo(), Some(("abcdef".into(), Sel::at(6))), "the ⌫ run, the caret where it began");

        let mut t = Typist::new("abcdef", 2);
        t.delete(false);
        t.delete(false);
        assert_eq!(t.text, "abef");
        t.delete(true);
        t.delete(false);
        assert_eq!(t.text, "af");
        assert_eq!(t.undo_all(), texts(&["aef", "abef", "abcdef"]));
    }

    /// Typing after the caret was moved begins a step, even if it moved
    /// back to where it was. Typing over a selection begins one too, and
    /// the rest of the word typed after it joins it (as on macOS): one ⌘Z
    /// puts back what was replaced, selected again.
    #[test]
    fn a_caret_moved_or_a_selection_typed_over_begins_a_step() {
        let mut t = Typist::new("", 0);
        t.type_in("abc");
        t.select(0, 0);
        t.type_in("X");
        t.select(3, 3);
        t.select(1, 1);
        t.type_in("Y");
        assert_eq!(t.text, "XYabc");
        assert_eq!(t.undo_all(), texts(&["Xabc", "abc", ""]), "moved away and back still begins a step");

        let mut t = Typist::new("hello world", 11);
        t.select(6, 11);
        t.type_in("there");
        assert_eq!(t.text, "hello there");
        assert_eq!(t.undo(), Some(("hello world".into(), Sel { anchor: 6, caret: 11 })), "one step, the selection back");
        assert_eq!(t.undo(), None);
        assert_eq!(t.redo(), Some(("hello there".into(), Sel::at(11))));

        // Also when what is typed begins or ends as the selection did, which
        // the text before and after alone can't tell from typing beside it.
        for (select, typed, want) in [((6, 11), "wonder", "hello wonder"), ((6, 11), "dog", "hello dog"), ((6, 11), "wd", "hello wd")] {
            let mut t = Typist::new("hello world", 11);
            t.select(select.0, select.1);
            t.type_in(typed);
            assert_eq!(t.text, want);
            assert_eq!(t.undo(), Some(("hello world".into(), Sel { anchor: 6, caret: 11 })), "{typed:?} over \"world\": one step");
            assert_eq!(t.undo(), None, "{typed:?}");
        }
        let mut t = Typist::new("name: \"Ann\"", 11);
        t.select(6, 11);
        t.type_in("\"Bob\"");
        assert_eq!(t.text, "name: \"Bob\"");
        assert_eq!(t.undo(), Some(("name: \"Ann\"".into(), Sel { anchor: 6, caret: 11 })), "\"Bob\" over \"Ann\", quotes and all: one step");

        // The next word is a step of its own, as ever.
        let mut t = Typist::new("hello world", 11);
        t.select(11, 6);
        t.type_in("there you");
        assert_eq!(t.undo_all(), texts(&["hello there", "hello world"]));
        assert_eq!(t.sel, Sel { anchor: 11, caret: 6 }, "selected as it was, the caret at its end");
    }

    /// A paste, or anything else that isn't typing, is a step of its own:
    /// it doesn't join the word before it, and typing after it begins
    /// another. So is a deletion of several characters at once.
    #[test]
    fn a_paste_is_a_step_of_its_own() {
        let mut t = Typist::new("", 0);
        t.type_in("print(");
        t.now += 0.1;
        t.put("db.a.find()", How::Other);
        t.type_in(")");
        assert_eq!(t.undo_all(), texts(&["print(db.a.find()", "print(", ""]));

        // ⌥⌫ takes a word: a step, even right after a ⌫.
        let mut t = Typist::new("db.a.find", 9);
        t.delete(true);
        t.now += 0.1;
        let before = t.sel;
        t.text = "db.a.".into();
        t.sel = Sel::at(5);
        t.h.edited(&t.text, before, t.sel, How::Back(1), t.now);
        assert_eq!(t.undo_all(), texts(&["db.a.fin", "db.a.find"]));
    }

    /// Load file of a file holding the very text on screen changes only
    /// the file: still a step, whose undo gives the file before.
    #[test]
    fn a_change_of_file_alone_is_a_step() {
        let a = PathBuf::from("/tmp/a.js");
        let mut t = Typist::new("x", 1);
        t.h.replaced_file("x", Sel::at(1), Sel::at(0), None, Some(a.clone()));
        let u = t.h.undo(&mut t.text).expect("a step");
        assert_eq!((t.text.as_str(), u.file), ("x", Some(None)));
        assert_eq!(t.h.redo(&mut t.text).expect("redone").file, Some(Some(a.clone())));
        // The same text and the same file: nothing to undo.
        let mut t = Typist::new("x", 1);
        t.h.replaced_file("x", Sel::at(1), Sel::at(0), Some(a.clone()), Some(a));
        assert_eq!(t.undo(), None);
    }

    /// A new edit after an undo leaves nothing to redo.
    #[test]
    fn an_edit_after_an_undo_clears_the_redo() {
        let mut t = Typist::new("", 0);
        t.type_in("one two");
        t.undo();
        t.type_in("x");
        assert_eq!(t.text, "onex");
        assert_eq!(t.redo(), None);
        assert_eq!(t.undo_all(), texts(&["one", ""]));
    }

    /// Thai, several characters to a cell (and sharing their first bytes),
    /// typed and undone, gives back exactly the text before, never a
    /// broken character.
    #[test]
    fn thai_undoes_cleanly() {
        let start = "db.x.find({ name: \"\" })";
        let mut t = Typist::new(start, 19);
        t.type_in("สวัสดี ครับ ข้");
        assert_eq!(t.text, "db.x.find({ name: \"สวัสดี ครับ ข้\" })");
        assert_eq!(t.undo_all(), texts(&["db.x.find({ name: \"สวัสดี ครับ\" })", "db.x.find({ name: \"สวัสดี\" })", start]));
        // ข replaced by ค over a selection: bytes in common, never split.
        let mut t = Typist::new("กขค", 3);
        t.select(1, 2);
        t.type_in("ค");
        t.delete(true);
        t.delete(true);
        assert_eq!(t.text, "ค");
        assert_eq!(t.undo_all(), texts(&["กคค", "กขค"]));
    }

    /// When what was typed repeats the text beside it (a space before a
    /// space, "a" before "a"), the edit is still found where it was made:
    /// the word joins its step, and ⌫ / ⌦ runs stay one step.
    #[test]
    fn repeated_text_still_joins_its_step() {
        let mut t = Typist::new("foo bar", 3);
        t.type_in(" x");
        assert_eq!(t.text, "foo x bar");
        assert_eq!(t.undo_all(), texts(&["foo bar"]));

        let mut t = Typist::new("aaaa))", 4);
        t.type_in("a)");
        assert_eq!(t.undo_all(), texts(&["aaaa))"]));

        let mut t = Typist::new("aaaa", 4);
        t.delete(true);
        t.delete(true);
        t.delete(true);
        assert_eq!(t.undo_all(), texts(&["aaaa"]));
        let mut t = Typist::new("aaaa", 0);
        t.delete(false);
        t.delete(false);
        assert_eq!(t.undo_all(), texts(&["aaaa"]));
    }

    /// A difference is found on character boundaries, also where two
    /// characters share their first bytes (ข and ค differ in the last).
    #[test]
    fn differences_never_split_a_character() {
        assert_eq!(diff("abc", "abc"), None);
        assert_eq!(diff("กขค", "กคค"), Some((3, 6, 6)));
        assert_eq!(diff("ข", "ค"), Some((0, 3, 3)));
        assert_eq!(diff("ab", "a"), Some((1, 2, 1)));
        assert_eq!(diff("", "ไทย"), Some((0, 0, 9)));
        // Longer than a block: found past it, from both ends.
        let long = "x".repeat(1000);
        assert_eq!(diff(&format!("{long}a{long}"), &format!("{long}bb{long}")), Some((1000, 1001, 1002)));
    }

    /// An input method's composition (preedit, then the committed text)
    /// is one step with what was typed before it.
    #[test]
    fn a_composition_is_undone_whole() {
        let mut t = Typist::new("", 0);
        t.type_in("x ");
        let compose = |t: &mut Typist, steps: &[(&str, Sel)]| {
            for &(text, sel) in steps {
                t.now += 0.1;
                let before = t.sel;
                t.text = text.into();
                t.sel = sel;
                t.h.edited(&t.text, before, sel, How::Typed, t.now);
            }
        };
        compose(&mut t, &[("x に", Sel { anchor: 2, caret: 3 }), ("x にほ", Sel { anchor: 2, caret: 4 }), ("x 日本", Sel::at(4))]);
        assert_eq!(t.undo_all(), texts(&["x", ""]));

        // A composition taken back whole leaves no step that undoes nothing.
        let mut t = Typist::new("x", 1);
        t.now += 5.0;
        compose(&mut t, &[("xに", Sel { anchor: 1, caret: 2 }), ("x", Sel::at(1))]);
        assert_eq!(t.undo(), None);
    }

    /// Typing into a 5,000-line script (about 550 KB), the history keeps
    /// what changed, never a copy of the script: 25 words typed add a few
    /// kilobytes, where a copy a step would be over ten megabytes.
    #[test]
    fn typing_into_a_long_script_keeps_no_copies_of_it() {
        let script: String = (0..5_000).map(|i| format!("db.getCollection(\"games\").updateOne({{ _id: {i} }}, {{ $set: {{ name: \"สล็อต {i}\", rtp: {}.5 }} }}) // line {i}\n", i % 97)).collect();
        let middle = script[..script.len() / 2].chars().count();
        let mut t = Typist::new(&script, middle);
        t.text.reserve(4096);
        // The history's copy of the text grows its room on the first edits.
        t.type_in("abc ");
        let live = crate::heap::live();
        t.type_in(&"word ".repeat(25));
        let grown = crate::heap::live() - live;
        assert!(grown < 64 * 1024, "the history grew by {grown} bytes for 125 characters typed");
        // "abc", " word" 25 times, and the last space.
        assert_eq!(t.undo_all().len(), 27, "a step a word");
        assert_eq!(t.text, script);
    }

    /// At most so many steps, and about so much text kept, the oldest
    /// going first; the newest stays however large it is.
    #[test]
    fn the_history_is_capped() {
        let mut t = Typist::new("", 0);
        t.h = History::with_limits("", 5, 1_000);
        t.type_in("a b c d e f g h");
        assert_eq!(t.undo_all().len(), 5, "five steps kept");
        assert_eq!(t.text, "a b c");

        let big = "y".repeat(600);
        let mut t = Typist::new(&format!("{big}{big}"), 0);
        t.h = History::with_limits(&t.text, 100, 1_000);
        for _ in 0..2 {
            t.select(0, 600);
            t.now += 0.1;
            t.put("", How::Back(1));
        }
        assert_eq!(t.text, "");
        assert_eq!(t.undo_all(), texts(&[&big]), "the older 600 bytes went");

        let huge = "z".repeat(5_000);
        let mut t = Typist::new(&huge, 0);
        t.h = History::with_limits(&huge, 100, 1_000);
        t.select(0, 5_000);
        t.now += 0.1;
        t.put("", How::Other);
        assert_eq!(t.undo_all(), texts(&[&huge]), "the newest stays, however large");
    }
}
