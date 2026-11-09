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
            let line = s.lines().next().unwrap_or("");
            let mut out: String = line.chars().take(CELL_MAX).collect();
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
        Bson::ObjectId(oid) => oid.to_hex(),
        Bson::DateTime(dt) => iso_date(*dt),
        Bson::Double(f) => double_text(*f),
        Bson::Int32(i) => i.to_string(),
        Bson::Int64(i) => i.to_string(),
        Bson::Decimal128(d) => d.to_string(),
        Bson::Boolean(b) => b.to_string(),
        Bson::Null => "null".into(),
        Bson::Undefined => "undefined".into(),
        Bson::Binary(b) if b.subtype == BinarySubtype::Uuid && b.bytes.len() == 16 => uuid_text(&b.bytes),
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

fn double_text(f: f64) -> String {
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e16 {
        format!("{f:.1}")
    } else {
        f.to_string()
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
fn date_literal(dt: bson::DateTime, tz: DisplayTz) -> String {
    let ms = dt.timestamp_millis();
    let text = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms).map(|_| iso_date_in(dt, tz));
    match text.filter(|t| t.len() > 4 && t.as_bytes()[..4].iter().all(u8::is_ascii_digit) && t.as_bytes()[4] == b'-') {
        Some(text) => format!("ISODate(\"{text}\")"),
        None => format!("new Date({ms})"),
    }
}

/// A date in UTC regardless of the display setting (for exports).
pub fn iso_date_utc(dt: bson::DateTime) -> String {
    iso_date_in(dt, DisplayTz::Utc)
}

pub fn iso_date_in(dt: bson::DateTime, tz: DisplayTz) -> String {
    let ms = dt.timestamp_millis();
    let Some(utc) = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms) else {
        return ms.to_string();
    };
    match tz {
        DisplayTz::Utc => utc.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
        DisplayTz::Local => utc.with_timezone(&chrono::Local).format("%Y-%m-%dT%H:%M:%S%.3f%:z").to_string(),
        DisplayTz::Offset(m) => match chrono::FixedOffset::east_opt(m * 60) {
            Some(off) => utc.with_timezone(&off).format("%Y-%m-%dT%H:%M:%S%.3f%:z").to_string(),
            None => utc.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
        },
    }
}

fn uuid_text(bytes: &[u8]) -> String {
    let h: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
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
    match indent {
        Some(w) => {
            out.push('\n');
            out.extend(std::iter::repeat_n(' ', w * level));
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
        Bson::Symbol(s) => write_string(out, s),
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
            let _ = write!(out, "ObjectId(\"{}\")", oid.to_hex());
        }
        Bson::DateTime(dt) => out.push_str(&date_literal(*dt, display_tz())),
        Bson::Binary(b) if b.subtype == BinarySubtype::Uuid && b.bytes.len() == 16 => {
            let _ = write!(out, "UUID(\"{}\")", uuid_text(&b.bytes));
        }
        Bson::Binary(b) => {
            let _ = write!(out, "BinData({}, \"{}\")", u8::from(b.subtype), base64(&b.bytes));
        }
        Bson::RegularExpression(r) => {
            // Escape the slashes that end the literal, not ones already
            // escaped (`a\/b` must not become `a\\/b`).
            out.push('/');
            let mut escaped = false;
            for c in r.pattern.as_str().chars() {
                if c == '/' && !escaped {
                    out.push('\\');
                }
                out.push(c);
                escaped = c == '\\' && !escaped;
            }
            let _ = write!(out, "/{}", r.options);
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
        Bson::DbPointer(_) => out.push_str("DBPointer()"),
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
            Style::Display => out.push_str(&double_text(f)),
            Style::Editable => {
                let _ = write!(out, "Double({f})");
            }
        }
    } else {
        let _ = write!(out, "{f}");
    }
}

fn write_string(out: &mut String, s: &str) {
    // serde_json escapes exactly like a JavaScript string literal needs.
    out.push_str(&serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into()));
}

fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
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
    fn walk(d: &Document, prefix: &str, depth: usize, max: usize, seen: &mut std::collections::HashSet<String>, out: &mut Vec<String>) {
        for (k, v) in d {
            let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
            if seen.insert(path.clone()) {
                out.push(path.clone());
            }
            if depth + 1 >= max {
                continue;
            }
            match v {
                Bson::Document(sub) => walk(sub, &path, depth + 1, max, seen, out),
                Bson::Array(items) => {
                    for item in items.iter().take(20) {
                        if let Bson::Document(sub) = item {
                            walk(sub, &path, depth + 1, max, seen, out);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let (mut seen, mut out) = (std::collections::HashSet::new(), Vec::new());
    for d in docs {
        walk(d, "", 0, max_depth.max(1), &mut seen, &mut out);
    }
    if let Some(pos) = out.iter().position(|c| c == "_id") {
        let id = out.remove(pos);
        out.insert(0, id);
    }
    out
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
        assert_eq!(date_literal(end, DisplayTz::Utc), r#"ISODate("9999-12-31T23:00:00.000Z")"#);
        assert_eq!(date_literal(end, DisplayTz::Offset(420)), "new Date(253402297200000)");
        assert_eq!(date_literal(bson::DateTime::from_millis(-62_198_755_200_000), DisplayTz::Utc), "new Date(-62198755200000)");
        let re = |p: &str| to_shell(&Bson::RegularExpression(bson::Regex { pattern: p.to_string().try_into().unwrap(), options: "i".to_string().try_into().unwrap() }), Style::Display);
        assert_eq!(re(r"a/b"), r"/a\/b/i");
        assert_eq!(re(r"a\/b"), r"/a\/b/i", "already escaped: left alone");
        assert_eq!(re(r"a\\/b"), r"/a\\\/b/i", "an escaped backslash, then a slash");
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
}
