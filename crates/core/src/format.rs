//! Turning BSON into text: compact cell text for tables, and mongosh-style
//! source text (`ObjectId("…")`, `ISODate("…")`) for the JSON view, the shell
//! output, and the document editor.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicI64, Ordering};

use bson::spec::BinarySubtype;
use bson::{Bson, Document};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TypeKind {
    ObjectId,
    String,
    Int32,
    Int64,
    Double,
    Decimal,
    Bool,
    Date,
    Null,
    Array,
    Object,
    Binary,
    Regex,
    Timestamp,
    Code,
    Other,
}

impl TypeKind {
    pub fn label(self) -> &'static str {
        match self {
            TypeKind::ObjectId => "ObjectId",
            TypeKind::String => "String",
            TypeKind::Int32 => "Int32",
            TypeKind::Int64 => "Int64",
            TypeKind::Double => "Double",
            TypeKind::Decimal => "Decimal128",
            TypeKind::Bool => "Boolean",
            TypeKind::Date => "Date",
            TypeKind::Null => "Null",
            TypeKind::Array => "Array",
            TypeKind::Object => "Object",
            TypeKind::Binary => "Binary",
            TypeKind::Regex => "Regex",
            TypeKind::Timestamp => "Timestamp",
            TypeKind::Code => "Code",
            TypeKind::Other => "Other",
        }
    }
}

pub fn kind(v: &Bson) -> TypeKind {
    match v {
        Bson::ObjectId(_) => TypeKind::ObjectId,
        Bson::String(_) | Bson::Symbol(_) => TypeKind::String,
        Bson::Int32(_) => TypeKind::Int32,
        Bson::Int64(_) => TypeKind::Int64,
        Bson::Double(_) => TypeKind::Double,
        Bson::Decimal128(_) => TypeKind::Decimal,
        Bson::Boolean(_) => TypeKind::Bool,
        Bson::DateTime(_) => TypeKind::Date,
        Bson::Null | Bson::Undefined => TypeKind::Null,
        Bson::Array(_) => TypeKind::Array,
        Bson::Document(_) => TypeKind::Object,
        Bson::Binary(_) => TypeKind::Binary,
        Bson::RegularExpression(_) => TypeKind::Regex,
        Bson::Timestamp(_) => TypeKind::Timestamp,
        Bson::JavaScriptCode(_) | Bson::JavaScriptCodeWithScope(_) => TypeKind::Code,
        _ => TypeKind::Other,
    }
}

const CELL_MAX: usize = 200;

/// One-line text for a table cell.
pub fn cell_text(v: &Bson) -> String {
    match v {
        Bson::String(s) | Bson::Symbol(s) => {
            // The first line's first CELL_MAX characters, found without
            // reading further: a long one-line string would be read whole.
            let mut end = s.len();
            for (n, (i, c)) in s.char_indices().enumerate() {
                if c == '\n' || n == CELL_MAX {
                    end = i;
                    break;
                }
            }
            let mut line = &s[..end];
            // A line ending in "\r\n" loses its '\r', as with `str::lines`.
            if s[end..].starts_with('\n') {
                line = line.strip_suffix('\r').unwrap_or(line);
            }
            let mut out = String::with_capacity(line.len() + 3);
            out.push_str(line);
            if out.len() < s.len() {
                out.push('…');
            }
            out
        }
        Bson::Array(a) => match a.len() {
            1 => "[ 1 element ]".into(),
            n => format!("[ {n} elements ]"),
        },
        Bson::Document(d) => match d.len() {
            1 => "{ 1 field }".into(),
            n => format!("{{ {n} fields }}"),
        },
        Bson::ObjectId(oid) => {
            let mut out = String::with_capacity(24);
            push_hex(&mut out, &oid.bytes());
            out
        }
        Bson::DateTime(dt) => iso_date(*dt),
        Bson::Double(f) => {
            let mut out = String::new();
            write_display_double(&mut out, *f);
            out
        }
        Bson::Int32(i) => i.to_string(),
        Bson::Int64(i) => i.to_string(),
        Bson::Decimal128(d) => d.to_string(),
        Bson::Boolean(b) => b.to_string(),
        Bson::Null => "null".into(),
        Bson::Undefined => "undefined".into(),
        Bson::Binary(b) if b.subtype == BinarySubtype::Uuid && b.bytes.len() == 16 => {
            let mut out = String::with_capacity(36);
            push_uuid(&mut out, &b.bytes);
            out
        }
        Bson::Binary(b) => format!("Binary ({} bytes)", b.bytes.len()),
        Bson::RegularExpression(r) => format!("/{}/{}", r.pattern, r.options),
        Bson::Timestamp(t) => format!("Timestamp({}, {})", t.time, t.increment),
        Bson::JavaScriptCode(c) => c.lines().next().unwrap_or("").to_string(),
        Bson::JavaScriptCodeWithScope(c) => c.code.lines().next().unwrap_or("").to_string(),
        Bson::MinKey => "MinKey".into(),
        Bson::MaxKey => "MaxKey".into(),
        other => to_shell(other, Style::Display),
    }
}

/// A double for reading: a whole one as `5.0`.
fn write_display_double(out: &mut String, f: f64) {
    let _ = if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e16 { write!(out, "{f:.1}") } else { write!(out, "{f}") };
}

