//! Export a query to a file, and import documents from a file or text, as
//! JSON (array or one document per line, relaxed or canonical Extended JSON)
//! or CSV.

use std::path::Path;

use bson::{Bson, Document};
use tokio::io::AsyncWriteExt;

use crate::api::{ExportFormat, ExportSpec, ImportFormat, Namespace};
use crate::conn::{Conn, FindOpts};
use crate::error::{CoreError, CoreResult};
use crate::format;
use crate::mock::query::{get_path, set_path};

const BATCH: usize = 1000;

fn io(e: std::io::Error) -> CoreError {
    CoreError::Other(e.to_string())
}

/// Stream the matching documents into `spec.path`. Returns how many were
/// written.
pub async fn export(conn: &Conn, spec: &ExportSpec) -> CoreResult<u64> {
    let opts = FindOpts {
        sort: spec.sort.clone(),
        projection: spec.projection.clone(),
        batch_size: Some(BATCH as u32),
        ..Default::default()
    };
    let mut cursor = conn.open_find(&spec.ns, spec.filter.clone(), opts).await?;
    let file = tokio::fs::File::create(&spec.path).await.map_err(io)?;
    let mut w = tokio::io::BufWriter::new(file);
    let mut count = 0u64;
    match spec.format {
        ExportFormat::JsonArray | ExportFormat::JsonLines => {
            let array = spec.format == ExportFormat::JsonArray;
            if array {
                w.write_all(b"[\n").await.map_err(io)?;
            }
            loop {
                let batch = cursor.next_batch(BATCH).await?;
                if batch.is_empty() {
                    break;
                }
                for d in batch {
                    if array && count > 0 {
                        w.write_all(b",\n").await.map_err(io)?;
                    }
                    w.write_all(json_line(d).as_bytes()).await.map_err(io)?;
                    if !array {
                        w.write_all(b"\n").await.map_err(io)?;
                    }
                    count += 1;
                }
            }
            if array {
                w.write_all(b"\n]\n").await.map_err(io)?;
            }
        }
        ExportFormat::Csv => {
            let mut batch = cursor.next_batch(BATCH).await?;
            let fields = spec.fields.clone().unwrap_or_else(|| format::columns(&batch));
            // Field names come from the data too.
            let header: Vec<String> = fields.iter().map(|f| defuse_formula(f)).collect();
            w.write_all(csv_line(header.iter().map(String::as_str)).as_bytes()).await.map_err(io)?;
            while !batch.is_empty() {
                for d in &batch {
                    let row: Vec<String> = fields.iter().map(|f| csv_value(get_path(d, f))).collect();
                    w.write_all(csv_line(row.iter().map(String::as_str)).as_bytes()).await.map_err(io)?;
                    count += 1;
                }
                batch = cursor.next_batch(BATCH).await?;
            }
        }
    }
    w.flush().await.map_err(io)?;
    Ok(count)
}

/// Relaxed Extended JSON, except Int64 stays `{"$numberLong": "…"}` (as in
/// canonical form): relaxed writes it as a plain number, which imports back
/// as Int32 when it fits.
fn json_line(d: Document) -> String {
    serde_json::to_string(&keep_longs(Bson::Document(d)).into_relaxed_extjson()).unwrap_or_default()
}

fn keep_longs(v: Bson) -> Bson {
    match v {
        Bson::Int64(n) => Bson::Document(bson::doc! { "$numberLong": n.to_string() }),
        Bson::Document(d) => Bson::Document(d.into_iter().map(|(k, v)| (k, keep_longs(v))).collect()),
        Bson::Array(a) => Bson::Array(a.into_iter().map(keep_longs).collect()),
        other => other,
    }
}

