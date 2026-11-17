//! Find: the find bar the JSON view and the document dialogs share, the
//! JSON view's find state and its search on other threads, and the matcher.

use std::sync::Arc;

use egui::text::{CCursor, CCursorRange};
use egui::{Key, RichText, Ui};
use egui_phosphor::regular as icon;

use super::json_text::TextLines;
use super::thousands;
use crate::{shortcut, theme};

/// Most matches a find bar collects.
pub(crate) const MAX_MATCHES: usize = 100_000;

/// What a find bar says after its buttons.
pub(crate) enum Status {
    /// Nothing typed: nothing said.
    Empty,
    Searching,
    /// Match `current` (from 0) of `total`; "No matches" for none.
    Count { current: usize, total: usize },
}

/// A find bar, as the JSON view and the document dialogs show it: the
/// magnifier, the field, match case (**Aa**), previous / next, what was
/// found, and close.
pub(crate) struct Bar<'a> {
    /// The field's id.
    pub(crate) id: egui::Id,
    pub(crate) hint: &'a str,
    pub(crate) text: &'a mut String,
    pub(crate) case: &'a mut bool,
    /// Focus the field and select what is typed in it (⌘F); taken.
    pub(crate) focus: &'a mut bool,
    /// There are matches to step through: ↑ and ↓ are enabled.
    pub(crate) any: bool,
    pub(crate) status: Status,
}

/// What a find bar was asked this frame.
#[derive(Default)]
pub(crate) struct Asked {
    /// -1 the previous match, 1 the next, 0 neither.
    pub(crate) step: i64,
    pub(crate) close: bool,
}

impl Bar<'_> {
    /// ↩ and ⇧↩ in the field step through the matches (the field keeps its
    /// focus), Esc in it closes the bar, as ✕ does.
    pub(crate) fn show(self, ui: &mut Ui) -> Asked {
        let Bar { id, hint, text, case, focus, any, status } = self;
        let mut asked = Asked::default();
        ui.horizontal(|ui| {
            ui.label(RichText::new(icon::MAGNIFYING_GLASS).color(theme::MUTED));
            if std::mem::take(focus) {
                // Focused before it is drawn: a field drawn without the
                // focus drops its selection.
                ui.memory_mut(|m| m.request_focus(id));
                let mut state = egui::TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
                state.cursor.set_char_range(Some(CCursorRange::two(CCursor::new(0), CCursor::new(text.chars().count()))));
                state.store(ui.ctx(), id);
            }
            let r = ui.add(egui::TextEdit::singleline(text).id(id).hint_text(hint).desired_width(280.0));
            // Read before the focus is asked back: after `request_focus`
            // in the same frame, `lost_focus` is never true.
            if r.lost_focus() {
                if ui.input(|i| i.key_pressed(Key::Enter)) {
                    asked.step = if ui.input(|i| i.modifiers.shift) { -1 } else { 1 };
                    r.request_focus();
                } else if ui.input(|i| i.key_pressed(Key::Escape)) {
                    asked.close = true;
                }
            }
            if ui.selectable_label(*case, "Aa").on_hover_text("Match case").clicked() {
                *case = !*case;
            }
            if ui.add_enabled(any, egui::Button::new(icon::CARET_UP)).on_hover_text(shortcut::label("Previous match (⇧↩)")).clicked() {
                asked.step = -1;
            }
            if ui.add_enabled(any, egui::Button::new(icon::CARET_DOWN)).on_hover_text(shortcut::label("Next match (↩)")).clicked() {
                asked.step = 1;
            }
            match status {
                Status::Empty => {}
                Status::Searching => _ = ui.weak("Searching…"),
                Status::Count { total: 0, .. } => _ = ui.colored_label(theme::ERROR, "No matches"),
                Status::Count { current, total } => {
                    let more = if total >= MAX_MATCHES { "+" } else { "" };
                    ui.weak(format!("{} of {}{more}", thousands(current as u64 + 1), thousands(total as u64)));
                }
            }
            if ui.button(icon::X).on_hover_text("Close (Esc)").clicked() {
                asked.close = true;
            }
        });
        asked
    }
}