/// Lowercase hex digits of `bytes`.
pub(crate) fn push_hex(out: &mut String, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for b in bytes {
        out.push(HEX[usize::from(b >> 4)] as char);
        out.push(HEX[usize::from(b & 15)] as char);
    }
}

/// 16 bytes as a UUID: `8-4-4-4-12` hex digits.
fn push_uuid(out: &mut String, b: &[u8]) {
    for (i, part) in [&b[0..4], &b[4..6], &b[6..8], &b[8..10], &b[10..16]].into_iter().enumerate() {
        if i > 0 {
            out.push('-');
        }
        push_hex(out, part);
    }
}

/// Time zone dates are shown in. Every form is still an exact instant, so
/// text like `ISODate("2026-01-02T10:00:00.000+07:00")` reads back unchanged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "minutes", rename_all = "snake_case")]
pub enum DisplayTz {
    #[default]
    Utc,
    /// The computer's zone, daylight saving included.
    Local,
    /// A fixed offset from UTC, in minutes (+420 for UTC+07:00).
    Offset(i32),
}

const TZ_UTC: i64 = i64::MIN;
const TZ_LOCAL: i64 = i64::MIN + 1;
static DISPLAY_TZ: AtomicI64 = AtomicI64::new(TZ_UTC);

/// Set the zone used by [`iso_date`] everywhere (tables, JSON, the shell).
pub fn set_display_tz(tz: DisplayTz) {
    let v = match tz {
        DisplayTz::Utc => TZ_UTC,
        DisplayTz::Local => TZ_LOCAL,
        DisplayTz::Offset(m) => i64::from(m),
    };
    DISPLAY_TZ.store(v, Ordering::Relaxed);
}

pub fn display_tz() -> DisplayTz {
    match DISPLAY_TZ.load(Ordering::Relaxed) {
        TZ_UTC => DisplayTz::Utc,
        TZ_LOCAL => DisplayTz::Local,
        m => DisplayTz::Offset(m as i32),
    }
}

/// The computer's current UTC offset, e.g. `+07:00`.
pub fn local_offset_label() -> String {
    chrono::Local::now().format("%:z").to_string()
}

/// A date in the display zone, RFC 3339 with milliseconds as mongosh prints it.
pub fn iso_date(dt: bson::DateTime) -> String {
    iso_date_in(dt, display_tz())
}

/// A date as shell text: `ISODate("…")` in zone `tz`, or `new Date(ms)` when
/// the year shown there isn't four digits (ISODate can't read it back:
/// 9999-12-31T23:00Z is +10000-01-01 at +07:00).
fn write_date_literal(out: &mut String, dt: bson::DateTime, tz: DisplayTz) {
    let ms = dt.timestamp_millis();
    if let Some(utc) = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms) {
        out.push_str("ISODate(\"");
        if write_iso_fast(out, utc, tz) {
            out.push_str("\")");
            return;
        }
        out.truncate(out.len() - "ISODate(\"".len());
        let text = iso_date_slow(utc, tz);
        if text.len() > 4 && text.as_bytes()[..4].iter().all(u8::is_ascii_digit) && text.as_bytes()[4] == b'-' {
            let _ = write!(out, "ISODate(\"{text}\")");
            return;
        }
    }
    let _ = write!(out, "new Date({ms})");
}

/// A date in UTC regardless of the display setting (for exports).
pub fn iso_date_utc(dt: bson::DateTime) -> String {
    iso_date_in(dt, DisplayTz::Utc)
}

pub fn iso_date_in(dt: bson::DateTime, tz: DisplayTz) -> String {
    let mut out = String::with_capacity(29);
    push_iso_date(&mut out, dt, tz);
    out
}

/// `iso_date_in`'s text, appended to `out`.
pub(crate) fn push_iso_date(out: &mut String, dt: bson::DateTime, tz: DisplayTz) {
    let ms = dt.timestamp_millis();
    match chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms) {
        Some(utc) => {
            if !write_iso_fast(out, utc, tz) {
                out.push_str(&iso_date_slow(utc, tz));
            }
        }
        None => {
            let _ = write!(out, "{ms}");
        }
    }
}

/// RFC 3339 with milliseconds in zone `tz`, through chrono's formatter:
/// what `write_iso_fast` leaves to it.
fn iso_date_slow(utc: chrono::DateTime<chrono::Utc>, tz: DisplayTz) -> String {
    match tz {
        DisplayTz::Utc => utc.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
        DisplayTz::Local => utc.with_timezone(&chrono::Local).format("%Y-%m-%dT%H:%M:%S%.3f%:z").to_string(),
        DisplayTz::Offset(m) => match chrono::FixedOffset::east_opt(m * 60) {
            Some(off) => utc.with_timezone(&off).format("%Y-%m-%dT%H:%M:%S%.3f%:z").to_string(),
            None => utc.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
        },
    }
}