/// A value as CSV cell text. Nested values become relaxed Extended JSON.
/// Numbers and booleans are written as they are; every other cell goes
/// through `defuse_formula`, whatever its BSON type.
pub fn csv_value(v: Option<&Bson>) -> String {
    let text = match v {
        None | Some(Bson::Null | Bson::Undefined) => return String::new(),
        Some(Bson::Int32(i)) => return i.to_string(),
        Some(Bson::Int64(i)) => return i.to_string(),
        Some(Bson::Double(f)) => return f.to_string(),
        Some(Bson::Decimal128(d)) => return d.to_string(),
        Some(Bson::Boolean(b)) => return b.to_string(),
        Some(Bson::String(s) | Bson::Symbol(s)) => s.clone(),
        Some(Bson::ObjectId(o)) => o.to_hex(),
        Some(Bson::DateTime(d)) => format::iso_date_utc(*d),
        Some(v @ (Bson::Document(_) | Bson::Array(_))) => serde_json::to_string(&v.clone().into_relaxed_extjson()).unwrap_or_default(),
        Some(other) => format::cell_text(other),
    };
    defuse_formula(&text)
}

/// Characters that make a spreadsheet treat a cell as a formula.
const FORMULA_START: [char; 6] = ['=', '+', '-', '@', '\t', '\r'];

/// Prefix `'` to text a spreadsheet would run as a formula (OWASP's advice
/// against CSV injection). Only strings: numbers like -5 are never touched.
/// Import strips the prefix again.
fn defuse_formula(s: &str) -> String {
    // Text that already looks defused ('=x) gets a quote too, so import's
    // stripping gives back exactly what was exported.
    if s.starts_with(FORMULA_START) || s.strip_prefix('\'').is_some_and(|r| r.starts_with(FORMULA_START)) { format!("'{s}") } else { s.to_string() }
}

/// The inverse of `defuse_formula`.
fn restore_formula(s: &str) -> &str {
    let defused = |r: &str| r.starts_with(FORMULA_START) || r.strip_prefix('\'').is_some_and(|r| r.starts_with(FORMULA_START));
    match s.strip_prefix('\'') {
        Some(rest) if defused(rest) => rest,
        _ => s,
    }
}

fn csv_line<'a>(cells: impl Iterator<Item = &'a str>) -> String {
    let mut line = cells.map(csv_escape).collect::<Vec<_>>().join(",");
    line.push('\n');
    line
}

fn csv_escape(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) || s.starts_with(' ') || s.ends_with(' ') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Read `path` and insert its documents in batches. Returns how many were
/// inserted; on failure the error says how many went in first.
pub async fn import(conn: &Conn, ns: &Namespace, path: &Path, format: ImportFormat) -> CoreResult<u64> {
    let text = tokio::fs::read_to_string(path).await.map_err(io)?;
    let format = match format {
        ImportFormat::Auto => detect_format(path, &text),
        f => f,
    };
    let docs = parse_documents(&text, format)?;
    if docs.is_empty() {
        return Err(CoreError::InvalidInput("the file contains no documents".into()));
    }
    let mut inserted = 0u64;
    for chunk in docs.chunks(BATCH) {
        conn.insert_many(ns, chunk.to_vec()).await.map_err(|e| match inserted {
            0 => e,
            n => CoreError::Other(format!("{e} (after inserting {n} documents)")),
        })?;
        inserted += chunk.len() as u64;
    }
    Ok(inserted)
}

pub fn detect_format(path: &Path, text: &str) -> ImportFormat {
    match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("csv") => ImportFormat::Csv,
        Some("jsonl" | "ndjson") => ImportFormat::JsonLines,
        _ if text.trim_start().starts_with('[') => ImportFormat::JsonArray,
        _ => ImportFormat::JsonLines,
    }
}

/// Parse documents from text in the given format (`Auto` decides between a
/// JSON array and JSON lines).
pub fn parse_documents(text: &str, format: ImportFormat) -> CoreResult<Vec<Document>> {
    match format {
        ImportFormat::Auto if text.trim_start().starts_with('[') => parse_json_array(text),
        ImportFormat::Auto | ImportFormat::JsonLines => parse_json_lines(text),
        ImportFormat::JsonArray => parse_json_array(text),
        ImportFormat::Csv => parse_csv(text),
    }
}

fn to_document(v: serde_json::Value, what: &str) -> CoreResult<Document> {
    match Bson::try_from(v).map_err(|e| CoreError::InvalidInput(format!("{what}: {e}")))? {
        Bson::Document(d) => Ok(d),
        _ => Err(CoreError::InvalidInput(format!("{what}: not an object"))),
    }
}

