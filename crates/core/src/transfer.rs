//! Export a query to a file, and import documents from a file or text, as
//! JSON (array or one document per line, relaxed or canonical Extended JSON)
//! or CSV.

use std::path::Path;

use bson::{Bson, Document};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

use crate::api::{ExportFormat, ExportSpec, ImportFormat, Namespace, WriteOutcome};
use crate::conn::{Conn, FindOpts};
use crate::engine::Progress;
use crate::error::{CoreError, CoreResult};
use crate::format;
use crate::fsx;
use crate::paths::{get_path, set_path};

/// Documents per round trip when streaming (export, import, copy).
pub(crate) const BATCH: usize = 1000;

fn io(e: std::io::Error) -> CoreError {
    CoreError::Other(e.to_string())
}

/// Stream the matching documents into `spec.path`. Returns how many were
/// written. They go into a new `.part` file beside it (a name no file has
/// yet), which takes the file's place only once complete and on disk,
/// with the permissions of the file it replaces; the folder is synced after
/// the rename. A symlink at `spec.path` is followed: the file it points to
/// is replaced, as writing through the link did. A failed or cancelled
/// export deletes its part and leaves the file that was there as it was.
/// The token is checked between batches; a cancelled export answers
/// `Cancelled`.
pub(crate) async fn export(conn: &Conn, spec: &ExportSpec, progress: &Progress, cancel: &CancellationToken) -> CoreResult<u64> {
    let target = real_target(&spec.path).await;
    if target.file_name().is_none() {
        return Err(CoreError::InvalidInput(format!("{} is not a file name", spec.path.display())));
    }
    let mut part = None;
    let done = match write_export(conn, spec, &target, &mut part, progress, cancel).await {
        Ok(count) => match &part {
            Some(p) => put_in_place(p, &target).await.map(|()| count),
            None => Err(CoreError::Other("internal error: the export wrote no file".into())),
        },
        Err(e) => Err(e),
    };
    if done.is_err()
        && let Some(p) = part
    {
        // Waits out antivirus reading the part (Windows), so none is left behind.
        let _ = tokio::task::spawn_blocking(move || fsx::remove_file(&p)).await;
    }
    done
}

/// Where an export to `path` lands: a symlink there (or a chain of them,
/// dangling or not) is followed, so the link stays and the file it points
/// to is the one replaced.
async fn real_target(path: &Path) -> std::path::PathBuf {
    let mut p = path.to_path_buf();
    // As many hops as the system follows before it gives up (ELOOP).
    for _ in 0..32 {
        if !tokio::fs::symlink_metadata(&p).await.is_ok_and(|m| m.file_type().is_symlink()) {
            break;
        }
        let Ok(to) = tokio::fs::read_link(&p).await else { break };
        p = match p.parent() {
            Some(dir) if to.is_relative() => dir.join(to),
            _ => to,
        };
    }
    p
}

/// A new, empty file beside `target` for the export to write into,
/// `<name>.<pid>-<n>.part`: created only if no file has that name, so a
/// file already there (`<name>.part` of the user's own, say) is never
/// touched. Its path goes into `part`, for the caller to clean up.
async fn open_part(target: &Path, part: &mut Option<std::path::PathBuf>) -> CoreResult<tokio::io::BufWriter<tokio::fs::File>> {
    let name = target.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let mut n = 0;
    loop {
        let path = target.with_file_name(format!("{name}.{}-{n}.part", std::process::id()));
        match tokio::fs::OpenOptions::new().write(true).create_new(true).open(&path).await {
            Ok(file) => {
                *part = Some(path);
                return Ok(tokio::io::BufWriter::with_capacity(1 << 20, file));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && n < 1000 => n += 1,
            Err(e) => return Err(CoreError::Other(format!("{}: {e}", path.display()))),
        }
    }
}

/// Renames the finished `part` over `target` (`fsx::rename`: a part that
/// antivirus is still reading is waited for, briefly), giving it the
/// permissions of the file it replaces, then syncs the folder so the rename
/// is on disk too (best effort: the export itself is already complete).
async fn put_in_place(part: &Path, target: &Path) -> CoreResult<()> {
    if let Ok(old) = tokio::fs::metadata(target).await
        && old.is_file()
    {
        tokio::fs::set_permissions(part, old.permissions()).await.map_err(io)?;
    }
    let (from, to) = (part.to_path_buf(), target.to_path_buf());
    let moved = tokio::task::spawn_blocking(move || {
        fsx::rename(&from, &to)?;
        fsx::sync_dir(to.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new(".")));
        Ok(())
    });
    moved.await.unwrap_or_else(|e| Err(std::io::Error::other(e))).map_err(|e| CoreError::Other(format!("{}: {e}", target.display())))
}

/// `export`'s writing, into a new part file beside `target` (its path put
/// in `part`), synced to disk at the end. Each batch is written into one
/// reused text buffer, then to the file through a 1 MiB buffer (an 8 KiB
/// one meant a hop to the blocking pool every 8 KiB).
async fn write_export(conn: &Conn, spec: &ExportSpec, target: &Path, part: &mut Option<std::path::PathBuf>, progress: &Progress, cancel: &CancellationToken) -> CoreResult<u64> {
    let opts = FindOpts {
        sort: spec.sort.clone(),
        projection: spec.projection.clone(),
        batch_size: Some(BATCH as u32),
        ..Default::default()
    };
    let mut count = 0u64;
    let mut text = String::new();
    let mut w = match spec.format {
        ExportFormat::JsonArray | ExportFormat::JsonLines => {
            // Raw documents, as the server sent them: nothing is parsed.
            let mut cursor = conn.open_find_raw(&spec.ns, spec.filter.clone(), opts).await?;
            let mut w = open_part(target, part).await?;
            let array = spec.format == ExportFormat::JsonArray;
            if array {
                text.push_str("[\n");
            }
            loop {
                if cancel.is_cancelled() {
                    return Err(CoreError::Cancelled);
                }
                let batch = cursor.next_batch(BATCH).await?;
                if batch.is_empty() {
                    break;
                }
                for d in &batch {
                    if array && count > 0 {
                        text.push_str(",\n");
                    }
                    write_ejson_doc(&mut text, d, true)?;
                    if !array {
                        text.push('\n');
                    }
                    count += 1;
                }
                w.write_all(text.as_bytes()).await.map_err(io)?;
                text.clear();
                progress.report(count, None, "");
            }
            if array {
                text.push_str("\n]\n");
            }
            w
        }
        ExportFormat::Csv => {
            let mut cursor = conn.open_find(&spec.ns, spec.filter.clone(), opts).await?;
            let mut w = open_part(target, part).await?;
            let mut batch = cursor.next_batch(BATCH).await?;
            let fields = spec.fields.clone().unwrap_or_else(|| format::columns(&batch));
            // Field names come from the data too.
            for (i, f) in fields.iter().enumerate() {
                if i > 0 {
                    text.push(',');
                }
                push_csv_text(&mut text, f, ',');
            }
            text.push('\n');
            let mut cell = String::new();
            while !batch.is_empty() {
                for d in &batch {
                    for (i, f) in fields.iter().enumerate() {
                        if i > 0 {
                            text.push(',');
                        }
                        push_csv_value(&mut text, &mut cell, get_path(d, f), ',');
                    }
                    text.push('\n');
                    count += 1;
                }
                w.write_all(text.as_bytes()).await.map_err(io)?;
                text.clear();
                progress.report(count, None, "");
                if cancel.is_cancelled() {
                    return Err(CoreError::Cancelled);
                }
                batch = cursor.next_batch(BATCH).await?;
            }
            w
        }
    };
    w.write_all(text.as_bytes()).await.map_err(io)?;
    w.flush().await.map_err(io)?;
    w.into_inner().sync_all().await.map_err(io)?;
    Ok(count)
}