/// `iso_date_slow`'s text, written digit by digit (no format string to
/// parse, no allocation), for a year of four digits in a zone a whole
/// number of minutes from UTC; false, with nothing written, otherwise.
fn write_iso_fast(out: &mut String, utc: chrono::DateTime<chrono::Utc>, tz: DisplayTz) -> bool {
    use chrono::{Datelike, Offset, Timelike};
    let offset_secs = match tz {
        DisplayTz::Utc => None,
        DisplayTz::Local => Some(utc.with_timezone(&chrono::Local).offset().fix().local_minus_utc()),
        DisplayTz::Offset(m) => chrono::FixedOffset::east_opt(m * 60).map(|o| o.local_minus_utc()),
    };
    let off = offset_secs.unwrap_or(0);
    if off % 60 != 0 {
        return false;
    }
    let Some(local) = chrono::FixedOffset::east_opt(off).map(|o| utc.with_timezone(&o)) else { return false };
    if !(0..=9999).contains(&local.year()) {
        return false;
    }
    let digits = |out: &mut String, n: u32, width: u32| {
        for p in (0..width).rev() {
            out.push(char::from(b'0' + (n / 10u32.pow(p) % 10) as u8));
        }
    };
    digits(out, local.year() as u32, 4);
    for (sep, n) in [('-', local.month()), ('-', local.day()), ('T', local.hour()), (':', local.minute()), (':', local.second())] {
        out.push(sep);
        digits(out, n, 2);
    }
    out.push('.');
    digits(out, local.timestamp_subsec_millis().min(999), 3);
    match offset_secs {
        None => out.push('Z'),
        Some(s) => {
            out.push(if s < 0 { '-' } else { '+' });
            let m = s.unsigned_abs() / 60;
            digits(out, m / 60, 2);
            out.push(':');
            digits(out, m % 60, 2);
        }
    }
    true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    /// For reading: doubles print as `5.0`.
    Display,
    /// For editing: the text evaluates back to the same BSON types. Whole
    /// doubles print as `Double(5)`, since `5.0` in JavaScript is just `5`.
    Editable,
}

/// mongosh-style source for a single value, on one line.
pub fn to_shell(v: &Bson, style: Style) -> String {
    let mut out = String::new();
    write_value(&mut out, v, style, None, 0);
    out
}

/// mongosh-style source for a document, indented over several lines.
pub fn doc_to_shell(d: &Document, style: Style) -> String {
    let mut out = String::new();
    write_doc(&mut out, d, style, Some(4), 0);
    out
}

pub fn value_to_shell_pretty(v: &Bson, style: Style) -> String {
    let mut out = String::new();
    write_value(&mut out, v, style, Some(4), 0);
    out
}

fn write_doc(out: &mut String, d: &Document, style: Style, indent: Option<usize>, level: usize) {
    if d.is_empty() {
        out.push_str("{ }");
        return;
    }
    out.push('{');
    let n = d.len();
    for (i, (k, v)) in d.iter().enumerate() {
        newline(out, indent, level + 1);
        write_string(out, k);
        out.push_str(" : ");
        write_value(out, v, style, indent, level + 1);
        if i + 1 < n {
            out.push(',');
        }
    }
    newline(out, indent, level);
    out.push('}');
}

fn newline(out: &mut String, indent: Option<usize>, level: usize) {
    const SPACES: &str = "                                                                ";
    match indent {
        Some(w) => {
            out.push('\n');
            let mut n = w * level;
            while n > 0 {
                let k = n.min(SPACES.len());
                out.push_str(&SPACES[..k]);
                n -= k;
            }
        }
        None => out.push(' '),
    }
}

fn write_value(out: &mut String, v: &Bson, style: Style, indent: Option<usize>, level: usize) {
    match v {
        Bson::Document(d) => write_doc(out, d, style, indent, level),
        Bson::Array(a) => {
            if a.is_empty() {
                out.push_str("[ ]");
                return;
            }
            out.push('[');
            for (i, item) in a.iter().enumerate() {
                newline(out, indent, level + 1);
                write_value(out, item, style, indent, level + 1);
                if i + 1 < a.len() {
                    out.push(',');
                }
            }
            newline(out, indent, level);
            out.push(']');
        }
        Bson::String(s) => write_string(out, s),
        Bson::Symbol(s) => {
            out.push_str("BSONSymbol(");
            write_string(out, s);
            out.push(')');
        }
        Bson::Int32(i) => {
            let _ = write!(out, "{i}");
        }
        Bson::Int64(i) => {
            let _ = write!(out, "NumberLong(\"{i}\")");
        }
        Bson::Double(f) => write_double(out, *f, style),
        Bson::Decimal128(d) => {
            let _ = write!(out, "NumberDecimal(\"{d}\")");
        }
        Bson::Boolean(b) => {
            let _ = write!(out, "{b}");
        }
        Bson::Null => out.push_str("null"),
        Bson::Undefined => out.push_str("undefined"),
        Bson::ObjectId(oid) => {
            out.push_str("ObjectId(\"");
            push_hex(out, &oid.bytes());
            out.push_str("\")");
        }
        Bson::DateTime(dt) => write_date_literal(out, *dt, display_tz()),
        Bson::Binary(b) if b.subtype == BinarySubtype::Uuid && b.bytes.len() == 16 => {
            out.push_str("UUID(\"");
            push_uuid(out, &b.bytes);
            out.push_str("\")");
        }
        Bson::Binary(b) => {
            let _ = write!(out, "BinData({}, \"", u8::from(b.subtype));
            push_base64(out, &b.bytes);
            out.push_str("\")");
        }
        Bson::RegularExpression(r) => {
            let (pattern, options) = (r.pattern.as_str(), r.options.as_str());
            if regex_literal_reads_back(pattern, options) {
                let _ = write!(out, "/{pattern}/{options}");
            } else {
                // Two quoted strings: data, whatever the pattern holds.
                out.push_str("BSONRegExp(");
                write_string(out, pattern);
                out.push_str(", ");
                write_string(out, options);
                out.push(')');
            }
        }
        Bson::Timestamp(t) => {
            let _ = write!(out, "Timestamp({{ t: {}, i: {} }})", t.time, t.increment);
        }
        Bson::JavaScriptCode(c) => {
            out.push_str("Code(");
            write_string(out, c);
            out.push(')');
        }
        Bson::JavaScriptCodeWithScope(c) => {
            out.push_str("Code(");
            write_string(out, &c.code);
            out.push_str(", ");
            write_doc(out, &c.scope, style, None, 0);
            out.push(')');
        }
        Bson::MinKey => out.push_str("MinKey()"),
        Bson::MaxKey => out.push_str("MaxKey()"),
        Bson::DbPointer(p) => {
            // Its parts are private to the bson crate; Extended JSON has them.
            let ejson = Bson::DbPointer(p.clone()).into_canonical_extjson();
            out.push_str("DBPointer(");
            write_string(out, ejson["$dbPointer"]["$ref"].as_str().unwrap_or_default());
            out.push_str(", ObjectId(");
            write_string(out, ejson["$dbPointer"]["$id"]["$oid"].as_str().unwrap_or_default());
            out.push_str("))");
        }
    }
}