/// The JSON view's find bar (⌘F).
#[derive(Default)]
pub(super) struct Find {
    pub(super) text: String,
    pub(super) case: bool,
    /// (line, start, end) byte offsets of each match, in order.
    pub(super) matches: Vec<(usize, usize, usize)>,
    /// The text and case `matches` is for; `None` searches again.
    pub(super) searched: Option<(String, bool)>,
    pub(super) current: usize,
    pub(super) focus: bool,
    pub(super) scroll_to: bool,
    pub(super) running: Option<Search>,
}

/// A search on a background thread: two and a half million lines take a
/// few ms (~30 ms for a needle with cased non-ASCII letters, each line then
/// lowercased), which a keystroke's frame shouldn't wait for. Dropping it
/// stops the thread.
pub(super) struct Search {
    pub(super) key: (String, bool),
    pub(super) rx: std::sync::mpsc::Receiver<Vec<(usize, usize, usize)>>,
    cancel: Arc<std::sync::atomic::AtomicBool>,
}

impl Search {
    pub(super) fn start(lines: Arc<TextLines>, key: (String, bool), ctx: egui::Context) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (k, stop) = (key.clone(), cancel.clone());
        std::thread::spawn(move || {
            let found = find_matches(&lines, &k.0, k.1, &stop);
            if tx.send(found).is_ok() {
                ctx.request_repaint();
            }
        });
        Self { key, rx, cancel }
    }
}

impl Drop for Search {
    fn drop(&mut self) {
        self.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Where `needle` occurs in the JSON lines, as (line, start, end), leaving
/// out the `// n` document number lines. Stops at MAX_MATCHES, or with
/// nothing when `cancel` is set. Ignoring case, a line is matched as if
/// each of its characters were lowercased on its own (a match must start
/// and end on the line's own characters). Big texts are searched on
/// several threads, by line range.
pub(super) fn find_matches(lines: &TextLines, needle: &str, case: bool, cancel: &std::sync::atomic::AtomicBool) -> Vec<(usize, usize, usize)> {
    find_matches_in(lines, needle, case, cancel, threads_for(lines), true)
}

/// Where `needle` occurs in `text`, as byte ranges, in order: the document
/// dialogs' find. It matches as the JSON view's does (ignoring case unless
/// `case`; a needle with a line break finds nothing), but searches every
/// line: in a document, a line starting with "// " is its own text. At
/// most MAX_MATCHES.
pub(crate) fn find_in_text(text: &str, needle: &str, case: bool) -> Vec<std::ops::Range<usize>> {
    let mut lines = TextLines::default();
    lines.push_text(text);
    let go = std::sync::atomic::AtomicBool::new(false);
    let found = find_matches_in(&lines, needle, case, &go, threads_for(&lines), false);
    found.into_iter().map(|(line, start, end)| lines.starts[line] + start..lines.starts[line] + end).collect()
}

/// Threads to search `lines` on: about 65,000 lines a thread, as fewer
/// aren't worth starting one for.
fn threads_for(lines: &TextLines) -> usize {
    std::thread::available_parallelism().map_or(1, |p| p.get()).min(lines.len().div_ceil(65_536))
}

/// `find_matches` on `threads` threads, each searching its own range of
/// lines for at most MAX_MATCHES; their matches are joined in order.
/// `numbered`: the text is the JSON view's, whose `// n` lines (document
/// numbers) are not searched.
fn find_matches_in(lines: &TextLines, needle: &str, case: bool, cancel: &std::sync::atomic::AtomicBool, threads: usize, numbered: bool) -> Vec<(usize, usize, usize)> {
    // A line never holds a line break.
    if needle.is_empty() || needle.contains('\n') {
        return Vec::new();
    }
    let lower = needle.to_lowercase();
    let search = Finder::new(needle, &lower, case, numbered);
    let n = lines.len();
    let threads = threads.clamp(1, n.max(1));
    let per = n.div_ceil(threads);
    let parts: Vec<Option<Vec<(usize, usize, usize)>>> = if threads == 1 {
        vec![search.range(lines, 0..n, cancel)]
    } else {
        std::thread::scope(|s| {
            let search = &search;
            let running: Vec<_> = (0..threads).map(|t| s.spawn(move || search.range(lines, t * per..((t + 1) * per).min(n), cancel))).collect();
            running.into_iter().map(|t| t.join().unwrap_or_else(|e| std::panic::resume_unwind(e))).collect()
        })
    };
    let mut out = Vec::new();
    for part in parts {
        let Some(found) = part else { return Vec::new() };
        out.extend(found);
        if out.len() >= MAX_MATCHES {
            out.truncate(MAX_MATCHES);
            break;
        }
    }
    out
}

/// Lines searched between looks at the cancel flag.
const FIND_BLOCK: usize = 4096;

/// Whether case changes `c`: it has an upper or lower case form.
fn cased(c: char) -> bool {
    c.is_lowercase() || c.is_uppercase() || !c.to_lowercase().eq([c]) || !c.to_uppercase().eq([c])
}

/// How a needle is compared with the lines.
#[derive(Clone, Copy, PartialEq)]
enum Fold {
    /// Byte for byte: matching case, or a needle with nothing cased in it
    /// (digits, Thai, Chinese, …), which only ever matches itself.
    Exact,
    /// An ASCII needle, ignoring ASCII case. No other character's bytes
    /// equal an ASCII byte, so non-ASCII lines are searched this way too.
    Ascii,
    /// Any other needle, ignoring case: each line lowercased (`fold_line`).
    Unicode,
}

/// One search: the needle, how it is compared, and what it looks for.
struct Finder<'a> {
    needle: &'a str,
    lower: &'a str,
    fold: Fold,
    exact: memchr::memmem::Finder<'a>,
    /// Where in `lower` the byte is that an ASCII search looks for first:
    /// the one least common in JSON text.
    anchor: usize,
    /// A line holding one of these is lowercased (`fold_line`), as before:
    /// İ, which lowercases into two characters (a match can't end between
    /// them), and, for an ASCII needle, the Kelvin sign, which lowercases
    /// into "k".
    special: Vec<memchr::memmem::Finder<'static>>,
    /// The `// n` lines are left out (the JSON view's document numbers).
    numbered: bool,
}

/// Buffers `fold_line` and the ASCII search reuse from line to line.
#[derive(Default)]
struct FindBuf {
    low: String,
    starts: Vec<(usize, usize)>,
    hits: Vec<usize>,
}

impl<'a> Finder<'a> {
    fn new(needle: &'a str, lower: &'a str, case: bool, numbered: bool) -> Self {
        let fold = if case || !needle.chars().any(cased) {
            Fold::Exact
        } else if needle.is_ascii() {
            Fold::Ascii
        } else {
            Fold::Unicode
        };
        let dotted_i = || memchr::memmem::Finder::new("\u{130}".as_bytes()).into_owned();
        let kelvin = || memchr::memmem::Finder::new("\u{212A}".as_bytes()).into_owned();
        let special = match fold {
            Fold::Exact if !case => vec![dotted_i()],
            Fold::Ascii => vec![dotted_i(), kelvin()],
            _ => Vec::new(),
        };
        let anchor = lower.bytes().enumerate().min_by_key(|&(_, b)| commonness(b)).map_or(0, |(k, _)| k);
        Self { needle, lower, fold, exact: memchr::memmem::Finder::new(needle.as_bytes()), anchor, special, numbered }
    }