/// One document as relaxed Extended JSON, except that (with `longs`) Int64
/// stays `{"$numberLong":"…"}` as in canonical form: relaxed writes it as a
/// plain number, which imports back as Int32 when it fits. Written in one
/// pass straight from the raw BSON, byte for byte what serde_json makes of
/// bson's `into_relaxed_extjson` (with Int64s replaced first, but not inside
/// a code's scope, which that replacing never reached).
fn write_ejson_doc(out: &mut String, d: &bson::RawDocument, longs: bool) -> CoreResult<()> {
    out.push('{');
    for (i, item) in d.iter().enumerate() {
        let (key, value) = item?;
        if i > 0 {
            out.push(',');
        }
        format::write_json_string(out, key.as_str());
        out.push(':');
        write_ejson(out, value, longs)?;
    }
    out.push('}');
    Ok(())
}

fn write_ejson(out: &mut String, v: bson::RawBsonRef<'_>, longs: bool) -> CoreResult<()> {
    use std::fmt::Write as _;
    use bson::RawBsonRef as R;
    let quoted = |out: &mut String, key: &str, s: &str| {
        let _ = write!(out, "{{\"{key}\":");
        format::write_json_string(out, s);
        out.push('}');
    };
    match v {
        R::Double(f) if f.is_nan() => out.push_str(if f.is_sign_negative() { r#"{"$numberDouble":"-NaN"}"# } else { r#"{"$numberDouble":"NaN"}"# }),
        R::Double(f) if f.is_infinite() => out.push_str(if f < 0.0 { r#"{"$numberDouble":"-Infinity"}"# } else { r#"{"$numberDouble":"Infinity"}"# }),
        R::Double(f) => {
            if let Some(n) = serde_json::Number::from_f64(f) {
                let _ = write!(out, "{n}");
            }
        }
        R::String(s) => format::write_json_string(out, s),
        R::Array(a) => {
            out.push('[');
            for (i, item) in a.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_ejson(out, item?, longs)?;
            }
            out.push(']');
        }
        R::Document(d) => write_ejson_doc(out, d, longs)?,
        R::Boolean(b) => out.push_str(if b { "true" } else { "false" }),
        R::Null => out.push_str("null"),
        R::RegularExpression(r) => {
            let mut options: Vec<char> = r.options.as_str().chars().collect();
            options.sort_unstable();
            out.push_str(r#"{"$regularExpression":{"pattern":"#);
            format::write_json_string(out, r.pattern.as_str());
            out.push_str(r#","options":"#);
            format::write_json_string(out, &options.into_iter().collect::<String>());
            out.push_str("}}");
        }
        R::JavaScriptCode(c) => quoted(out, "$code", c),
        R::JavaScriptCodeWithScope(c) => {
            out.push_str(r#"{"$code":"#);
            format::write_json_string(out, c.code);
            out.push_str(r#","$scope":"#);
            write_ejson_doc(out, c.scope, false)?;
            out.push('}');
        }
        R::Int32(i) => {
            let _ = write!(out, "{i}");
        }
        R::Int64(i) if longs => {
            let _ = write!(out, r#"{{"$numberLong":"{i}"}}"#);
        }
        R::Int64(i) => {
            let _ = write!(out, "{i}");
        }
        R::Timestamp(t) => {
            let _ = write!(out, r#"{{"$timestamp":{{"t":{},"i":{}}}}}"#, t.time, t.increment);
        }
        R::Binary(b) => {
            out.push_str(r#"{"$binary":{"base64":""#);
            format::push_base64(out, b.bytes);
            let _ = write!(out, r#"","subType":"{:02x}"}}}}"#, u8::from(b.subtype));
        }
        R::ObjectId(o) => {
            out.push_str(r#"{"$oid":""#);
            format::push_hex(out, &o.bytes());
            out.push_str("\"}");
        }
        R::DateTime(d) => write_ejson_date(out, d),
        R::Symbol(s) => quoted(out, "$symbol", s),
        R::Decimal128(d) => {
            let _ = write!(out, r#"{{"$numberDecimal":"{d}"}}"#);
        }
        R::Undefined => out.push_str(r#"{"$undefined":true}"#),
        R::MinKey => out.push_str(r#"{"$minKey":1}"#),
        R::MaxKey => out.push_str(r#"{"$maxKey":1}"#),
        // Its parts are private to the bson crate; rare enough to go the long way.
        other @ R::DbPointer(_) => out.push_str(&serde_json::to_string(&Bson::try_from(other)?.into_relaxed_extjson()).unwrap_or_default()),
    }
    Ok(())
}

/// A date as relaxed Extended JSON: `{"$date":"…"}` in RFC 3339 (UTC, the
/// milliseconds without trailing zeros, as bson writes it) from 1970 to
/// 9999, else `{"$date":{"$numberLong":"…"}}`.
fn write_ejson_date(out: &mut String, d: bson::DateTime) {
    use chrono::{Datelike, Timelike};
    use std::fmt::Write as _;
    let ms = d.timestamp_millis();
    let utc = (0..253_402_300_800_000).contains(&ms).then(|| chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)).flatten();
    let Some(t) = utc else {
        let _ = write!(out, r#"{{"$date":{{"$numberLong":"{ms}"}}}}"#);
        return;
    };
    let _ = write!(out, r#"{{"$date":"{:04}-{:02}-{:02}T{:02}:{:02}:{:02}"#, t.year(), t.month(), t.day(), t.hour(), t.minute(), t.second());
    let mut millis = ms % 1000;
    if millis != 0 {
        let mut digits = 3;
        while millis % 10 == 0 {
            millis /= 10;
            digits -= 1;
        }
        let _ = write!(out, ".{millis:0digits$}");
    }
    out.push_str("Z\"}");
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

/// `csv_value` written straight into a line as one cell (quoted as
/// `delimited_line` quotes), with no allocation for numbers, strings,
/// ObjectIds and dates; `scratch` holds the text of the other kinds.
fn push_csv_value(out: &mut String, scratch: &mut String, v: Option<&Bson>, sep: char) {
    use std::fmt::Write as _;
    let _ = match v {
        None | Some(Bson::Null | Bson::Undefined) => Ok(()),
        Some(Bson::Int32(i)) => write!(out, "{i}"),
        Some(Bson::Int64(i)) => write!(out, "{i}"),
        Some(Bson::Double(f)) => write!(out, "{f}"),
        Some(Bson::Decimal128(d)) => write!(out, "{d}"),
        Some(Bson::Boolean(b)) => write!(out, "{b}"),
        Some(Bson::String(s) | Bson::Symbol(s)) => {
            push_csv_text(out, s, sep);
            Ok(())
        }
        // Hex digits: nothing to defuse or quote.
        Some(Bson::ObjectId(o)) => {
            format::push_hex(out, &o.bytes());
            Ok(())
        }
        Some(other) => {
            scratch.clear();
            match other {
                Bson::DateTime(d) => format::push_iso_date(scratch, *d, format::DisplayTz::Utc),
                Bson::Document(_) | Bson::Array(_) => scratch.push_str(&serde_json::to_string(&other.clone().into_relaxed_extjson()).unwrap_or_default()),
                other => scratch.push_str(&format::cell_text(other)),
            }
            push_csv_text(out, scratch, sep);
            Ok(())
        }
    };
}

/// Text (a string value, a field name) written as one cell: defused as
/// `defuse_formula` does, then quoted as `delimited_line` quotes.
fn push_csv_text(out: &mut String, s: &str, sep: char) {
    let defused = formula_like(s);
    // A leading `'` neither needs quoting nor is a space.
    let quote = s.contains([sep, '"', '\n', '\r']) || (!defused && s.starts_with(' ')) || s.ends_with(' ');
    if quote {
        out.push('"');
    }
    if defused {
        out.push('\'');
    }
    // (A quote in it means the cell is quoted: each is doubled.)
    for (i, part) in s.split('"').enumerate() {
        if i > 0 {
            out.push_str("\"\"");
        }
        out.push_str(part);
    }
    if quote {
        out.push('"');
    }
}

/// Characters that make a spreadsheet treat a cell as a formula.
const FORMULA_START: [char; 6] = ['=', '+', '-', '@', '\t', '\r'];

/// Whether `defuse_formula` prefixes `'` to `s`.
fn formula_like(s: &str) -> bool {
    // Text that already looks defused ('=x) gets a quote too, so import's
    // stripping gives back exactly what was exported.
    s.starts_with(FORMULA_START) || s.strip_prefix('\'').is_some_and(|r| r.starts_with(FORMULA_START))
}

/// Prefix `'` to text a spreadsheet would run as a formula (OWASP's advice
/// against CSV injection). Only strings: numbers like -5 are never touched.
/// Import strips the prefix again.
pub fn defuse_formula(s: &str) -> String {
    if formula_like(s) { format!("'{s}") } else { s.to_string() }
}

/// The inverse of `defuse_formula`.
fn restore_formula(s: &str) -> &str {
    let defused = |r: &str| r.starts_with(FORMULA_START) || r.strip_prefix('\'').is_some_and(|r| r.starts_with(FORMULA_START));
    match s.strip_prefix('\'') {
        Some(rest) if defused(rest) => rest,
        _ => s,
    }
}

/// One line of cells joined by `sep` (no line break), each quoted (RFC 4180,
/// `"` doubled) only when it holds `sep`, a quote or a line break, or has
/// a space at either end.
pub fn delimited_line<'a>(cells: impl Iterator<Item = &'a str>, sep: char) -> String {
    let escape = |s: &str| {
        if s.contains([sep, '"', '\n', '\r']) || s.starts_with(' ') || s.ends_with(' ') { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_string() }
    };
    cells.map(escape).collect::<Vec<_>>().join(&sep.to_string())
}

/// Read `path` and insert its documents in batches, every one it can
/// (unordered): a document whose `_id` is already in the collection is
/// skipped and counted in `duplicates`; documents refused for another
/// reason are counted in `warning` with the first reason, and the import
/// goes on; so does one after a write-concern error, which sets
/// `unconfirmed`. An error that stops it says how many went in first. The token
/// is checked between batches; a cancelled import answers what it
/// inserted, with `cancelled` set.
///
/// The file is read and parsed on a blocking thread, a batch at a time,
/// while this task inserts the batches before it: the runtime's workers
/// never wait on a parse, and the first documents go in at once. So a file
/// found malformed part way leaves what came before inserted (the error
/// says how much).
pub(crate) async fn import(conn: &Conn, ns: &Namespace, path: &Path, format: ImportFormat, progress: &Progress, cancel: &CancellationToken) -> CoreResult<WriteOutcome> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<CoreResult<Vec<Document>>>(2);
    let file = path.to_path_buf();
    let reader = tokio::task::spawn_blocking(move || {
        // A send fails once this import has stopped: the reader stops too.
        if let Err(e) = read_documents(&file, format, &mut |batch| tx.blocking_send(Ok(batch)).is_ok()) {
            let _ = tx.blocking_send(Err(e));
        }
    });
    let mut tally = Tally::default();
    loop {
        // Nothing is on its way to the server while this waits for the
        // reader (reading the file, parsing a batch): a cancel stops it here.
        let next = tokio::select! {
            next = rx.recv() => next,
            () = cancel.cancelled() => None,
        };
        if cancel.is_cancelled() {
            tally.out.cancelled = true;
            return Ok(tally.finish());
        }
        let Some(batch) = next else { break };
        let batch = batch.map_err(|e| tally.stopped(e))?;
        tally.insert(conn, ns, batch).await?;
        progress.report(tally.done(), None, "");
    }
    // Its sender is gone, so the reader has ended: a panic there is no end of file.
    reader.await.map_err(|_| CoreError::Other("internal error: the file's reader stopped".into()))?;
    if tally.done() == 0 {
        return Err(CoreError::InvalidInput("the file contains no documents".into()));
    }
    Ok(tally.finish())
}

/// What an import has done so far, batch by batch.
#[derive(Default)]
struct Tally {
    out: WriteOutcome,
    /// Documents refused for a reason other than an existing `_id`.
    refused: u64,
    /// Why the first of them was refused.
    first_refusal: Option<CoreError>,
    /// The first error of a batch that refused nothing for it (a write
    /// concern's): kept apart, so it is never given as a refusal's reason.
    other_error: Option<CoreError>,
}

impl Tally {
    /// Inserts one batch, unordered. An error that fails the whole batch
    /// ends the import; once something was done, it says how much.
    async fn insert(&mut self, conn: &Conn, ns: &Namespace, batch: Vec<Document>) -> CoreResult<()> {
        let n = batch.len() as u64;
        let r = conn.insert_many_unordered(ns, &batch).await.map_err(|e| self.stopped(e))?;
        self.add(n, r);
        Ok(())
    }

    /// Counts what an unordered insert of `n` documents did.
    fn add(&mut self, n: u64, r: crate::conn::Inserted) {
        let duplicates = r.duplicates.len() as u64;
        let refused = n.saturating_sub(r.inserted + duplicates);
        self.out.inserted += r.inserted;
        self.out.duplicates += duplicates;
        self.refused += refused;
        // A write-concern error: what went in is not confirmed.
        self.out.unconfirmed |= r.unconfirmed;
        if r.unconfirmed {
            self.out.unconfirmed_docs += r.inserted;
        }
        // A batch's error is a refusal's reason only when it refused something.
        let first = if refused > 0 { &mut self.first_refusal } else { &mut self.other_error };
        if first.is_none() {
            *first = r.error;
        }
    }

    /// The refusals so far, as `finish` words them, and the other error.
    fn problems(&self) -> Vec<String> {
        let refusals = self.first_refusal.as_ref().map(|e| match self.refused {
            1 => format!("1 document refused: {e}"),
            n => format!("{n} documents refused; the first: {e}"),
        });
        refusals.into_iter().chain(self.other_error.as_ref().map(ToString::to_string)).collect()
    }

    /// Documents dealt with: inserted, skipped or refused.
    fn done(&self) -> u64 {
        self.out.inserted + self.out.duplicates + self.refused
    }

    /// `e`, saying what was done before it, if anything was: inserted,
    /// skipped, refused (with the first reason), and an earlier error.
    fn stopped(&self, e: CoreError) -> CoreError {
        if self.done() == 0 && self.other_error.is_none() {
            return e;
        }
        let mut before = format!("after inserting {} documents", self.out.inserted);
        if self.out.duplicates > 0 {
            before.push_str(&format!(", skipping {} with an _id already in the collection", self.out.duplicates));
        }
        if let Some(reason) = &self.first_refusal {
            before.push_str(&match self.refused {
                1 => format!(", 1 refused ({reason})"),
                n => format!(", {n} refused (the first: {reason})"),
            });
        }
        if let Some(other) = &self.other_error {
            before.push_str(&format!("; earlier: {other}"));
        }
        CoreError::Other(format!("{e} ({before})"))
    }

    fn finish(mut self) -> WriteOutcome {
        let problems = self.problems();
        self.out.warning = (!problems.is_empty()).then(|| problems.join("; "));
        self.out
    }
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
    let mut out = Vec::new();
    each_document(text, format, &mut |d| {
        out.push(d);
        true
    })?;
    Ok(out)
}

/// Reads `path` and hands its documents to `emit` in batches of `BATCH`,
/// parsing as it goes. Stops when `emit` says to (returns false).
fn read_documents(path: &Path, format: ImportFormat, emit: &mut dyn FnMut(Vec<Document>) -> bool) -> CoreResult<()> {
    let text = std::fs::read_to_string(path).map_err(io)?;
    let format = match format {
        ImportFormat::Auto => detect_format(path, &text),
        f => f,
    };
    let (mut batch, mut going) = (Vec::with_capacity(BATCH), true);
    each_document(&text, format, &mut |d| {
        batch.push(d);
        if batch.len() == BATCH {
            going = emit(std::mem::replace(&mut batch, Vec::with_capacity(BATCH)));
        }
        going
    })?;
    if going && !batch.is_empty() {
        emit(batch);
    }
    Ok(())
}

/// Hands each document of `text` to `sink` as it is parsed, until `sink`
/// says to stop (returns false).
fn each_document(text: &str, format: ImportFormat, sink: &mut dyn FnMut(Document) -> bool) -> CoreResult<()> {
    match format {
        ImportFormat::Auto if text.trim_start().starts_with('[') => json_array(text, sink),
        ImportFormat::Auto | ImportFormat::JsonLines => json_lines(text, sink),
        ImportFormat::JsonArray => json_array(text, sink),
        ImportFormat::Csv => csv_documents(text, sink),
    }
}

/// `what` names the value in an error (built only for one).
fn to_document(v: serde_json::Value, what: impl Fn() -> String) -> CoreResult<Document> {
    match Bson::try_from(v).map_err(|e| CoreError::InvalidInput(format!("{}: {e}", what())))? {
        Bson::Document(d) => Ok(d),
        _ => Err(CoreError::InvalidInput(format!("{}: not an object", what()))),
    }
}

/// The items of one JSON array, each handed on as soon as it is parsed.
fn json_array(text: &str, sink: &mut dyn FnMut(Document) -> bool) -> CoreResult<()> {
    use serde::Deserializer as _;
    struct Items<'s> {
        sink: &'s mut dyn FnMut(Document) -> bool,
        failed: Option<CoreError>,
        stopped: bool,
    }
    impl<'de> serde::de::Visitor<'de> for &mut Items<'_> {
        type Value = ();
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a JSON array")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
            let mut n = 0;
            while let Some(v) = seq.next_element::<serde_json::Value>()? {
                n += 1;
                let going = match to_document(v, || format!("item {n}")) {
                    Ok(d) => (self.sink)(d),
                    Err(e) => {
                        self.failed = Some(e);
                        false
                    }
                };
                if !going {
                    self.stopped = self.failed.is_none();
                    return Err(serde::de::Error::custom("stopped"));
                }
            }
            Ok(())
        }
    }
    let mut items = Items { sink, failed: None, stopped: false };
    let mut de = serde_json::Deserializer::from_str(text);
    let result = (&mut de).deserialize_seq(&mut items).and_then(|()| de.end());
    match (items.failed, items.stopped) {
        (Some(e), _) => Err(e),
        (None, true) => Ok(()),
        (None, false) => result.map_err(|e| CoreError::InvalidInput(format!("JSON: {e}"))),
    }
}

/// One document per line; also accepts documents spread over several lines
/// one after another, as mongoexport --pretty writes them.
fn json_lines(text: &str, sink: &mut dyn FnMut(Document) -> bool) -> CoreResult<()> {
    let stream = serde_json::Deserializer::from_str(text).into_iter::<serde_json::Value>();
    for (i, v) in stream.enumerate() {
        let v = v.map_err(|e| CoreError::InvalidInput(format!("JSON: {e}")))?;
        if !sink(to_document(v, || format!("document {}", i + 1))?) {
            break;
        }
    }
    Ok(())
}

fn csv_documents(text: &str, sink: &mut dyn FnMut(Document) -> bool) -> CoreResult<()> {
    let mut rows = CsvRows::new(text);
    let Some(header) = rows.next() else { return Ok(()) };
    let header: Vec<String> = header.into_iter().map(|h| restore_formula(h.trim()).to_string()).collect();
    if header.iter().any(String::is_empty) {
        return Err(CoreError::InvalidInput("every CSV column needs a name in the first row".into()));
    }
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
        if !sink(d) {
            break;
        }
    }
    Ok(())
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

/// RFC 4180 rows, one at a time: quoted cells may contain commas, quotes
/// ("") and newlines.
struct CsvRows<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
}

impl<'a> CsvRows<'a> {
    fn new(text: &'a str) -> Self {
        CsvRows { chars: text.trim_start_matches('\u{feff}').chars().peekable() }
    }
}

impl Iterator for CsvRows<'_> {
    type Item = Vec<String>;

    fn next(&mut self) -> Option<Vec<String>> {
        let mut row = Vec::new();
        let mut cell = String::new();
        let mut quoted = false;
        while let Some(c) = self.chars.next() {
            if quoted {
                match c {
                    '"' if self.chars.peek() == Some(&'"') => {
                        cell.push('"');
                        self.chars.next();
                    }
                    '"' => quoted = false,
                    _ => cell.push(c),
                }
                continue;
            }
            match c {
                '"' if cell.is_empty() => quoted = true,
                ',' => row.push(std::mem::take(&mut cell)),
                '\r' if self.chars.peek() == Some(&'\n') => {}
                '\n' | '\r' => {
                    row.push(cell);
                    return Some(row);
                }
                _ => cell.push(c),
            }
        }
        if !cell.is_empty() || !row.is_empty() {
            row.push(cell);
            return Some(row);
        }
        None
    }
}

#[cfg(test)]
fn csv_rows(text: &str) -> Vec<Vec<String>> {
    CsvRows::new(text).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockStore;
    use bson::doc;

    async fn run_export(conn: &Conn, spec: &ExportSpec) -> CoreResult<u64> {
        export(conn, spec, &Progress::channel().0, &CancellationToken::new()).await
    }

    async fn run_import(conn: &Conn, ns: &Namespace, path: &Path) -> CoreResult<WriteOutcome> {
        import(conn, ns, path, ImportFormat::Auto, &Progress::channel().0, &CancellationToken::new()).await
    }

    #[test]
    fn csv_rows_handle_quotes_and_newlines() {
        let rows = csv_rows("a,b\r\n\"x, y\",\"say \"\"hi\"\"\nthere\"\n1,\n");
        assert_eq!(rows, vec![vec!["a", "b"], vec!["x, y", "say \"hi\"\nthere"], vec!["1", ""]]);
        assert_eq!(delimited_line(["plain"].into_iter(), ','), "plain");
        assert_eq!(delimited_line(["a,\"b\""].into_iter(), ','), "\"a,\"\"b\"\"\"");
    }

    #[test]
    fn delimited_lines_quote_only_what_needs_it() {
        assert_eq!(delimited_line(["a", "b c", "x,y"].into_iter(), ','), "a,b c,\"x,y\"");
        assert_eq!(delimited_line(["a,b", "t\tab", "q\"t", "l\nf"].into_iter(), '\t'), "a,b\t\"t\tab\"\t\"q\"\"t\"\t\"l\nf\"");
        assert_eq!(delimited_line([" lead", "x"].into_iter(), ','), "\" lead\",x");
    }

    #[test]
    fn csv_types_and_nested_headers() {
        let docs = parse_documents("name,age,score,vip,code,profile.country\nann,31,9.5,true,007,TH\nbob,,1e3,false,12,\n", ImportFormat::Csv).unwrap();
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
        let docs = parse_documents("'=total,n\n1,2\n", ImportFormat::Csv).unwrap();
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
        let conn = Conn::mock(MockStore::test_data(), false);
        let ns = Namespace::new("types", "c");
        conn.insert_one(&ns, doc! { "_id": 1, "n": 5_i64, "big": 9_007_199_254_740_993_i64, "i": 7, "s": "'=x", "f": "=SUM(A1)" }).await.unwrap();
        let dir = std::env::temp_dir().join(format!("rongo-types-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (format, ext) in [(ExportFormat::JsonLines, "jsonl"), (ExportFormat::JsonArray, "json"), (ExportFormat::Csv, "csv")] {
            let path = dir.join(format!("c.{ext}"));
            let spec = ExportSpec { ns: ns.clone(), filter: doc! {}, sort: None, projection: None, format, path: path.clone(), fields: None };
            run_export(&conn, &spec).await.unwrap();
            let target = Namespace::new("types", ext);
            run_import(&conn, &target, &path).await.unwrap();
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

    /// A fresh temp folder for one test.
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rongo-{tag}-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Imports `path` into `ns`, with every progress report's count.
    async fn import_counting(conn: &Conn, ns: &Namespace, path: &Path) -> (CoreResult<WriteOutcome>, Vec<u64>) {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        let progress = Progress::with_hook(move |done| log.lock().unwrap().push(done));
        let r = import(conn, ns, path, ImportFormat::Auto, &progress, &CancellationToken::new()).await;
        let counts = seen.lock().unwrap().clone();
        (r, counts)
    }

    /// Import inserts batch by batch as it reads, in every format, and says
    /// so after each batch.
    #[tokio::test]
    async fn an_import_inserts_as_it_reads() {
        let conn = Conn::mock(MockStore::empty(), false);
        let dir = temp_dir("stream");
        let files = [
            ("rows.jsonl", (0..2500).map(|i| format!("{{\"_id\": {i}}}\n")).collect::<String>()),
            ("rows.json", format!("[{}]", (0..2500).map(|i| format!("{{\"_id\": {i}}}")).collect::<Vec<_>>().join(",\n"))),
            ("rows.csv", format!("_id,n\n{}", (0..2500).map(|i| format!("{i},{i}\n")).collect::<String>())),
        ];
        for (name, text) in files {
            let path = dir.join(name);
            std::fs::write(&path, text).unwrap();
            let ns = Namespace::new("t", name.replace('.', "_"));
            let (r, counts) = import_counting(&conn, &ns, &path).await;
            assert_eq!(r.map(|w| w.inserted), Ok(2500), "{name}");
            assert_eq!(counts, [1000, 2000, 2500], "{name}");
            assert_eq!(conn.count(&ns, doc! {}).await.unwrap(), 2500);
        }
        std::fs::remove_dir_all(dir).ok();
    }

    /// ⊗ stops an import at once, also while its file is still being read
    /// and the first batch parsed (here a pipe nobody has written to yet).
    #[cfg(unix)]
    #[tokio::test]
    async fn an_import_stops_while_its_file_is_read() {
        use std::os::unix::ffi::OsStrExt;
        let conn = Conn::mock(MockStore::empty(), false);
        let dir = temp_dir("fifo");
        let fifo = dir.join("slow.jsonl");
        let name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            stop.cancel();
        });
        let ns = Namespace::new("t", "c");
        let r = tokio::time::timeout(std::time::Duration::from_secs(5), import(&conn, &ns, &fifo, ImportFormat::Auto, &Progress::channel().0, &cancel)).await;
        // Let the reader go: open the pipe's other end and close it.
        drop(std::fs::OpenOptions::new().write(true).open(&fifo).unwrap());
        let w = r.expect("stopped while the file was read").unwrap();
        assert!(w.cancelled && w.inserted == 0, "{w:?}");
        std::fs::remove_dir_all(dir).ok();
    }

    /// An error that refused no documents (a write concern's) is never given
    /// as the reason for refusals that came after it, and an import that
    /// stops says what was refused before it, with the first reason.
    #[test]
    fn import_reasons_stay_with_what_they_explain() {
        use crate::conn::Inserted;
        let concern = CoreError::WriteConcern { code: 64, msg: "waiting for replication timed out".into() };
        let validation = CoreError::Server { code: 121, msg: "Document failed validation".into() };
        let mut t = Tally::default();
        t.add(1000, Inserted { inserted: 1000, duplicates: vec![], error: Some(concern), unconfirmed: true });
        t.add(1000, Inserted { inserted: 997, duplicates: vec![4], error: Some(validation), unconfirmed: false });
        let stop = t.stopped(CoreError::InvalidInput("JSON: expected value at line 2001 column 1".into())).to_string();
        assert_eq!(stop, "JSON: expected value at line 2001 column 1 (after inserting 1997 documents, skipping 1 with an _id already in the collection, 2 refused (the first: server error 121: Document failed validation); earlier: write concern error 64: waiting for replication timed out)");
        let out = t.finish();
        assert_eq!(out.warning.as_deref(), Some("2 documents refused; the first: server error 121: Document failed validation; write concern error 64: waiting for replication timed out"));
        assert!(out.unconfirmed);
        assert_eq!((out.inserted, out.unconfirmed_docs), (1997, 1000), "only the batch with the write-concern error is unconfirmed");
    }

    /// A write-concern error on a batch: its documents went in on the
    /// primary but were not confirmed. The import goes on, and its answer is
    /// unconfirmed, with the reason.
    #[tokio::test]
    async fn a_write_concern_error_makes_an_import_unconfirmed() {
        let conn = Conn::mock(MockStore::empty(), false);
        let ns = Namespace::new("t", "rows");
        let dir = temp_dir("concern");
        let path = dir.join("rows.jsonl");
        std::fs::write(&path, (0..1500).map(|i| format!("{{\"_id\": {i}}}\n")).collect::<String>()).unwrap();
        conn.concern_fails_next();
        let w = import_counting(&conn, &ns, &path).await.0.unwrap();
        assert_eq!((w.inserted, w.unconfirmed, w.unconfirmed_docs), (1500, true, 1000), "the first batch's 1,000 are not confirmed: {w:?}");
        assert!(w.warning.as_deref().is_some_and(|w| w.contains("write concern error 64")), "{w:?}");
        std::fs::remove_dir_all(dir).ok();
    }

    /// The same through a whole import: a validator refuses two documents
    /// of the first batch, then the file turns out malformed.
    #[tokio::test]
    async fn a_stopped_import_names_what_was_refused() {
        let conn = Conn::mock(MockStore::empty(), false);
        let ns = Namespace::new("t", "checked");
        conn.create_collection_with(&ns, doc! { "validator": { "$jsonSchema": { "required": ["n"] } } }).await.unwrap();
        let dir = temp_dir("refused");
        let path = dir.join("rows.jsonl");
        let good: String = (0..1500).map(|i| if i == 3 || i == 9 { format!("{{\"_id\": {i}}}\n") } else { format!("{{\"_id\": {i}, \"n\": {i}}}\n") }).collect();
        std::fs::write(&path, format!("{good}{{oops\n")).unwrap();
        let e = import_counting(&conn, &ns, &path).await.0.unwrap_err().to_string();
        assert!(e.starts_with("JSON: ") && e.ends_with("(after inserting 998 documents, 2 refused (the first: server error 121: Document failed validation))"), "{e}");
        std::fs::remove_dir_all(dir).ok();
    }

    /// A file found malformed part way leaves the batches before it
    /// inserted, and the error says how many; one malformed from the start
    /// inserts nothing and says only what is wrong.
    #[tokio::test]
    async fn a_file_malformed_part_way_says_what_went_in() {
        let conn = Conn::mock(MockStore::empty(), false);
        let dir = temp_dir("malformed");
        let lines = dir.join("lines.jsonl");
        std::fs::write(&lines, format!("{}{{\"_id\": oops}}\n{{\"_id\": -1}}\n", (0..1500).map(|i| format!("{{\"_id\": {i}}}\n")).collect::<String>())).unwrap();
        let ns = Namespace::new("t", "lines");
        let e = import_counting(&conn, &ns, &lines).await.0.unwrap_err().to_string();
        assert!(e.starts_with("JSON: ") && e.ends_with("(after inserting 1000 documents)"), "{e}");
        assert_eq!(conn.count(&ns, doc! {}).await.unwrap(), 1000);

        let array = dir.join("items.json");
        std::fs::write(&array, format!("[{}, 7]", (0..1200).map(|i| format!("{{\"_id\": {i}}}")).collect::<Vec<_>>().join(","))).unwrap();
        let ns = Namespace::new("t", "items");
        let e = import_counting(&conn, &ns, &array).await.0.unwrap_err().to_string();
        assert_eq!(e, "item 1201: not an object (after inserting 1000 documents)");

        let broken = dir.join("broken.json");
        std::fs::write(&broken, "[{\"a\": 1},, ]").unwrap();
        let ns = Namespace::new("t", "broken");
        let e = import_counting(&conn, &ns, &broken).await.0.unwrap_err();
        assert!(matches!(&e, CoreError::InvalidInput(m) if m.starts_with("JSON: ") && !m.contains("after")), "{e:?}");
        assert_eq!(conn.count(&ns, doc! {}).await.unwrap(), 0);
        assert!(parse_documents("{\"a\": 1}", ImportFormat::JsonArray).unwrap_err().to_string().contains("expected a JSON array"));
        assert!(parse_documents("[{\"a\": 1}] x", ImportFormat::JsonArray).is_err(), "trailing text");
        std::fs::remove_dir_all(dir).ok();
    }

    /// A date after the year 9999 (or before 1970) exports as its
    /// milliseconds and imports back as itself: bson's relaxed form wrote
    /// every date after 9999 as 9999-12-31T23:59:59.999999999Z.
    #[tokio::test]
    async fn far_dates_round_trip_through_json() {
        let conn = Conn::mock(MockStore::empty(), false);
        let ns = Namespace::new("t", "dates");
        let dates = [253_402_300_800_000_i64, i64::MAX, -1, 253_402_300_799_999, 0, 1_700_000_000_120];
        let docs: Vec<Document> = dates.iter().enumerate().map(|(i, ms)| doc! { "_id": i as i32, "at": bson::DateTime::from_millis(*ms) }).collect();
        conn.insert_many(&ns, docs.clone()).await.unwrap();
        let dir = std::env::temp_dir().join(format!("rongo-dates-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dates.jsonl");
        let spec = ExportSpec { ns: ns.clone(), filter: doc! {}, sort: Some(doc! { "_id": 1 }), projection: None, format: ExportFormat::JsonLines, path: path.clone(), fields: None };
        run_export(&conn, &spec).await.unwrap();
        let target = Namespace::new("t", "back");
        run_import(&conn, &target, &path).await.unwrap();
        let raw = conn.find_page(&crate::api::FindSpec { sort: Some(doc! { "_id": 1 }), ..crate::api::FindSpec::all(target, 10) }).await.unwrap().0;
        let back: Vec<Document> = raw.into_iter().map(|d| Document::try_from(d).unwrap()).collect();
        assert_eq!(back, docs, "{}", std::fs::read_to_string(&path).unwrap());
        std::fs::remove_dir_all(dir).ok();
    }

    /// Exported JSON keeps each document's field order (and so compound
    /// `_id`s), and import reads it back in that order.
    #[tokio::test]
    async fn json_export_keeps_field_order() {
        let conn = Conn::mock(MockStore::test_data(), false);
        let ns = Namespace::new("order", "c");
        let original = doc! { "_id": { "z": 1, "a": 2 }, "name": "x", "age": 3 };
        conn.insert_one(&ns, original.clone()).await.unwrap();
        let dir = std::env::temp_dir().join(format!("rongo-order-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("c.jsonl");
        let spec = ExportSpec { ns: ns.clone(), filter: doc! {}, sort: None, projection: None, format: ExportFormat::JsonLines, path: path.clone(), fields: None };
        run_export(&conn, &spec).await.unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(r#"{"_id":{"z":1,"a":2},"name":"x","age":3}"#), "{text}");
        let target = Namespace::new("order", "copy");
        run_import(&conn, &target, &path).await.unwrap();
        let raw = conn.find_page(&crate::api::FindSpec::all(target, 10)).await.unwrap().0;
        let back = Document::try_from(raw[0].clone()).unwrap();
        assert_eq!(back.keys().collect::<Vec<_>>(), ["_id", "name", "age"]);
        assert_eq!(back.get_document("_id").unwrap().keys().collect::<Vec<_>>(), ["z", "a"]);
        std::fs::remove_dir_all(dir).ok();
    }

    /// Import inserts every document it can. One whose `_id` is already in
    /// the collection is skipped and counted, one refused for another
    /// reason (a validator) is counted with the first reason, and the
    /// counts are exact across batches.
    #[tokio::test]
    async fn import_skips_existing_ids_and_counts_exactly() {
        let conn = Conn::mock(MockStore::empty(), false);
        let ns = Namespace::new("t", "c");
        conn.create_collection_with(&ns, doc! { "validator": { "$jsonSchema": { "required": ["n"] } } }).await.unwrap();
        conn.insert_many(&ns, vec![doc! { "_id": 5, "n": 0 }, doc! { "_id": 1500, "n": 0 }]).await.unwrap();
        // 2,500 documents in three batches: _id 5 and 1500 are there
        // already, and 7 and 2000 have no "n".
        let text: String = (0..2500).map(|i| if i == 7 || i == 2000 { format!("{{\"_id\": {i}}}\n") } else { format!("{{\"_id\": {i}, \"n\": {i}}}\n") }).collect();
        let dir = std::env::temp_dir().join(format!("rongo-import-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rows.jsonl");
        std::fs::write(&path, text).unwrap();
        let w = run_import(&conn, &ns, &path).await.unwrap();
        assert_eq!((w.inserted, w.duplicates, w.cancelled), (2496, 2, false), "{w:?}");
        let warning = w.warning.unwrap_or_default();
        assert!(warning.starts_with("2 documents refused") && warning.contains("Document failed validation"), "{warning}");
        assert_eq!(conn.count(&ns, doc! {}).await.unwrap(), 2498);
        assert_eq!(conn.count(&ns, doc! { "_id": 5, "n": 0 }).await.unwrap(), 1, "what was there stays");
        std::fs::remove_dir_all(dir).ok();
    }

    /// An export over a symlink replaces the file it points to and leaves
    /// the link; the file it replaces keeps its permissions; a file of the
    /// user's own named `<name>.part` is never touched.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_export_replaces_the_real_file_as_it_was_set_up() {
        use std::os::unix::fs::PermissionsExt;
        let conn = Conn::mock(MockStore::test_data(), false);
        let dir = temp_dir("replace");
        let spec = |path: &Path| ExportSpec { ns: Namespace::new("demo-games", "providers"), filter: doc! {}, sort: None, projection: None, format: ExportFormat::JsonArray, path: path.to_path_buf(), fields: None };
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let real = dir.join("real.json");
        std::fs::write(&real, "previous").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o604)).unwrap();
        let link = dir.join("link.json");
        std::os::unix::fs::symlink("real.json", &link).unwrap();
        std::fs::write(dir.join("link.json.part"), "mine").unwrap();
        std::fs::write(dir.join("real.json.part"), "mine too").unwrap();

        assert_eq!(run_export(&conn, &spec(&link)).await.unwrap(), 8);
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "the link stays a link");
        assert!(std::fs::read_to_string(&real).unwrap().starts_with("[\n{"), "the file it points to has the export");
        assert_eq!(mode(&real), 0o604, "and keeps its permissions");
        assert_eq!((std::fs::read_to_string(dir.join("link.json.part")).unwrap(), std::fs::read_to_string(dir.join("real.json.part")).unwrap()), ("mine".into(), "mine too".into()));
        let mut names: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names, ["link.json", "link.json.part", "real.json", "real.json.part"], "nothing left beside them");

        // Cancelled: the user's .part file is still theirs.
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let bets = ExportSpec { ns: Namespace::new("demo-games", "bet_logs"), ..spec(&real) };
        assert!(matches!(export(&conn, &bets, &Progress::with_hook(move |_| token.cancel()), &cancel).await, Err(CoreError::Cancelled)));
        assert_eq!(std::fs::read_to_string(dir.join("real.json.part")).unwrap(), "mine too");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 4);

        // A link to a file not made yet: the export makes it.
        let dangling = dir.join("later.json");
        std::os::unix::fs::symlink(dir.join("made.json"), &dangling).unwrap();
        assert_eq!(run_export(&conn, &spec(&dangling)).await.unwrap(), 8);
        assert!(std::fs::symlink_metadata(&dangling).unwrap().file_type().is_symlink());
        assert!(std::fs::read_to_string(dir.join("made.json")).unwrap().starts_with("[\n{"));
        std::fs::remove_dir_all(dir).ok();
    }

    /// An export is written beside its file into a temporary `.part` file
    /// and put in place only once complete: a cancelled or failed one leaves
    /// the file that was there as it was, and nothing next to it.
    #[tokio::test]
    async fn a_cancelled_or_failed_export_keeps_the_previous_file() {
        let conn = Conn::mock(MockStore::test_data(), false);
        let dir = std::env::temp_dir().join(format!("rongo-part-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bets.json");
        std::fs::write(&path, "previous").unwrap();
        let spec = ExportSpec { ns: Namespace::new("demo-games", "bet_logs"), filter: doc! {}, sort: None, projection: None, format: ExportFormat::JsonArray, path: path.clone(), fields: None };
        let files = || std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().to_string()).collect::<Vec<_>>();

        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let progress = Progress::with_hook(move |_| token.cancel());
        assert!(matches!(export(&conn, &spec, &progress, &cancel).await, Err(CoreError::Cancelled)));
        assert!(std::fs::read_to_string(&path).unwrap() == "previous", "the file that was there is kept");
        assert_eq!(files(), ["bets.json"]);

        // The target is a folder: the finished export can't be put there.
        let folder = ExportSpec { path: dir.join("taken"), ..spec.clone() };
        std::fs::create_dir(&folder.path).unwrap();
        assert!(run_export(&conn, &folder).await.is_err());
        let mut names = files();
        names.sort();
        assert_eq!(names, ["bets.json", "taken"]);

        assert_eq!(run_export(&conn, &spec).await.unwrap(), 25_000);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("[\n{") && text.ends_with("}\n]\n"), "complete");
        let mut names = files();
        names.sort();
        assert_eq!(names, ["bets.json", "taken"]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn export_then_import_round_trips() {
        let conn = Conn::mock(MockStore::test_data(), false);
        let ns = Namespace::new("demo-games", "providers");
        let dir = std::env::temp_dir().join(format!("rongo-transfer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (format, ext) in [(ExportFormat::JsonArray, "json"), (ExportFormat::JsonLines, "jsonl"), (ExportFormat::Csv, "csv")] {
            let path = dir.join(format!("providers.{ext}"));
            let spec = ExportSpec { ns: ns.clone(), filter: doc! {}, sort: Some(doc! { "code": 1 }), projection: None, format, path: path.clone(), fields: None };
            assert_eq!(run_export(&conn, &spec).await.unwrap(), 8);
            let target = Namespace::new("copy", ext);
            assert_eq!(run_import(&conn, &target, &path).await.unwrap().inserted, 8);
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

    /// The one-pass writers give exactly what the general way gave: JSON as
    /// serde_json writes bson's relaxed Extended JSON (Int64s replaced
    /// first), CSV cells as `delimited_line` writes `csv_value`. The one
    /// difference is meant: a date after 9999 is written as its
    /// milliseconds, where bson wrote 9999-12-31T23:59:59.999999999Z.
    #[test]
    fn export_writers_match_their_references() {
        let far = bson::DateTime::from_millis(253_402_300_800_000);
        assert_eq!(serde_json::to_string(&Bson::DateTime(far).into_relaxed_extjson()).unwrap(), r#"{"$date":"9999-12-31T23:59:59.999999999Z"}"#, "bson's quirk");
        // The old way, but with far dates as the new writer writes them.
        fn keep_longs(v: Bson) -> Bson {
            match v {
                Bson::DateTime(d) if d.timestamp_millis() >= 253_402_300_800_000 => Bson::Document(doc! { "$date": { "$numberLong": d.timestamp_millis().to_string() } }),
                Bson::Document(d) => Bson::Document(d.into_iter().map(|(k, v)| (k, keep_longs(v))).collect()),
                Bson::Array(a) => Bson::Array(a.into_iter().map(keep_longs).collect()),
                other => reference_longs(other),
            }
        }
        let every = crate::format::every_type_doc();
        let scope = Bson::JavaScriptCodeWithScope(bson::JavaScriptCodeWithScope { code: "f".into(), scope: doc! { "n": 5_i64, "in": [7_i64, { "m": 8_i64 }] } });
        let dates: Vec<Bson> = (-3000_i64..3000).map(|k| Bson::DateTime(bson::DateTime::from_millis(k * 98_765_432_101 + k))).collect();
        let mut docs = vec![every.clone(), doc! { "code": scope, "n": [1_i64, { "m": 2_i64 }], "dates": dates }];
        let demo = MockStore::test_data();
        for coll in ["users", "providers", "bet_logs"] {
            docs.extend(demo.find(&Namespace::new("demo-games", coll), &doc! {}, None, None, 0, Some(2000)).unwrap());
        }
        for d in &docs {
            let mut out = String::new();
            write_ejson_doc(&mut out, &bson::RawDocumentBuf::try_from(d).unwrap(), true).unwrap();
            let want = serde_json::to_string(&keep_longs(Bson::Document(d.clone())).into_relaxed_extjson()).unwrap();
            let at = out.bytes().zip(want.bytes()).position(|(a, b)| a != b).unwrap_or(out.len().min(want.len()));
            let around = |s: &str| s.get(at.saturating_sub(60)..(at + 60).min(s.len())).unwrap_or_default().to_string();
            assert!(out == want, "first difference at {at}: {:?} / {:?}", around(&out), around(&want));
        }

        let mut values: Vec<Bson> = Vec::new();
        for v in every.values() {
            values.push(v.clone());
            if let Bson::Array(a) = v {
                values.extend(a.iter().cloned());
            }
        }
        let texts = ["=1+1", "'=x", "''=x", "-5", " lead", "trail ", " both ", "a,b", "q\"t", "l\nf", "c\rr", "t\tab", "'plain", "@me", "+", "", " ", "'"];
        values.extend(texts.map(Bson::from));
        for sep in [',', '\t'] {
            for v in values.iter().map(Some).chain([None]) {
                let (mut out, mut scratch) = (String::new(), String::new());
                push_csv_value(&mut out, &mut scratch, v, sep);
                assert_eq!(out, delimited_line([csv_value(v).as_str()].into_iter(), sep), "{v:?}");
            }
            for t in texts {
                let mut out = String::new();
                push_csv_text(&mut out, t, sep);
                assert_eq!(out, delimited_line([defuse_formula(t).as_str()].into_iter(), sep), "{t:?}");
            }
        }
    }

    /// Times export (JSON lines, JSON array, CSV) and import (JSON lines,
    /// CSV) of 100,000 documents on the demo server, and how long an import
    /// takes to insert its first batch.
    /// `cargo test -p rongo-core --release --lib -- --ignored --nocapture report_transfer_time`
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "timing; run explicitly in release mode"]
    async fn report_transfer_time() {
        let demo = MockStore::test_data();
        let bets = demo.find(&Namespace::new("demo-games", "bet_logs"), &doc! {}, None, None, 0, None).unwrap();
        let mut store = MockStore::empty();
        let ns = Namespace::new("t", "bets");
        let docs: Vec<Document> = (0..100_000).map(|i| {
            let mut d = bets[i % bets.len()].clone();
            d.insert("_id", i as i64);
            d
        }).collect();
        store.insert_unordered(&ns, docs).unwrap();
        let conn = Conn::mock(store, false);
        let dir = std::env::temp_dir().join(format!("rongo-transfer-time-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (format, ext) in [(ExportFormat::JsonLines, "jsonl"), (ExportFormat::JsonArray, "json"), (ExportFormat::Csv, "csv")] {
            let path = dir.join(format!("bets.{ext}"));
            let spec = ExportSpec { ns: ns.clone(), filter: doc! {}, sort: None, projection: None, format, path: path.clone(), fields: None };
            let t = std::time::Instant::now();
            let n = run_export(&conn, &spec).await.unwrap();
            println!("export {ext}: {:?} ({n} documents, {} bytes)", t.elapsed(), std::fs::metadata(&path).unwrap().len());
        }
        for ext in ["jsonl", "csv"] {
            let target = Namespace::new("t", format!("imported_{ext}"));
            let first = std::sync::Arc::new(std::sync::Mutex::new(None));
            let started = std::time::Instant::now();
            let seen = first.clone();
            let progress = Progress::with_hook(move |done| {
                let mut f = seen.lock().unwrap();
                if done > 0 && f.is_none() {
                    *f = Some(started.elapsed());
                }
            });
            let w = import(&conn, &target, &dir.join(format!("bets.{ext}")), ImportFormat::Auto, &progress, &CancellationToken::new()).await.unwrap();
            println!("import {ext}: {:?} ({} documents; first batch in after {:?})", started.elapsed(), w.inserted, first.lock().unwrap().unwrap());
        }
        std::fs::remove_dir_all(dir).ok();

        // The JSON writing alone, from the raw documents a real server
        // sends, against the way it was done before (parse, rebuild with
        // the Int64s replaced, a serde_json tree, then text).
        let docs = conn.open_find(&ns, doc! {}, FindOpts::default()).await.unwrap().next_batch(usize::MAX).await.unwrap();
        let raw: Vec<bson::RawDocumentBuf> = docs.iter().map(|d| bson::RawDocumentBuf::try_from(d).unwrap()).collect();
        let t = std::time::Instant::now();
        let mut text = String::new();
        for d in &raw {
            write_ejson_doc(&mut text, d, true).unwrap();
            text.push('\n');
        }
        println!("JSON lines text, one pass from raw: {:?} ({} bytes)", t.elapsed(), text.len());
        let t = std::time::Instant::now();
        let mut old = String::new();
        for d in &raw {
            let parsed = Document::try_from(d.clone()).unwrap();
            old.push_str(&serde_json::to_string(&reference_longs(Bson::Document(parsed)).into_relaxed_extjson()).unwrap());
            old.push('\n');
        }
        println!("JSON lines text, as before: {:?} ({} bytes)", t.elapsed(), old.len());
        assert!(text == old);
    }

    /// The old export's step before relaxed Extended JSON: Int64 as `$numberLong`.
    fn reference_longs(v: Bson) -> Bson {
        match v {
            Bson::Int64(n) => Bson::Document(doc! { "$numberLong": n.to_string() }),
            Bson::Document(d) => Bson::Document(d.into_iter().map(|(k, v)| (k, reference_longs(v))).collect()),
            Bson::Array(a) => Bson::Array(a.into_iter().map(reference_longs).collect()),
            other => other,
        }
    }
}