fn write_double(out: &mut String, f: f64, style: Style) {
    if f.is_nan() {
        out.push_str(if style == Style::Editable { "Double(NaN)" } else { "NaN" });
    } else if f.is_infinite() {
        let s = if f > 0.0 { "Infinity" } else { "-Infinity" };
        if style == Style::Editable {
            let _ = write!(out, "Double({s})");
        } else {
            out.push_str(s);
        }
    } else if f.fract() == 0.0 {
        match style {
            Style::Display => write_display_double(out, f),
            Style::Editable => {
                let _ = write!(out, "Double({f})");
            }
        }
    } else {
        let _ = write!(out, "{f}");
    }
}

/// Whether `/pattern/options` reads back in the shell as exactly this regex.
/// The text this module writes is evaluated again (the document editor, the
/// Update/Delete dialogs, Paste), and the pattern comes from the server: one
/// that ends the literal early (a line break, a `[` with no `]`, a trailing
/// backslash) would have whatever follows read as code. `//` and `/*` start
/// comments, a `/` comes back escaped, and JS has no x or l option.
///
/// A pattern JavaScript refuses would make the whole text fail to
/// evaluate, and the server holds PCRE patterns, unchecked. So this only
/// says yes to what JS surely reads: no PCRE group syntax (`(?i)`,
/// `(?P<n>…)`; only `(?:` and lookarounds), balanced groups, quantifiers
/// that follow something repeatable (not `a++`, `^*`, `{2}x`), ranges in
/// order (`a{2,1}`, `[z-a]`), and with the u option, which is strict
/// about escapes and braces, none of `\ { } ]` outside a class. Anything
/// else is written as `BSONRegExp("…", "…")`, which always reads back.
fn regex_literal_reads_back(pattern: &str, options: &str) -> bool {
    if pattern.is_empty() || pattern.contains(['/', '\n', '\r', '\u{2028}', '\u{2029}']) {
        return false;
    }
    let mut flags = String::new();
    let flags_ok = options.chars().all(|c| {
        let ok = "imsu".contains(c) && !flags.contains(c);
        flags.push(c);
        ok
    });
    let unicode = options.contains('u');
    if !flags_ok || (unicode && pattern.contains(['\\', '{', '}'])) {
        return false;
    }
    let p: Vec<char> = pattern.chars().collect();
    // Open groups (whether each is a lookaround), and whether what came
    // last can take a quantifier.
    let (mut groups, mut atom, mut i) = (Vec::new(), false, 0);
    while i < p.len() {
        match p[i] {
            '\\' => {
                let Some(&next) = p.get(i + 1) else { return false };
                atom = !matches!(next, 'b' | 'B');
                i += 1;
            }
            '[' => match class_end(&p, i + 1) {
                Some(end) => {
                    i = end;
                    atom = true;
                }
                None => return false,
            },
            ']' if unicode => return false,
            '(' => {
                let rest: String = p[i + 1..].iter().take(4).collect();
                let prefix = ["?:", "?=", "?!", "?<=", "?<!"].into_iter().find(|s| rest.starts_with(s));
                if rest.starts_with('?') && prefix.is_none() {
                    return false;
                }
                groups.push(prefix.is_some_and(|s| s != "?:"));
                i += prefix.map_or(0, str::len);
                atom = false;
            }
            ')' => match groups.pop() {
                Some(lookaround) => atom = !lookaround,
                None => return false,
            },
            '|' | '^' | '$' => atom = false,
            c @ ('*' | '+' | '?' | '{') => {
                let len = if c == '{' {
                    match braced_quantifier(&p[i..]) {
                        Some(Some(len)) => len,
                        Some(None) => return false,
                        // A `{` that is no quantifier is a plain character.
                        None => {
                            atom = true;
                            i += 1;
                            continue;
                        }
                    }
                } else {
                    1
                };
                if !atom {
                    return false;
                }
                i += len - 1;
                // A lazy `?` may follow; nothing else repeats it again.
                if p.get(i + 1) == Some(&'?') {
                    i += 1;
                }
                atom = false;
            }
            _ => atom = true,
        }
        i += 1;
    }
    groups.is_empty()
}