fn parse_json_array(text: &str) -> CoreResult<Vec<Document>> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|e| CoreError::InvalidInput(format!("JSON: {e}")))?;
    let serde_json::Value::Array(items) = value else {
        return Err(CoreError::InvalidInput("expected a JSON array".into()));
    };
    items.into_iter().enumerate().map(|(i, v)| to_document(v, &format!("item {}", i + 1))).collect()
}

/// One document per line; also accepts documents spread over several lines
/// one after another, as mongoexport --pretty writes them.
fn parse_json_lines(text: &str) -> CoreResult<Vec<Document>> {
    let mut out = Vec::new();
    let stream = serde_json::Deserializer::from_str(text).into_iter::<serde_json::Value>();
    for (i, v) in stream.enumerate() {
        let v = v.map_err(|e| CoreError::InvalidInput(format!("JSON: {e}")))?;
        out.push(to_document(v, &format!("document {}", i + 1))?);
    }
    Ok(out)
}

fn parse_csv(text: &str) -> CoreResult<Vec<Document>> {
    let mut rows = csv_rows(text).into_iter();
    let Some(header) = rows.next() else { return Ok(Vec::new()) };
    let header: Vec<String> = header.into_iter().map(|h| restore_formula(h.trim()).to_string()).collect();
    if header.iter().any(String::is_empty) {
        return Err(CoreError::InvalidInput("every CSV column needs a name in the first row".into()));
    }
    let mut out = Vec::new();
    for (n, row) in rows.enumerate() {
        if row.len() == 1 && row[0].is_empty() {
            continue;
        }
        if row.len() > header.len() {
            return Err(CoreError::InvalidInput(format!("CSV row {} has more cells than the header", n + 2)));
        }
        let mut d = Document::new();
        for (key, cell) in header.iter().zip(row) {
            if let Some(v) = csv_typed(&cell) {
                set_path(&mut d, key, v)?;
            }
        }
        out.push(d);
    }
    Ok(out)
}

/// Numbers and booleans become typed values; empty cells are left out.
/// Numbers with leading zeros (codes, phone numbers) stay strings.
fn csv_typed(s: &str) -> Option<Bson> {
    if s.is_empty() {
        return None;
    }
    // Undo `defuse_formula` from our own exports.
    let restored = restore_formula(s);
    if restored.len() != s.len() {
        return Some(Bson::String(restored.to_string()));
    }
    let leading_zero = s.len() > 1 && s.trim_start_matches('-').starts_with('0') && !s.trim_start_matches('-').starts_with("0.");
    if !leading_zero {
        if let Ok(i) = s.parse::<i64>() {
            return Some(i32::try_from(i).map(Bson::Int32).unwrap_or(Bson::Int64(i)));
        }
        if s.contains(['.', 'e', 'E'])
            && let Ok(f) = s.parse::<f64>()
            && f.is_finite()
        {
            return Some(Bson::Double(f));
        }
    }
    Some(match s {
        "true" => Bson::Boolean(true),
        "false" => Bson::Boolean(false),
        _ => Bson::String(s.to_string()),
    })
}