    /// The matches in lines `range`, at most MAX_MATCHES; None when cancelled.
    fn range(&self, lines: &TextLines, range: std::ops::Range<usize>, cancel: &std::sync::atomic::AtomicBool) -> Option<Vec<(usize, usize, usize)>> {
        let mut out = Vec::new();
        let mut buf = FindBuf::default();
        let mut from = range.start;
        while from < range.end {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return None;
            }
            let to = (from + FIND_BLOCK).min(range.end);
            match self.fold {
                Fold::Unicode => (from..to).for_each(|i| self.fold_line(lines, i, &mut buf, &mut out)),
                Fold::Exact | Fold::Ascii => self.block(lines, from..to, &mut buf, &mut out),
            }
            if out.len() >= MAX_MATCHES {
                out.truncate(MAX_MATCHES);
                break;
            }
            from = to;
        }
        Some(out)
    }

    /// A block of lines searched as one stretch of the text (no needle
    /// holds a line break, so no match spans two lines), each hit then put
    /// on its line. Lines holding a `special` character are lowercased
    /// instead.
    fn block(&self, lines: &TextLines, block: std::ops::Range<usize>, buf: &mut FindBuf, out: &mut Vec<(usize, usize, usize)>) {
        let text = lines.text.as_bytes();
        let begin = lines.starts[block.start];
        let end = lines.starts.get(block.end).map_or(text.len(), |s| s - 1);
        let hay = &text[begin..end];
        let line_of = |cur: &mut usize, at: usize| {
            while *cur + 1 < block.end && lines.starts[*cur + 1] <= at {
                *cur += 1;
            }
            *cur
        };
        let mut special = Vec::new();
        for f in &self.special {
            let mut cur = block.start;
            special.extend(f.find_iter(hay).map(|at| line_of(&mut cur, begin + at)));
        }
        special.sort_unstable();
        special.dedup();
        buf.hits.clear();
        match self.fold {
            Fold::Exact => buf.hits.extend(self.exact.find_iter(hay)),
            _ => ascii_fold_find(hay, self.lower.as_bytes(), self.anchor, &mut buf.hits),
        }
        let len = self.needle.len();
        let (mut cur, mut next) = (block.start, 0);
        let hits = std::mem::take(&mut buf.hits);
        for &at in &hits {
            let line = line_of(&mut cur, begin + at);
            while next < special.len() && special[next] < line {
                self.fold_line(lines, special[next], buf, out);
                next += 1;
            }
            if special.get(next) == Some(&line) || (self.numbered && lines.line(line).starts_with("// ")) {
                continue;
            }
            let at = begin + at - lines.starts[line];
            out.push((line, at, at + len));
        }
        buf.hits = hits;
        for &line in &special[next..] {
            self.fold_line(lines, line, buf, out);
        }
    }