/// The index of the `]` that ends a class whose contents start at `from`,
/// if it ends and its ranges are in order. A range with an escape at either
/// end isn't checked, so it counts as not in order.
fn class_end(p: &[char], from: usize) -> Option<usize> {
    let mut i = from + usize::from(p.get(from) == Some(&'^'));
    // The class's items: a character, an escape (None), or a `-`.
    let mut items: Vec<Option<char>> = Vec::new();
    let end = loop {
        match *p.get(i)? {
            ']' => break i,
            '\\' => {
                p.get(i + 1)?;
                items.push(None);
                i += 1;
            }
            c => items.push(Some(c)),
        }
        i += 1;
    };
    let mut k = 0;
    while k + 2 < items.len() {
        if items[k + 1] == Some('-') {
            match (items[k], items[k + 2]) {
                (Some(lo), Some(hi)) if lo <= hi => k += 3,
                _ => return None,
            }
        } else {
            k += 1;
        }
    }
    Some(end)
}

/// A `{n}`, `{n,}` or `{n,m}` quantifier at the start of `p`: its length,
/// or None inside when its numbers are out of order or too large. None
/// when it is no quantifier (JS then reads `{` as a character).
fn braced_quantifier(p: &[char]) -> Option<Option<usize>> {
    let digits = |from: usize| p[from..].iter().take_while(|c| c.is_ascii_digit()).count();
    let number = |from: usize, n: usize| p[from..from + n].iter().collect::<String>().parse::<u32>().ok().filter(|v| *v < i32::MAX as u32);
    let n = digits(1);
    if n == 0 {
        return None;
    }
    let (mut len, min) = (1 + n, number(1, n));
    let max = if p.get(len) == Some(&',') {
        let m = digits(len + 1);
        let max = if m == 0 { Some(u32::MAX) } else { number(len + 1, m) };
        len += 1 + m;
        max
    } else {
        min
    };
    if p.get(len) != Some(&'}') {
        return None;
    }
    Some(match (min, max) {
        (Some(lo), Some(hi)) if lo <= hi => Some(len + 1),
        _ => None,
    })
}

fn write_string(out: &mut String, s: &str) {
    write_json_string(out, s);
}

/// `s` as a quoted JSON string, escaped exactly as serde_json does (which
/// is also what a JavaScript string literal needs): `"` and `\`, `\b \t \n
/// \f \r`, and the other control characters as `\u00XX`. Runs of plain
/// characters are copied whole.
pub(crate) fn write_json_string(out: &mut String, s: &str) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.reserve(s.len() + 2);
    out.push('"');
    let mut start = 0;
    for (i, &b) in s.as_bytes().iter().enumerate() {
        let escape = match b {
            b'"' => "\\\"",
            b'\\' => "\\\\",
            b'\n' => "\\n",
            b'\r' => "\\r",
            b'\t' => "\\t",
            0x08 => "\\b",
            0x0c => "\\f",
            0..=0x1f => "",
            _ => continue,
        };
        // An ASCII byte is never inside a multi-byte character: `i` is a boundary.
        out.push_str(&s[start..i]);
        if escape.is_empty() {
            out.push_str("\\u00");
            out.push(HEX[usize::from(b >> 4)] as char);
            out.push(HEX[usize::from(b & 15)] as char);
        } else {
            out.push_str(escape);
        }
        start = i + 1;
    }
    out.push_str(&s[start..]);
    out.push('"');
}

#[cfg(test)]
fn base64(bytes: &[u8]) -> String {
    let mut out = String::new();
    push_base64(&mut out, bytes);
    out
}

pub(crate) fn push_base64(out: &mut String, bytes: &[u8]) {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    out.reserve(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
}

/// Field names of `docs` in order of first appearance, `_id` first.
pub fn columns<'a>(docs: impl IntoIterator<Item = &'a Document>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut cols = Vec::new();
    for d in docs {
        for k in d.keys() {
            if seen.insert(k.as_str()) {
                cols.push(k.clone());
            }
        }
    }
    if let Some(pos) = cols.iter().position(|c| c == "_id") {
        let id = cols.remove(pos);
        cols.insert(0, id);
    }
    cols
}

/// What an ObjectId encodes, as Studio 3T shows it on hover: creation time
/// (display zone), then the "machine", "pid" and "counter" fields of the
/// classic layout. Since MongoDB 3.4 the middle five bytes are random, so
/// machine and pid no longer identify anything.
pub fn object_id_details(oid: &bson::oid::ObjectId) -> String {
    let b = oid.bytes();
    let secs = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
    let machine = u32::from_be_bytes([0, b[4], b[5], b[6]]);
    let pid = u16::from_be_bytes([b[7], b[8]]);
    let counter = u32::from_be_bytes([0, b[9], b[10], b[11]]);
    let created = iso_date(bson::DateTime::from_millis(i64::from(secs) * 1000));
    format!("created: {created}\nmachine: {machine}\npid: {pid}\ncounter: {counter}")
}

/// Bytes as Studio 3T shows them: `0 B`, `4.0 KiB`, `1.5 MiB`.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", UNITS[u]) }
}

/// The current local time, e.g. `2026-10-18 13:12:03 +07:00`.
pub fn local_now() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S %:z").to_string()
}

