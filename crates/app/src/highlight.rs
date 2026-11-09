//! A small JavaScript tokenizer for colouring the shell editor and the JSON
//! view. It only has to be good enough for colour, not for parsing.

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId};

use crate::theme;

const KEYWORDS: &[&str] = &[
    "const", "let", "var", "function", "return", "for", "of", "in", "while", "do", "if", "else", "true", "false",
    "null", "undefined", "new", "break", "continue", "try", "catch", "finally", "throw", "typeof", "instanceof",
    "class", "this", "switch", "case", "default", "async", "await", "use", "show", "NaN", "Infinity",
];

const BUILTINS: &[&str] = &[
    "db", "ObjectId", "ISODate", "NumberLong", "NumberInt", "NumberDecimal", "Double", "UUID", "BinData",
    "Timestamp", "MinKey", "MaxKey", "print", "printjson", "sleep", "console", "Date", "Math", "JSON",
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Token {
    Plain,
    Keyword,
    Builtin,
    Function,
    String,
    Number,
    Comment,
    Key,
}

fn color(t: Token) -> Color32 {
    match t {
        Token::Plain => theme::TEXT,
        Token::Keyword => theme::KEYWORD,
        Token::Builtin => theme::FUNCTION,
        Token::Function => theme::FUNCTION,
        Token::String => theme::STRING,
        Token::Number => theme::NUMBER,
        Token::Comment => theme::COMMENT,
        Token::Key => Color32::from_rgb(0x9e, 0xc6, 0xf3),
    }
}

/// Split `src` into (byte range, token) spans covering all of it.
pub fn tokenize(src: &str) -> Vec<(std::ops::Range<usize>, Token)> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut plain_start = 0;
    let flush = |out: &mut Vec<_>, from: usize, to: usize| {
        if to > from {
            out.push((from..to, Token::Plain));
        }
    };
    while i < b.len() {
        let c = b[i];
        let start = i;
        let tok = if c == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            Some(Token::Comment)
        } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
            i += 2;
            while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                i += 1;
            }
            i = (i + 2).min(b.len());
            Some(Token::Comment)
        } else if c == b'"' || c == b'\'' || c == b'`' {
            i += 1;
            while i < b.len() && b[i] != c {
                if b[i] == b'\n' && c != b'`' {
                    break;
                }
                if b[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            if i < b.len() && b[i] == c {
                i += 1;
            }
            i = i.min(b.len());
            // A string followed by `:` is an object key.
            let rest = src[i..].trim_start_matches([' ', '\t']);
            Some(if rest.starts_with(':') { Token::Key } else { Token::String })
        } else if c.is_ascii_digit() || (c == b'-' && b.get(i + 1).is_some_and(u8::is_ascii_digit) && prev_allows_sign(b, i)) {
            i += 1;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.' || b[i] == b'_') {
                i += 1;
            }
            Some(Token::Number)
        } else if c.is_ascii_alphabetic() || c == b'_' || c == b'$' {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$') {
                i += 1;
            }
            let word = &src[start..i];
            let rest = src[i..].trim_start_matches([' ', '\t']);
            Some(if KEYWORDS.contains(&word) {
                Token::Keyword
            } else if BUILTINS.contains(&word) {
                Token::Builtin
            } else if rest.starts_with('(') {
                Token::Function
            } else if rest.starts_with(':') && !rest.starts_with("::") {
                Token::Key
            } else {
                Token::Plain
            })
        } else {
            i += src[i..].chars().next().map_or(1, char::len_utf8);
            None
        };
        if let Some(t) = tok {
            flush(&mut out, plain_start, start);
            out.push((start..i, t));
            plain_start = i;
        }
    }
    flush(&mut out, plain_start, b.len());
    out
}

fn prev_allows_sign(b: &[u8], i: usize) -> bool {
    b[..i].iter().rev().find(|c| !c.is_ascii_whitespace()).is_none_or(|c| b"(:,[{=+-*/!<>?&|".contains(c))
}

pub fn layout_job(src: &str, font: FontId, wrap_width: f32) -> LayoutJob {
    let mut job = LayoutJob::default();
    for (range, tok) in tokenize(src) {
        job.append(&src[range], 0.0, TextFormat { font_id: font.clone(), color: color(tok), ..Default::default() });
    }
    job.wrap.max_width = wrap_width;
    job
}

/// Give byte ranges of `job`'s text a background colour (find matches). The
/// ranges must not overlap and must fall on character boundaries.
pub fn mark(job: &mut LayoutJob, marks: &[(std::ops::Range<usize>, Color32)]) {
    if marks.is_empty() {
        return;
    }
    let span = |s: &egui::text::LayoutSection| s.byte_range.start.0..s.byte_range.end.0;
    let mut cuts: Vec<usize> = job.sections.iter().map(span).chain(marks.iter().map(|(r, _)| r.clone())).flat_map(|r| [r.start, r.end]).collect();
    cuts.sort_unstable();
    cuts.dedup();
    let old = std::mem::take(&mut job.sections);
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        let Some(s) = old.iter().find(|s| span(s).contains(&a)) else { continue };
        let mut format = s.format.clone();
        if let Some((_, c)) = marks.iter().find(|(r, _)| r.contains(&a)) {
            format.background = *c;
        }
        let leading_space = if a == span(s).start { s.leading_space } else { 0.0 };
        let byte_range = egui::text::ByteIndex(a)..egui::text::ByteIndex(b);
        job.sections.push(egui::text::LayoutSection { leading_space, byte_range, format });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<(&str, Token)> {
        tokenize(src).into_iter().filter(|(_, t)| *t != Token::Plain).map(|(r, t)| (&src[r], t)).collect()
    }

    #[test]
    fn marks_split_sections_and_keep_their_colours() {
        let src = r#""brand" : "DGM","#;
        let mut job = layout_job(src, FontId::monospace(13.0), f32::INFINITY);
        let span = |s: &egui::text::LayoutSection| s.byte_range.start.0..s.byte_range.end.0;
        let before: Vec<_> = job.sections.iter().map(|s| (span(s), s.format.color)).collect();
        mark(&mut job, &[(3..5, Color32::RED), (11..14, Color32::BLUE)]);
        let text: String = job.sections.iter().map(|s| &src[span(s)]).collect();
        assert_eq!(text, src, "sections still cover the text in order");
        let bg = |at: usize| job.sections.iter().find(|s| span(s).contains(&at)).unwrap().format.background;
        assert_eq!((bg(3), bg(4), bg(5), bg(12), bg(14)), (Color32::RED, Color32::RED, Color32::TRANSPARENT, Color32::BLUE, Color32::TRANSPARENT));
        for (r, c) in before {
            assert!(job.sections.iter().filter(|s| r.contains(&span(s).start)).all(|s| s.format.color == c));
        }
    }

    #[test]
    fn tokens() {
        assert_eq!(
            kinds(r#"db.users.find({ age: { $gt: -5 } }) // hi"#),
            [("db", Token::Builtin), ("find", Token::Function), ("age", Token::Key), ("$gt", Token::Key), ("-5", Token::Number), ("// hi", Token::Comment)]
        );
        assert_eq!(kinds(r#""k" : 'v'"#), [("\"k\"", Token::Key), ("'v'", Token::String)]);
        assert_eq!(kinds("a - 1"), [("1", Token::Number)]);
        let total: usize = tokenize("x = \"unterminated\n y").iter().map(|(r, _)| r.len()).sum();
        assert_eq!(total, "x = \"unterminated\n y".len());
    }
}