    /// Line `i` searched ignoring case as find always did: an ASCII line
    /// byte by byte, ignoring ASCII case; any other lowercased a character
    /// at a time, with offsets mapped back to the line's own characters
    /// (lowercasing can change a character's length: İ, ẞ). A match that
    /// starts or ends inside one character's lowercase form is dropped.
    fn fold_line(&self, lines: &TextLines, i: usize, buf: &mut FindBuf, out: &mut Vec<(usize, usize, usize)>) {
        let line = lines.line(i);
        if self.numbered && line.starts_with("// ") {
            return;
        }
        if line.is_ascii() {
            // A needle that lowercases into anything but ASCII can't match.
            if self.lower.is_ascii() {
                buf.hits.clear();
                ascii_fold_find(line.as_bytes(), self.lower.as_bytes(), self.anchor, &mut buf.hits);
                out.extend(buf.hits.iter().map(|&at| (i, at, at + self.lower.len())));
            }
            return;
        }
        buf.low.clear();
        buf.starts.clear();
        for (at, c) in line.char_indices() {
            buf.starts.push((buf.low.len(), at));
            buf.low.extend(c.to_lowercase());
        }
        buf.starts.push((buf.low.len(), line.len()));
        let starts = &buf.starts;
        let orig = |at: usize| starts.binary_search_by_key(&at, |p| p.0).ok().map(|k| starts[k].1);
        for (at, m) in buf.low.match_indices(self.lower) {
            if let (Some(s), Some(e)) = (orig(at), orig(at + m.len())) {
                out.push((i, s, e));
            }
        }
    }
}

/// Where `pat` (lowercase ASCII) occurs in `hay` ignoring ASCII case, left
/// to right without overlaps, as starts. Candidates are found by looking
/// for the byte at `anchor` in either case.
fn ascii_fold_find(hay: &[u8], pat: &[u8], anchor: usize, out: &mut Vec<usize>) {
    let n = pat.len();
    if n == 0 || hay.len() < n {
        return;
    }
    let (lo, up) = (pat[anchor].to_ascii_lowercase(), pat[anchor].to_ascii_uppercase());
    let mut from = anchor;
    while from < hay.len() {
        let found = if lo == up { memchr::memchr(lo, &hay[from..]) } else { memchr::memchr2(lo, up, &hay[from..]) };
        let Some(k) = found else { break };
        let start = from + k - anchor;
        if start + n > hay.len() {
            break;
        }
        if hay[start..start + n].eq_ignore_ascii_case(pat) {
            out.push(start);
            from = start + n + anchor;
        } else {
            from += k + 1;
        }
    }
}