/// Field paths in `docs`, nested ones dotted (`profile.country`), in order of
/// first appearance with `_id` first. Arrays of documents contribute their
/// elements' fields. Stops at `max_depth` levels.
pub fn field_paths<'a>(docs: impl IntoIterator<Item = &'a Document>, max_depth: usize) -> Vec<String> {
    // `path` is one buffer for the whole walk: a path already seen costs no allocation.
    fn walk(d: &Document, path: &mut String, depth: usize, max: usize, seen: &mut std::collections::HashSet<String>, out: &mut Vec<String>) {
        for (k, v) in d {
            let prefix = path.len();
            if prefix > 0 {
                path.push('.');
            }
            path.push_str(k);
            if !seen.contains(path.as_str()) {
                seen.insert(path.clone());
                out.push(path.clone());
            }
            if depth + 1 < max {
                match v {
                    Bson::Document(sub) => walk(sub, path, depth + 1, max, seen, out),
                    Bson::Array(items) => {
                        for item in items.iter().take(20) {
                            if let Bson::Document(sub) = item {
                                walk(sub, path, depth + 1, max, seen, out);
                            }
                        }
                    }
                    _ => {}
                }
            }
            path.truncate(prefix);
        }
    }
    let (mut seen, mut out, mut path) = (std::collections::HashSet::new(), Vec::new(), String::new());
    for d in docs {
        walk(d, &mut path, 0, max_depth.max(1), &mut seen, &mut out);
    }
    if let Some(pos) = out.iter().position(|c| c == "_id") {
        let id = out.remove(pos);
        out.insert(0, id);
    }
    out
}