/// RFC 4180 rows: quoted cells may contain commas, quotes ("") and newlines.
fn csv_rows(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut cell = String::new();
    let mut quoted = false;
    let mut chars = text.trim_start_matches('\u{feff}').chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    cell.push('"');
                    chars.next();
                }
                '"' => quoted = false,
                _ => cell.push(c),
            }
            continue;
        }
        match c {
            '"' if cell.is_empty() => quoted = true,
            ',' => row.push(std::mem::take(&mut cell)),
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' | '\r' => {
                row.push(std::mem::take(&mut cell));
                rows.push(std::mem::take(&mut row));
            }
            _ => cell.push(c),
        }
    }
    if !cell.is_empty() || !row.is_empty() {
        row.push(cell);
        rows.push(row);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockStore;
    use bson::doc;

    #[test]
    fn csv_rows_handle_quotes_and_newlines() {
        let rows = csv_rows("a,b\r\n\"x, y\",\"say \"\"hi\"\"\nthere\"\n1,\n");
        assert_eq!(rows, vec![vec!["a", "b"], vec!["x, y", "say \"hi\"\nthere"], vec!["1", ""]]);
        assert_eq!(csv_escape("plain"), "plain");
        assert_eq!(csv_escape("a,\"b\""), "\"a,\"\"b\"\"\"");
    }

    #[test]
    fn csv_types_and_nested_headers() {
        let docs = parse_csv("name,age,score,vip,code,profile.country\nann,31,9.5,true,007,TH\nbob,,1e3,false,12,\n").unwrap();
        assert_eq!(docs[0], doc! { "name": "ann", "age": 31, "score": 9.5, "vip": true, "code": "007", "profile": { "country": "TH" } });
        assert_eq!(docs[1], doc! { "name": "bob", "score": 1000.0, "vip": false, "code": 12 });
    }

    #[test]
    fn csv_export_defuses_formulas_and_import_restores_them() {
        let evil = Bson::String("=HYPERLINK(\"http://x\")".into());
        assert_eq!(csv_value(Some(&evil)), "'=HYPERLINK(\"http://x\")");
        assert_eq!(csv_value(Some(&Bson::String("-1+2".into()))), "'-1+2");
        assert_eq!(csv_value(Some(&Bson::Int32(-5))), "-5");
        assert_eq!(csv_value(Some(&Bson::String("plain".into()))), "plain");
        assert_eq!(csv_typed("'=HYPERLINK(1)"), Some(Bson::String("=HYPERLINK(1)".into())));
        assert_eq!(csv_typed("'quoted"), Some(Bson::String("'quoted".into())));
        // Other text-like types are covered too.
        assert_eq!(csv_value(Some(&Bson::Symbol("=cmd".into()))), "'=cmd");
        assert_eq!(csv_value(Some(&Bson::JavaScriptCode("@x".into()))), "'@x");
        // Header names round-trip through the same escaping.
        let docs = parse_csv("'=total,n\n1,2\n").unwrap();
        assert_eq!(docs[0], bson::doc! { "=total": 1, "n": 2 });
    }

    #[test]
    fn json_formats_and_detection() {
        let lines = "{\"a\": 1}\n\n{\"_id\": {\"$oid\": \"64f6ef45435c6900a1b2c3d4\"}, \"at\": {\"$date\": \"2026-01-01T00:00:00Z\"}}\n";
        let docs = parse_documents(lines, ImportFormat::Auto).unwrap();
        assert_eq!(docs.len(), 2);
        assert!(matches!(docs[1].get("_id"), Some(Bson::ObjectId(_))));
        assert!(matches!(docs[1].get("at"), Some(Bson::DateTime(_))));
        assert_eq!(parse_documents("[{\"a\":1},{\"a\":2}]", ImportFormat::Auto).unwrap().len(), 2);
        assert!(parse_documents("[1]", ImportFormat::JsonArray).is_err());
        assert_eq!(detect_format(Path::new("x.CSV"), ""), ImportFormat::Csv);
        assert_eq!(detect_format(Path::new("x.json"), "  ["), ImportFormat::JsonArray);
        assert_eq!(detect_format(Path::new("x.json"), "{}"), ImportFormat::JsonLines);
    }

    /// NumberLong survives a JSON export and import (relaxed JSON writes it as
    /// a plain number, which comes back Int32), and text that already starts
    /// with '= survives CSV's formula defusing.
    #[tokio::test]
    async fn longs_and_quoted_formulas_round_trip() {
        let conn = Conn::mock(MockStore::demo(), false);
        let ns = Namespace::new("types", "c");
        conn.insert_one(&ns, doc! { "_id": 1, "n": 5_i64, "big": 9_007_199_254_740_993_i64, "i": 7, "s": "'=x", "f": "=SUM(A1)" }).await.unwrap();
        let dir = std::env::temp_dir().join(format!("rongo-types-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (format, ext) in [(ExportFormat::JsonLines, "jsonl"), (ExportFormat::JsonArray, "json"), (ExportFormat::Csv, "csv")] {
            let path = dir.join(format!("c.{ext}"));
            let spec = ExportSpec { ns: ns.clone(), filter: doc! {}, sort: None, projection: None, format, path: path.clone(), fields: None };
            export(&conn, &spec).await.unwrap();
            let target = Namespace::new("types", ext);
            import(&conn, &target, &path, ImportFormat::Auto).await.unwrap();
            let raw = conn.find_page(&crate::api::FindSpec::all(target, 10)).await.unwrap().0;
            let back = Document::try_from(raw[0].clone()).unwrap();
            assert_eq!(back.get_str("s").unwrap(), "'=x", "{ext}");
            assert_eq!(back.get_str("f").unwrap(), "=SUM(A1)", "{ext}");
            if format != ExportFormat::Csv {
                assert_eq!(back.get("n"), Some(&Bson::Int64(5)), "{ext}");
                assert_eq!(back.get("big"), Some(&Bson::Int64(9_007_199_254_740_993)), "{ext}");
                assert_eq!(back.get("i"), Some(&Bson::Int32(7)), "{ext}");
            }
        }
        std::fs::remove_dir_all(dir).ok();
    }

    /// Exported JSON keeps each document's field order (and so compound
    /// `_id`s), and import reads it back in that order.
    #[tokio::test]
    async fn json_export_keeps_field_order() {
        let conn = Conn::mock(MockStore::demo(), false);
        let ns = Namespace::new("order", "c");
        let original = doc! { "_id": { "z": 1, "a": 2 }, "name": "x", "age": 3 };
        conn.insert_one(&ns, original.clone()).await.unwrap();
        let dir = std::env::temp_dir().join(format!("rongo-order-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("c.jsonl");
        let spec = ExportSpec { ns: ns.clone(), filter: doc! {}, sort: None, projection: None, format: ExportFormat::JsonLines, path: path.clone(), fields: None };
        export(&conn, &spec).await.unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(r#"{"_id":{"z":1,"a":2},"name":"x","age":3}"#), "{text}");
        let target = Namespace::new("order", "copy");
        import(&conn, &target, &path, ImportFormat::Auto).await.unwrap();
        let raw = conn.find_page(&crate::api::FindSpec::all(target, 10)).await.unwrap().0;
        let back = Document::try_from(raw[0].clone()).unwrap();
        assert_eq!(back.keys().collect::<Vec<_>>(), ["_id", "name", "age"]);
        assert_eq!(back.get_document("_id").unwrap().keys().collect::<Vec<_>>(), ["z", "a"]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn export_then_import_round_trips() {
        let conn = Conn::mock(MockStore::demo(), false);
        let ns = Namespace::new("demo-games", "providers");
        let dir = std::env::temp_dir().join(format!("rongo-transfer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (format, ext) in [(ExportFormat::JsonArray, "json"), (ExportFormat::JsonLines, "jsonl"), (ExportFormat::Csv, "csv")] {
            let path = dir.join(format!("providers.{ext}"));
            let spec = ExportSpec { ns: ns.clone(), filter: doc! {}, sort: Some(doc! { "code": 1 }), projection: None, format, path: path.clone(), fields: None };
            assert_eq!(export(&conn, &spec).await.unwrap(), 8);
            let target = Namespace::new("copy", ext);
            assert_eq!(import(&conn, &target, &path, ImportFormat::Auto).await.unwrap(), 8);
            let mut pages = Vec::new();
            for ns in [ns.clone(), target] {
                let raw = conn.find_page(&crate::api::FindSpec { sort: Some(doc! { "code": 1 }), ..crate::api::FindSpec::all(ns, 100) }).await.unwrap().0;
                pages.push(raw.into_iter().map(|d| Document::try_from(d).unwrap()).collect::<Vec<Document>>());
            }
            let (copied, original) = (pages.pop().unwrap(), pages.pop().unwrap());
            if format == ExportFormat::Csv {
                // CSV keeps scalars; nested values come back as JSON text.
                assert_eq!(copied[0].get_str("code").unwrap(), original[0].get_str("code").unwrap());
                assert!(copied[0].get_str("config").unwrap().contains("timeoutMs"));
            } else {
                assert_eq!(copied, original, "{ext}");
            }
        }
        std::fs::remove_dir_all(dir).ok();
    }
}