/// Roughly how common a byte is in JSON text (either case), to look for
/// a needle by its least common byte.
fn commonness(b: u8) -> u8 {
    match b.to_ascii_lowercase() {
        b' ' => 255,
        b'"' => 250,
        b'e' => 240,
        b't' => 230,
        b'a' => 225,
        b'o' => 220,
        b'i' => 215,
        b'n' => 210,
        b's' => 205,
        b'r' => 200,
        b',' | b':' => 195,
        b'h' => 190,
        b'l' => 185,
        b'd' => 180,
        b'c' => 175,
        b'u' => 170,
        b'm' => 165,
        b'0'..=b'9' => 160,
        b'f' => 150,
        b'p' => 145,
        b'g' => 140,
        b'w' => 135,
        b'y' => 130,
        b'b' => 125,
        b'.' | b'/' | b'_' | b'-' => 120,
        b'v' => 110,
        b'k' => 105,
        b'x' | b'j' | b'q' | b'z' => 90,
        _ => 50,
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    #[test]
    fn find_matches_in_json_lines() {
        let lines = TextLines::from_lines(&["// 1", "{", r#"    "brand" : "DGM","#, r#"    "group" : "dgm-DGM","#, r#"    "name" : "ไทย DGM""#, "}"]);
        let go = std::sync::atomic::AtomicBool::new(false);
        assert_eq!(find_matches(&lines, "dgm", false, &go), [(2, 15, 18), (3, 15, 18), (3, 19, 22), (4, 24, 27)]);
        assert_eq!(find_matches(&lines, "DGM", true, &go), [(2, 15, 18), (3, 19, 22), (4, 24, 27)]);
        // The document number lines are not searched.
        assert_eq!(find_matches(&lines, "1", false, &go), []);
        assert_eq!(find_matches(&lines, "", false, &go), []);
        assert_eq!(find_matches(&lines, "ไทย", false, &go), [(4, 14, 23)]);
        // İ lowercases to 3 bytes and ẞ to 2: the line keeps its length but
        // offsets shift. Matches still land on the line's own characters.
        let tricky = TextLines::from_lines(&["ẞİx İ ab"]);
        let found = find_matches(&tricky, "x", false, &go);
        assert_eq!(found, [(0, 5, 6)]);
        for needle in ["İ", "i", "ß", "b", "x i"] {
            for &(_, s, e) in &find_matches(&tricky, needle, false, &go) {
                assert!(tricky.line(0).is_char_boundary(s) && tricky.line(0).is_char_boundary(e), "{needle}: {s}..{e}");
            }
        }
        // "x i" ends inside İ's lowercase form ("i" + combining dot): dropped.
        assert_eq!(find_matches(&tricky, "x i", false, &go), []);
        assert_eq!(find_matches(&tricky, "x i\u{307}", false, &go), [(0, 5, 9)]);
    }

    /// The document dialogs' find searches every line, "// " lines too
    /// (in a document they are its own text), and answers byte ranges
    /// into the text, in order, by the JSON view's rules.
    #[test]
    fn find_in_text_searches_every_line() {
        let text = "// 1 note\n{\n    a: \"ไทย DGM\",\n    b: \"dgm-x\"\n}";
        let at = |s: &str| text.find(s).unwrap_or_else(|| panic!("{s} in the text"));
        let found = |needle: &str, case: bool| find_in_text(text, needle, case).into_iter().map(|r| (r.start, r.end)).collect::<Vec<_>>();
        let (upper, lower) = (at("DGM"), at("dgm"));
        assert_eq!(found("dgm", false), [(upper, upper + 3), (lower, lower + 3)]);
        assert_eq!(found("DGM", true), [(upper, upper + 3)]);
        assert_eq!(found("1 note", false), [(3, 9)], "the comment line is searched");
        assert_eq!(found("ไทย d", false), [(at("ไทย"), upper + 1)]);
        assert!(find_in_text(text, "", false).is_empty());
        assert!(find_in_text(text, "\",\n    b", false).is_empty(), "a needle with a line break finds nothing");
        let many = "ab ".repeat(MAX_MATCHES + 10);
        assert_eq!(find_in_text(&many, "AB", false).len(), MAX_MATCHES);
    }

    /// `find_matches` as it was before its fast paths: every line on its
    /// own, a non-ASCII line lowercased a character at a time. The fast
    /// paths must find exactly what this finds.
    pub(in crate::result_view) fn find_matches_before(lines: &TextLines, needle: &str, case: bool) -> Vec<(usize, usize, usize)> {
        find_matches_before_in(lines, needle, case, true)
    }

    /// `find_matches_before`, leaving the `// n` lines out or not (`numbered`).
    fn find_matches_before_in(lines: &TextLines, needle: &str, case: bool, numbered: bool) -> Vec<(usize, usize, usize)> {
        let mut out = Vec::new();
        if needle.is_empty() {
            return out;
        }
        let lower = needle.to_lowercase();
        for i in 0..lines.len() {
            let line = lines.line(i);
            if numbered && line.starts_with("// ") {
                continue;
            }
            if case {
                out.extend(line.match_indices(needle).map(|(at, m)| (i, at, at + m.len())));
            } else if line.is_ascii() {
                let (h, n) = (line.as_bytes(), lower.as_bytes());
                let mut at = 0;
                while at + n.len() <= h.len() {
                    if h[at..at + n.len()].eq_ignore_ascii_case(n) {
                        out.push((i, at, at + n.len()));
                        at += n.len();
                    } else {
                        at += 1;
                    }
                }
            } else {
                let mut low = String::with_capacity(line.len());
                let mut starts = Vec::new();
                for (at, c) in line.char_indices() {
                    starts.push((low.len(), at));
                    low.extend(c.to_lowercase());
                }
                starts.push((low.len(), line.len()));
                let orig = |at: usize| starts.binary_search_by_key(&at, |p| p.0).ok().map(|k| starts[k].1);
                for (at, m) in low.match_indices(&lower) {
                    if let (Some(s), Some(e)) = (orig(at), orig(at + m.len())) {
                        out.push((i, s, e));
                    }
                }
            }
            if out.len() >= MAX_MATCHES {
                out.truncate(MAX_MATCHES);
                break;
            }
        }
        out
    }

    /// Lines in many scripts, with the characters whose case changes their
    /// length or turns them into ASCII: İ (into "i" and a combining dot), K
    /// (the Kelvin sign, into "k"), ẞ, Å (the Angstrom sign), final sigma.
    fn mixed_script_lines(n: usize) -> TextLines {
        let samples = [
            r#"    "name" : "Golden Dragon Slots 4321 name","#,
            r#"    "th" : "สล็อตมังกรทองคำ แจ็คพอตแตกง่าย 4321 — สล็อตมังกรทองคำ","#,
            r#"    "zh" : "金龙老虎机 大奖 4321","#,
            r#"    "ja" : "ゴールデンドラゴンスロット 4321 title","#,
            r#"    "ko" : "골든 드래곤 슬롯 잭팟 4321","#,
            r#"    "vi" : "Rồng Vàng Slot Nổ Hũ 4321 — RỒNG VÀNG","#,
            "    \"vi2\" : \"Ro\u{302}\u{300}ng Va\u{300}ng (combining marks)\",",
            "    \"tr\" : \"\u{130}STANBUL \u{130}stanbul istanbul \u{131}stanbul \u{130}\u{130} i\u{307}\",",
            "    \"kelvin\" : \"273 \u{212A} is 0 \u{B0}C; 1 \u{212A} = 1 k? KELVIN \u{212A}k\",",
            "    \"angstrom\" : \"1 \u{212B} = 0.1 nm; \u{C5} \u{E5} \u{212B}\",",
            r#"    "de" : "Straße STRASSE Größe GRÖẞE ß ẞ ss","#,
            r#"    "el" : "ΟΔΥΣΣΕΥΣ Οδυσσεύς σς Σ ΣΑΣ","#,
            r#"    "ru" : "Золотой Дракон ЗОЛОТОЙ дракон","#,
            r#"    "emoji" : "🐉 dragon 🐉🐉 DRAGON 🎰","#,
            r#"    "img" : "https://cdn.example/img/game/th/G04321_2.webp","#,
            r#"    "price" : 96.5,"#,
            "// 7",
            "",
            "{",
            "}",
            "    \"mixed\" : \"\u{212A}\u{130} \u{212A}\u{130}\u{212A} \u{130}\u{212A} k\u{307} \u{130}\u{307} i\u{307}i\u{307} \u{130}\u{307}\u{307}x\",",
        ];
        let lines: Vec<String> = (0..n).map(|i| samples[i % samples.len()].replace("4321", &format!("{}", i % 997))).collect();
        TextLines::from_lines(&lines)
    }

    /// The fast byte paths and the threads find what the line-by-line
    /// search found: the same lines, offsets and order, case-insensitive
    /// and not, for needles in every script on the lines above; and with
    /// the `// n` lines searched too, as the document dialogs search.
    #[test]
    fn find_matches_equal_the_line_by_line_search() {
        // Smaller in debug builds, where the line-by-line search is slow;
        // still more than one block of lines (FIND_BLOCK). The release run
        // (`--release`) takes the full corpus.
        let debug = cfg!(debug_assertions);
        let lines = mixed_script_lines(if debug { 5_000 } else { 10_000 });
        let go = std::sync::atomic::AtomicBool::new(false);
        let mut needles: Vec<String> = [
            "golden", "GOLDEN", "Golden Dragon", "dragon", "4321", "12", "96.5", "webp", ".", "\"", " ", ": \"",
            "สล็อต", "แตกง่าย 1", "มังกร", "金龙", "ドラゴン", "골든", "rồng", "RỒNG", "nổ hũ", "va\u{300}ng", "\u{302}\u{300}",
            "i", "I", "\u{130}", "i\u{307}", "\u{307}", "\u{307}\u{307}", "istanbul", "\u{130}STANBUL", "\u{130}stanbul", "\u{131}", "k", "K", "\u{212A}", "kelvin", "kk", "\u{212A}\u{130}",
            "\u{E5}", "\u{C5}", "\u{212B}", "\u{DF}", "\u{1E9E}", "ss", "SS", "gr\u{F6}\u{DF}e", "\u{3C3}", "\u{3A3}", "\u{3C2}", "\u{3BF}\u{3B4}\u{3C5}\u{3C3}\u{3C3}\u{3B5}\u{3C5}\u{3C2}", "\u{434}\u{440}\u{430}\u{43A}\u{43E}\u{43D}", "\u{414}\u{420}\u{410}\u{41A}\u{41E}\u{41D}", "\u{1F409}", "\u{1F409} d", "// 7", "7",
            "{", "}", "k\u{130}k", "\u{130}k", "k\u{307}", "\n", "a\nb",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        // Pieces of the lines themselves, cased at random.
        let mut seed = 12345u64;
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as usize
        };
        for _ in 0..if debug { 60 } else { 120 } {
            let line: Vec<char> = lines.line(next() % 40).chars().collect();
            if line.is_empty() {
                continue;
            }
            let a = next() % line.len();
            let len = 1 + next() % 6;
            let piece: String = line[a..(a + len).min(line.len())]
                .iter()
                .map(|&c| match next() % 3 {
                    0 => c.to_uppercase().next().unwrap_or(c),
                    1 => c.to_lowercase().next().unwrap_or(c),
                    _ => c,
                })
                .collect();
            needles.push(piece);
        }
        for needle in &needles {
            for case in [false, true] {
                let before = find_matches_before(&lines, needle, case);
                for threads in [1, 3, 8] {
                    assert_eq!(find_matches_in(&lines, needle, case, &go, threads, true), before, "{needle:?} case={case} on {threads} threads");
                }
                assert_eq!(find_matches(&lines, needle, case, &go), before, "{needle:?} case={case}");
                let every_line = find_matches_before_in(&lines, needle, case, false);
                assert_eq!(find_matches_in(&lines, needle, case, &go, 3, false), every_line, "{needle:?} case={case}, every line");
            }
        }
        // More than MAX_MATCHES: the first ones, in order.
        let lines = mixed_script_lines(if debug { 15_000 } else { 30_000 });
        let many = find_matches_before(&lines, " ", false);
        assert_eq!(many.len(), MAX_MATCHES);
        assert_eq!(find_matches_in(&lines, " ", false, &go, 5, true), many);
    }

    /// What the fast paths rely on, checked on every character of this
    /// Rust's Unicode tables: only İ lowercases into more than one
    /// character; only İ and the Kelvin sign lowercase into ASCII from
    /// outside it; and a character that lowercases into another one always
    /// becomes something with case (so a needle without case can only
    /// match itself).
    #[test]
    fn case_mappings_are_what_find_assumes() {
        for c in (0..=0x10FFFFu32).filter_map(char::from_u32) {
            let lower: Vec<char> = c.to_lowercase().collect();
            if c != '\u{130}' {
                assert_eq!(lower.len(), 1, "{c:?} lowercases into {lower:?}");
            }
            if !c.is_ascii() && lower.iter().any(char::is_ascii) {
                assert!(c == '\u{130}' || c == '\u{212A}', "{c:?} lowercases into ASCII {lower:?}");
            }
            if lower != [c] {
                assert!(lower.iter().any(|&l| cased(l)), "{c:?} lowercases into {lower:?}, none of it cased");
            }
        }
    }
}