/// A document holding every BSON type, with awkward values of each (tests).
#[cfg(test)]
pub(crate) fn every_type_doc() -> Document {
    let re = |p: &str, o: &str| Bson::RegularExpression(bson::Regex { pattern: p.to_string().try_into().unwrap(), options: o.to_string().try_into().unwrap() });
    let bin = |st: u8, b: &[u8]| Bson::Binary(bson::Binary { subtype: st.into(), bytes: b.to_vec() });
    let ptr = Bson::try_from(serde_json::json!({ "$dbPointer": { "$ref": "a.b", "$id": { "$oid": "64f6ef45435c6900a1b2c3d4" } } })).unwrap();
    let dates: Vec<Bson> = [0_i64, -1, 1, 999, 1_700_000_000_123, -62_198_755_200_000, 253_402_300_799_999, 253_402_300_800_000, i64::MAX, i64::MIN, -86_400_000 * 365 * 3000].into_iter().map(|ms| Bson::DateTime(bson::DateTime::from_millis(ms))).collect();
    let doubles: Vec<Bson> = [0.0, -0.0, 5.0, -5.0, 0.1, 1e21, 1e-7, 123456789012345680.0, 1e16, 9007199254740993.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY, f64::MIN_POSITIVE, 5e-324, f64::MAX].into_iter().map(Bson::Double).collect();
    let long = |n: usize, tail: &str| format!("{}{tail}", "é".repeat(n));
    let strings: Vec<Bson> = ["", "plain", "q\"uo\\te", "line\nbreak\r\t\u{8}\u{c}", "\u{0}\u{1}\u{1f}\u{7f}", "é中文😀", "\u{2028}\u{2029}", "</script>", "x".repeat(300).as_str(), "a\nb\nc", "abc\r", "x\r\ny", "\r\n", "\n", &long(199, "\r\nz"), &long(200, "\r\nz"), &long(200, "\nz"), &long(200, ""), &long(201, ""), &long(199, "\r")]
        .into_iter()
        .map(|s| Bson::String(s.into()))
        .collect();
    bson::doc! {
        "_id": bson::oid::ObjectId::parse_str("64f6ef45435c6900a1b2c3d4").unwrap(),
        "weird \"key\"\n\u{1}": 1,
        "i32": i32::MIN, "i64": i64::MIN, "i64b": 5_i64, "dec": "-12.50E+3".parse::<bson::Decimal128>().unwrap(),
        "doubles": doubles, "dates": dates, "strings": strings,
        "bool": true, "null": Bson::Null, "undef": Bson::Undefined, "min": Bson::MinKey, "max": Bson::MaxKey,
        "uuid": bin(4, &[0xab; 16]), "uuid_short": bin(4, &[1, 2, 3]), "bin": bin(0, b"hello world"), "bin_empty": bin(128, b""),
        "regexes": [re("^SALE_", "i"), re("a/b", ""), re("(?i)x", ""), re("a b", "ix")],
        "ts": Bson::Timestamp(bson::Timestamp { time: 1_700_000_000, increment: 7 }),
        "code": Bson::JavaScriptCode("function () {\n return 1;\n}".into()),
        "scope": Bson::JavaScriptCodeWithScope(bson::JavaScriptCodeWithScope { code: "x".into(), scope: bson::doc! { "a": 1, "b": [1, 2] } }),
        "sym": Bson::Symbol("s\"y".into()), "ptr": ptr,
        "empty_doc": {}, "empty_arr": [], "nested": { "a": [[], {}, [1, [2, { "b": "c" }]]] },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::{doc, oid::ObjectId};

    #[test]
    fn cell_text_summarises_containers() {
        assert_eq!(cell_text(&Bson::Array(vec![])), "[ 0 elements ]");
        assert_eq!(cell_text(&Bson::Array(vec![Bson::Null])), "[ 1 element ]");
        assert_eq!(cell_text(&Bson::Document(doc! {"a": 1, "b": 2})), "{ 2 fields }");
        assert_eq!(cell_text(&Bson::Double(5.0)), "5.0");
        assert_eq!(cell_text(&Bson::Double(5.25)), "5.25");
        assert_eq!(cell_text(&Bson::String("a\nb".into())), "a…");
    }

    #[test]
    fn shell_text_uses_constructors() {
        let oid = ObjectId::parse_str("64f6ef45435c6900a1b2c3d4").unwrap();
        let d = doc! {
            "_id": oid,
            "n": 1,
            "big": 5_i64,
            "f": 2.0,
            "at": bson::DateTime::from_millis(1_700_000_000_123),
            "s": "say \"hi\"",
            "tags": [],
        };
        let text = doc_to_shell(&d, Style::Editable);
        assert!(text.contains(r#""_id" : ObjectId("64f6ef45435c6900a1b2c3d4")"#), "{text}");
        assert!(text.contains(r#""big" : NumberLong("5")"#));
        assert!(text.contains(r#""f" : Double(2)"#));
        assert!(text.contains(r#""at" : ISODate("2023-11-14T22:13:20.123Z")"#), "{text}");
        assert!(text.contains(r#""s" : "say \"hi\"""#));
        assert!(text.contains(r#""tags" : [ ]"#));
        assert!(doc_to_shell(&d, Style::Display).contains(r#""f" : 2.0"#));
    }

    #[test]
    fn far_dates_and_escaped_slashes_read_back() {
        let end = bson::DateTime::from_millis(253_402_297_200_000); // 9999-12-31T23:00:00Z
        let date_literal = |dt, tz| {
            let mut out = String::new();
            write_date_literal(&mut out, dt, tz);
            out
        };
        assert_eq!(date_literal(end, DisplayTz::Utc), r#"ISODate("9999-12-31T23:00:00.000Z")"#);
        assert_eq!(date_literal(end, DisplayTz::Offset(420)), "new Date(253402297200000)");
        assert_eq!(date_literal(bson::DateTime::from_millis(-62_198_755_200_000), DisplayTz::Utc), "new Date(-62198755200000)");
        let re = |p: &str| to_shell(&Bson::RegularExpression(bson::Regex { pattern: p.to_string().try_into().unwrap(), options: "i".to_string().try_into().unwrap() }), Style::Display);
        // A slash comes back escaped from a `/…/` literal (`a\/b`, not
        // `a/b`), so those patterns are written as two strings.
        assert_eq!(re("^SALE_"), "/^SALE_/i");
        assert_eq!(re(r"a/b"), r#"BSONRegExp("a/b", "i")"#);
        assert_eq!(re(r"a\/b"), r#"BSONRegExp("a\\/b", "i")"#);
    }

    /// Regexes JS reads as written stay `/…/`; PCRE-only syntax and
    /// patterns JS refuses are written as two strings instead.
    #[test]
    fn regexes_js_refuses_are_written_as_strings() {
        let re = |p: &str, o: &str| to_shell(&Bson::RegularExpression(bson::Regex { pattern: p.to_string().try_into().unwrap(), options: o.to_string().try_into().unwrap() }), Style::Editable);
        for (p, o) in [(r"^[a-z0-9._%+-]+@[a-z0-9.-]+\.[a-z]{2,}$", "i"), (r"(?:ab)+c??|d{1,3}?", ""), (r"^\d{3}-\d{4}$", ""), (r"(?=x)(?!y)(?<=z)(?<!w)\bq\B", "s"), (r"[]a]{,2}\p{L}", ""), ("^abc$", "u"), ("é+", "mu")] {
            assert_eq!(re(p, o), format!("/{p}/{o}"));
        }
        for (p, o) in [("(?i)abc", ""), ("(?P<n>x)", ""), ("(?<n>x)", ""), ("a{2,1}", ""), (r"a\-b", "u"), ("a{2}", "u"), ("a++", ""), ("(a", ""), ("a)", ""), ("+a", ""), ("a|*b", ""), (r"\b*", ""), ("[z-a]", ""), (r"[\w-z]", "")] {
            assert!(re(p, o).starts_with("BSONRegExp("), "{p} {o}: {}", re(p, o));
        }
    }

    #[test]
    fn dates_in_each_zone() {
        let dt = bson::DateTime::from_millis(1_767_323_045_000); // 2026-01-02T03:04:05Z
        assert_eq!(iso_date_in(dt, DisplayTz::Utc), "2026-01-02T03:04:05.000Z");
        assert_eq!(iso_date_in(dt, DisplayTz::Offset(420)), "2026-01-02T10:04:05.000+07:00");
        assert_eq!(iso_date_in(dt, DisplayTz::Offset(-150)), "2026-01-02T00:34:05.000-02:30");
        let local = iso_date_in(dt, DisplayTz::Local);
        assert!(local.starts_with("2026-01-0") && local.len() == "2026-01-02T03:04:05.000+00:00".len(), "{local}");
        assert_eq!(serde_json::to_string(&DisplayTz::Offset(420)).unwrap(), r#"{"kind":"offset","minutes":420}"#);
    }

    #[test]
    fn field_paths_include_nested_fields() {
        let docs = [
            doc! { "name": "a", "_id": 1, "profile": { "country": "TH", "geo": { "lat": 1 } }, "items": [{ "sku": "x" }] },
            doc! { "extra": true },
        ];
        assert_eq!(field_paths(&docs, 3), ["_id", "name", "profile", "profile.country", "profile.geo", "profile.geo.lat", "items", "items.sku", "extra"]);
        assert_eq!(field_paths(&docs, 1), ["_id", "name", "profile", "items", "extra"]);
    }

    #[test]
    fn object_id_details_decode_each_part() {
        let oid = bson::oid::ObjectId::parse_str("6acfed20c33444084cfc9836").unwrap();
        let text = object_id_details(&oid);
        let expected_created = iso_date(bson::DateTime::from_millis(0x6acf_ed20_i64 * 1000));
        assert_eq!(text, format!("created: {expected_created}\nmachine: {}\npid: {}\ncounter: {}", 0xc3_3444, 0x084c, 0xfc_9836));
    }

    #[test]
    fn human_bytes_like_studio_3t() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(4096), "4.0 KiB");
        assert_eq!(human_bytes(12_288), "12.0 KiB");
        assert_eq!(human_bytes(1_572_864), "1.5 MiB");
    }

    #[test]
    fn columns_put_id_first() {
        let docs = [doc! {"name": "a", "_id": 1}, doc! {"extra": true, "name": "b"}];
        assert_eq!(columns(&docs), ["_id", "name", "extra"]);
    }

    #[test]
    fn base64_matches_known_values() {
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"M"), "TQ==");
    }

    /// The hand-written writers give exactly what serde_json and chrono's
    /// formatter give (checked once over 145 MB of output when they
    /// replaced them).
    #[test]
    fn fast_writers_match_their_references() {
        let mut strings: Vec<String> = (0u8..0x80).map(|b| format!("a{}b", b as char)).collect();
        strings.extend(["", "é中文😀\u{2028}\u{2029}", "\"\\\u{0}\u{1f}\u{7f}", "plain text"].map(String::from));
        if let Some(Bson::Array(more)) = every_type_doc().get("strings") {
            strings.extend(more.iter().filter_map(|s| s.as_str().map(String::from)));
        }
        for s in &strings {
            let mut out = String::from("x");
            write_json_string(&mut out, s);
            assert_eq!(out[1..], serde_json::to_string(s).unwrap(), "{s:?}");
        }
        for tz in [DisplayTz::Utc, DisplayTz::Offset(420), DisplayTz::Offset(-150), DisplayTz::Offset(0), DisplayTz::Offset(100_000), DisplayTz::Local] {
            let mut fast = 0;
            for ms in (-20_000_i64..20_000).map(|k| k * 17_654_321_987).chain([0, -1, 999, 253_402_300_799_999]) {
                let utc = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms).unwrap();
                let mut out = String::new();
                if write_iso_fast(&mut out, utc, tz) {
                    assert_eq!(out, iso_date_slow(utc, tz), "{ms} in {tz:?}");
                    fast += 1;
                } else {
                    assert!(out.is_empty());
                }
            }
            assert!(fast > 10_000, "{tz:?}: most dates take the fast way ({fast})");
        }
        let bytes: Vec<u8> = (0..16).map(|i| i * 17).collect();
        let mut out = String::new();
        push_uuid(&mut out, &bytes);
        let h: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(out, format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32]));
        // Indentation deeper than one run of spaces.
        let mut deep = Bson::Int32(1);
        for _ in 0..20 {
            deep = Bson::Array(vec![deep]);
        }
        let text = value_to_shell_pretty(&deep, Style::Display);
        assert!(text.contains(&format!("\n{}1\n", " ".repeat(80))), "{text}");
    }

    /// The demo server's bet_logs, users and providers.
    fn sample_docs() -> Vec<Document> {
        let s = crate::mock::MockStore::demo();
        let mut docs = Vec::new();
        for coll in ["bet_logs", "users", "providers"] {
            docs.extend(s.find(&crate::api::Namespace::new("demo-games", coll), &doc! {}, None, None, 0, None).unwrap());
        }
        docs
    }

    /// Times the JSON view's text (doc_to_shell), table cells (cell_text)
    /// and field_paths over ~26k documents, and cell_text of a 1 MB line.
    /// `cargo test -p rongo-core --release --lib -- --ignored --nocapture report_format_time`
    #[test]
    #[ignore = "timing; run explicitly in release mode"]
    fn report_format_time() {
        let docs = sample_docs();
        let time = |what: &str, f: &mut dyn FnMut() -> usize| {
            let mut best = std::time::Duration::MAX;
            let mut n = 0;
            for _ in 0..5 {
                let t = std::time::Instant::now();
                n = f();
                best = best.min(t.elapsed());
            }
            println!("{what}: {best:?} (best of 5; {n})");
        };
        time("doc_to_shell Display", &mut || docs.iter().map(|d| doc_to_shell(d, Style::Display).len()).sum());
        time("doc_to_shell Editable", &mut || docs.iter().map(|d| doc_to_shell(d, Style::Editable).len()).sum());
        time("cell_text every field", &mut || docs.iter().flat_map(|d| d.values()).map(|v| cell_text(v).len()).sum());
        let line = Bson::String("x".repeat(1 << 20));
        time("cell_text 1 MB line x100", &mut || (0..100).map(|_| cell_text(&line).len()).sum());
        time("field_paths depth 3", &mut || field_paths(&docs, 3).len());
    }
}
