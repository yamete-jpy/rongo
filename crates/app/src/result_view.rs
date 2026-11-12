//! A page of documents with Table, Tree and JSON views. Used by collection
//! tabs, shell result tabs and info tabs.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use egui::{Align, Align2, Color32, Event, Key, Layout, Modifiers, RichText, Sense, Ui, pos2, vec2};
use egui_extras::{Column, TableBuilder};
use egui_phosphor::regular as icon;
use rongo_core::bson::{Bson, Document, RawDocumentBuf, doc};
use rongo_core::format::{self, Style};
use rongo_core::{Command, ConnId, FindSpec, Namespace, RequestId};

use crate::net::{Net, Origin, ViewReq};
use crate::{highlight, theme};

pub enum Source {
    /// Pages are fetched from the server. `max` caps the total, from a
    /// cursor's `limit()`.
    Find { conn: ConnId, spec: FindSpec, max: Option<u64> },
    /// Documents already in hand (shell output, command replies).
    Static { docs: Vec<Document>, truncated: bool },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Table,
    Tree,
    Json,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Mode::Table => "Table View",
            Mode::Tree => "Tree View",
            Mode::Json => "JSON View",
        }
    }
}

/// One level of drilling into nested values.
#[derive(Clone, Debug, PartialEq)]
enum Step {
    /// The same field of every row (Step Into Column on objects).
    Field(String),
    /// The elements of one row's array field.
    Elements { row: usize, field: String },
    /// One row's object field (Step Into Cell on objects).
    Cell { row: usize, field: String },
}

/// Temp-data key a result view sets while it wants ⌘F (pointer over it, or
/// its find bar focused). The app reads it to leave ⌘F to the view.
pub const FIND_CLAIM: &str = "result-view-find";

/// Most matches the find bar collects.
const MAX_MATCHES: usize = 100_000;

/// The JSON view's find bar (⌘F).
#[derive(Default)]
struct Find {
    text: String,
    case: bool,
    /// (line, start, end) byte offsets of each match, in order.
    matches: Vec<(usize, usize, usize)>,
    /// The text and case `matches` is for; `None` searches again.
    searched: Option<(String, bool)>,
    current: usize,
    focus: bool,
    scroll_to: bool,
    running: Option<Search>,
}

/// A search on a background thread: two million lines take ~90 ms, too
/// long for every keystroke. Dropping it stops the thread.
struct Search {
    key: (String, bool),
    rx: std::sync::mpsc::Receiver<Vec<(usize, usize, usize)>>,
    cancel: Arc<std::sync::atomic::AtomicBool>,
}

impl Search {
    fn start(lines: Arc<JsonText>, key: (String, bool), ctx: egui::Context) -> Self {
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

/// Requests a view makes that need the app (dialogs, confirmations, writes).
pub enum ViewAction {
    /// Edit a document. The row may be partial (a projection), so the app
    /// fetches the whole document by `_id` first.
    Edit { conn: ConnId, ns: Namespace, doc: Document },
    /// Insert a copy of a document, fetched whole by `_id` like Edit.
    Clone { conn: ConnId, ns: Namespace, doc: Document },
    View { doc: Document },
    Insert { conn: ConnId, ns: Namespace, template: Option<Document> },
    Delete { conn: ConnId, ns: Namespace, ids: Vec<Bson> },
    /// Open the value editor for one field of one document.
    EditValue { conn: ConnId, ns: Namespace, id: Bson, path: String, value: Bson },
    /// Apply an update to one document right away (set null, remove field).
    UpdateOne { conn: ConnId, ns: Namespace, id: Bson, update: Document },
    Paste { conn: ConnId, ns: Namespace, text: String },
    /// Open the Update (or Delete) dialog. `field` is the selected cell's path
    /// and value, which pre-fills the `$set`.
    Bulk { conn: ConnId, ns: Namespace, delete: bool, selected: Vec<Bson>, query: Document, field: Option<(String, Bson)> },
    Export { conn: ConnId, spec: FindSpec },
    /// The view re-sorted itself from a column menu.
    SortChanged(Document),
}

const PAGE_SIZES: [i64; 6] = [20, 50, 100, 200, 500, 1000];
const MAX_PAGE_SIZE: i64 = crate::settings::MAX_PAGE_SIZE;
static DEFAULT_PAGE_SIZE: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(50);

/// Page size for views opened from now on.
pub fn set_default_page_size(n: i64) {
    DEFAULT_PAGE_SIZE.store(n.clamp(1, MAX_PAGE_SIZE), std::sync::atomic::Ordering::Relaxed);
}
static NULL: Bson = Bson::Null;

/// Parsed documents kept at once before the cache starts over. Scrolling
/// parses rows as they come into view.
const MAX_PARSED: usize = 500;

/// A page of documents, kept as raw BSON and parsed one at a time when first
/// used. Parsed, nested documents take up to twenty times their raw size, and
/// most of a 50,000-document page is never on screen. Clones share the raw
/// bytes and the parsed cache.
#[derive(Clone, Default)]
struct Docs(Arc<DocsInner>);

#[derive(Default)]
struct DocsInner {
    raw: Arc<Vec<RawDocumentBuf>>,
    parsed: Vec<std::sync::OnceLock<Box<Bson>>>,
    count: std::sync::atomic::AtomicUsize,
}

impl Docs {
    fn new(raw: Vec<RawDocumentBuf>) -> Self {
        Self::shared(Arc::new(raw))
    }

    fn shared(raw: Arc<Vec<RawDocumentBuf>>) -> Self {
        let parsed = (0..raw.len()).map(|_| std::sync::OnceLock::new()).collect();
        Self(Arc::new(DocsInner { raw, parsed, count: Default::default() }))
    }

    fn from_docs<'a>(docs: impl IntoIterator<Item = &'a Document>) -> Self {
        Self::new(docs.into_iter().filter_map(|d| RawDocumentBuf::try_from(d).ok()).collect())
    }

    fn len(&self) -> usize {
        self.0.raw.len()
    }

    fn is_empty(&self) -> bool {
        self.0.raw.is_empty()
    }

    fn get(&self, i: usize) -> Option<&Bson> {
        let cell = self.0.parsed.get(i)?;
        Some(cell.get_or_init(|| {
            self.0.count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Box::new(match Document::try_from(self.0.raw[i].as_ref()) {
                Ok(d) => Bson::Document(d),
                Err(e) => Bson::String(format!("(unreadable BSON: {e})")),
            })
        }))
    }

    fn doc(&self, i: usize) -> Option<&Document> {
        self.get(i)?.as_document()
    }

    fn raw(&self) -> &Arc<Vec<RawDocumentBuf>> {
        &self.0.raw
    }

    /// How many documents have been parsed.
    fn parsed(&self) -> usize {
        self.0.count.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The page with document `i` replaced, or removed for `None`.
    fn with(&self, i: usize, doc: Option<RawDocumentBuf>) -> Self {
        let mut raw = (*self.0.raw).clone();
        match doc {
            Some(d) => raw[i] = d,
            None => {
                raw.remove(i);
            }
        }
        Self::new(raw)
    }
}

/// The JSON view's text: one buffer of lines separated by `\n`, and where
/// each line starts. Half the memory of a `String` per line.
#[derive(Default)]
struct JsonText {
    text: String,
    starts: Vec<usize>,
}

impl JsonText {
    fn push(&mut self, line: &str) {
        if !self.starts.is_empty() {
            self.text.push('\n');
        }
        self.starts.push(self.text.len());
        self.text.push_str(line);
    }

    fn len(&self) -> usize {
        self.starts.len()
    }

    fn line(&self, i: usize) -> &str {
        let end = self.starts.get(i + 1).map_or(self.text.len(), |s| s - 1);
        &self.text[self.starts[i]..end]
    }

    #[cfg(test)]
    fn from_lines<S: AsRef<str>>(lines: &[S]) -> Self {
        let mut t = Self::default();
        for l in lines {
            t.push(l.as_ref());
        }
        t
    }
}

/// A row at the current drill level.
#[derive(Clone)]
struct Row<'a> {
    /// Index of the document it came from.
    origin: usize,
    value: &'a Bson,
    /// Dotted path from the document root to `value` ("" at the top level).
    path: String,
}

fn join(prefix: &str, field: &str) -> String {
    if prefix.is_empty() { field.to_string() } else { format!("{prefix}.{field}") }
}

pub struct ResultView {
    pub id: u64,
    pub title: String,
    pub source: Source,
    page_size: i64,
    /// The page size field's text while it is being typed.
    page_text: String,
    offset: u64,
    /// The page's documents. Shared, so a frame can borrow them without
    /// cloning 50,000 documents.
    rows: Docs,
    has_more: bool,
    needs_fetch: bool,
    loading: Option<RequestId>,
    counting: Option<RequestId>,
    pub error: Option<String>,
    elapsed_ms: Option<u64>,
    total: Option<u64>,
    pub mode: Mode,
    selected: BTreeSet<usize>,
    anchor: Option<usize>,
    /// Column of the last clicked cell, for Step Into and Field shortcuts.
    selected_col: Option<String>,
    hidden: BTreeSet<String>,
    drill: Vec<Step>,
    /// A static result sorted from a column header: the path and direction,
    /// and the documents' indexes in that order.
    local_sort: Option<(String, i32, Vec<usize>)>,
    /// Column names at the current drill level, computed once per change.
    columns_cache: Option<Vec<String>>,
    /// The JSON view's text, one entry per line, built on a background thread
    /// on first use.
    json_lines: Option<Arc<JsonText>>,
    json_building: Option<std::sync::mpsc::Receiver<JsonText>>,
    /// Measured height of each Tree view row, for drawing only visible rows.
    tree_heights: Vec<f32>,
    find: Option<Find>,
    /// The Refresh Document request in flight, if any.
    refreshing: Option<RequestId>,
    /// ⌘A in the JSON view: all of its text is selected, on screen or not.
    json_all: bool,
    /// First JSON line in view last frame, where a new search starts.
    json_top: usize,
    /// Column order chosen by dragging headers, per drill level (keyed by
    /// the drill path). Columns not in the list keep their own order, after.
    col_order: HashMap<String, Vec<String>>,
    /// Each column's width as last drawn, so a column keeps its width when
    /// it moves (the table's own widths go by position).
    col_widths: HashMap<String, f32>,
    /// The header being dragged.
    dragging_col: Option<String>,
    /// Width of each toolbar group (paging, document buttons, view mode)
    /// as last drawn, to decide which share a row.
    toolbar_widths: [f32; 3],
    /// Last page was pressed before the count was known: jump there when
    /// the count arrives.
    pending_last: bool,
    /// Bottom of the last JSON line drawn and of the visible area, last frame.
    #[cfg(test)]
    json_fill: Option<(f32, f32)>,
    /// Header cells as last drawn.
    #[cfg(test)]
    header_rects: Vec<(String, egui::Rect)>,
}

impl ResultView {
    pub fn new(id: u64, title: impl Into<String>, source: Source) -> Self {
        let total = match &source {
            Source::Static { docs, .. } => Some(docs.len() as u64),
            Source::Find { .. } => None,
        };
        Self {
            id,
            title: title.into(),
            source,
            page_size: DEFAULT_PAGE_SIZE.load(std::sync::atomic::Ordering::Relaxed),
            page_text: String::new(),
            offset: 0,
            rows: Docs::default(),
            has_more: false,
            needs_fetch: true,
            loading: None,
            counting: None,
            error: None,
            elapsed_ms: None,
            total,
            mode: Mode::Table,
            selected: BTreeSet::new(),
            anchor: None,
            selected_col: None,
            hidden: BTreeSet::new(),
            drill: Vec::new(),
            local_sort: None,
            columns_cache: None,
            json_lines: None,
            json_building: None,
            tree_heights: Vec::new(),
            find: None,
            refreshing: None,
            json_all: false,
            json_top: 0,
            col_order: HashMap::new(),
            col_widths: HashMap::new(),
            dragging_col: None,
            toolbar_widths: [0.0; 3],
            pending_last: false,
            #[cfg(test)]
            json_fill: None,
            #[cfg(test)]
            header_rects: Vec::new(),
        }
    }

    /// Replace the query and start again from the first page.
    pub fn set_spec(&mut self, spec: FindSpec) {
        if let Source::Find { spec: s, .. } = &mut self.source {
            *s = spec;
        }
        self.offset = 0;
        self.total = None;
        // A count still running was for the old query.
        self.counting = None;
        self.pending_last = false;
        self.set_drill(Vec::new());
        self.refresh();
    }

    pub fn refresh(&mut self) {
        self.needs_fetch = true;
    }

    /// Drop cached text so it is rebuilt (after a display setting changed).
    pub fn relayout(&mut self) {
        self.json_lines = None;
        self.json_building = None;
    }

    /// Forget everything derived from the rows.
    fn rows_changed(&mut self) {
        self.columns_cache = None;
        self.json_lines = None;
        self.json_building = None;
        self.json_all = false;
        self.tree_heights.clear();
    }

    fn set_drill(&mut self, drill: Vec<Step>) {
        self.drill = drill;
        self.columns_cache = None;
        self.tree_heights.clear();
        self.selected.clear();
        self.selected_col = None;
        self.hidden.clear();
    }

    fn editable(&self) -> Option<(ConnId, &Namespace)> {
        match &self.source {
            Source::Find { conn, spec, .. } => Some((*conn, &spec.ns)),
            Source::Static { .. } => None,
        }
    }

    fn fetch(&mut self, net: &mut Net) {
        self.needs_fetch = false;
        // Any page change overrides a Last page still waiting for its count.
        self.pending_last = false;
        self.refreshing = None;
        self.error = None;
        self.selected.clear();
        self.rows_changed();
        match &self.source {
            Source::Find { conn, spec, max } => {
                let mut page = spec.clone();
                page.skip = spec.skip + self.offset;
                page.limit = match max {
                    Some(m) => (m.saturating_sub(self.offset) as i64).min(self.page_size).max(0),
                    None => self.page_size,
                };
                if page.limit == 0 {
                    self.rows = Docs::default();
                    self.rows_changed();
                    self.has_more = false;
                    return;
                }
                if let Some(old) = self.loading.take() {
                    net.send(Origin::None, None, Command::Cancel(old));
                }
                self.loading = Some(net.send(Origin::View(self.id, ViewReq::Page), Some(*conn), Command::Find(page)));
            }
            Source::Static { docs, .. } => {
                let start = (self.offset as usize).min(docs.len());
                let end = (start + self.page_size as usize).min(docs.len());
                self.rows = match &self.local_sort {
                    Some((_, _, order)) => Docs::from_docs(&order[start..end].iter().map(|&i| docs[i].clone()).collect::<Vec<_>>()),
                    None => Docs::from_docs(&docs[start..end]),
                };
                self.has_more = end < docs.len();
                self.rows_changed();
            }
        }
    }

    /// Re-read one document by `_id` (Refresh Document).
    fn refresh_doc(&mut self, net: &mut Net, origin: usize) {
        let Some((conn, ns)) = self.editable().map(|(c, n)| (c, n.clone())) else { return };
        let Some(id) = self.rows.doc(origin).and_then(|d| d.get("_id")).cloned() else { return };
        let spec = FindSpec { filter: doc! { "_id": id }, ..FindSpec::all(ns, 1) };
        self.refreshing = Some(net.send(Origin::View(self.id, ViewReq::RefreshDoc(origin)), Some(conn), Command::Find(spec)));
    }

    pub fn on_page(&mut self, req: RequestId, docs: Vec<RawDocumentBuf>, has_more: bool, elapsed_ms: u64) {
        if self.loading != Some(req) {
            return;
        }
        self.loading = None;
        let capped = match &self.source {
            Source::Find { max: Some(m), .. } => self.offset + docs.len() as u64 >= *m,
            _ => false,
        };
        self.rows = Docs::new(docs);
        // Rows clicked while this page loaded were rows of the old page.
        self.selected.clear();
        self.anchor = None;
        self.has_more = has_more && !capped;
        self.elapsed_ms = Some(elapsed_ms);
        self.rows_changed();
        // Drilled into a row that no longer exists: back out.
        if self.resolved().is_empty() && !self.drill.is_empty() {
            self.set_drill(Vec::new());
        }
    }

    /// The reply to `refresh_doc`: replace the row, or drop it if it is gone.
    pub fn on_doc(&mut self, req: RequestId, origin: usize, docs: Vec<RawDocumentBuf>) {
        // A reply for a page that has since changed: `origin` is another row.
        if self.refreshing.take() != Some(req) {
            return;
        }
        let Some(old_id) = self.rows.doc(origin).and_then(|d| d.get("_id")).cloned() else { return };
        match docs.into_iter().next() {
            Some(d) if Document::try_from(d.as_ref()).ok().and_then(|p| p.get("_id").cloned()) == Some(old_id) => self.rows = self.rows.with(origin, Some(d)),
            Some(_) => return,
            None => {
                self.rows = self.rows.with(origin, None);
                self.selected.clear();
            }
        }
        self.rows_changed();
    }

    pub fn on_count(&mut self, req: RequestId, n: u64) {
        if self.counting == Some(req) {
            self.counting = None;
            let cap = match &self.source {
                Source::Find { max: Some(m), .. } => *m,
                _ => u64::MAX,
            };
            let total = n.min(cap);
            self.total = Some(total);
            if std::mem::take(&mut self.pending_last)
                && let Some(last) = self.last_offset()
                && last > self.offset
            {
                self.offset = last;
                self.needs_fetch = true;
            }
        }
    }

    /// Offset of the last page, once the count is known.
    fn last_offset(&self) -> Option<u64> {
        let page = self.page_size as u64;
        self.total.filter(|t| *t > 0).map(|t| (t - 1) / page * page)
    }

    /// Last page is available whenever there are pages after this one,
    /// counted or not.
    fn can_go_last(&self) -> bool {
        self.loading.is_none() && !self.pending_last && (self.has_more || self.last_offset().is_some_and(|l| l > self.offset))
    }

    /// Go to the last page, counting the documents first when that is not
    /// known yet.
    fn go_last(&mut self, net: &mut Net) {
        if let Some(last) = self.last_offset().filter(|l| *l > self.offset) {
            self.offset = last;
            self.fetch(net);
        } else if self.total.is_none()
            && let Source::Find { conn, spec, .. } = &self.source
        {
            self.pending_last = true;
            if self.counting.is_none() {
                let cmd = Command::Count { ns: spec.ns.clone(), filter: spec.filter.clone() };
                self.counting = Some(net.send(Origin::View(self.id, ViewReq::Count), Some(*conn), cmd));
            }
        }
    }

    /// A request of this view was cancelled from the status bar: stop
    /// waiting for it and keep what is on screen.
    pub fn on_cancelled(&mut self, req: RequestId) {
        if self.loading == Some(req) {
            self.loading = None;
        }
        if self.counting == Some(req) {
            self.counting = None;
            self.pending_last = false;
        }
        if self.refreshing == Some(req) {
            self.refreshing = None;
        }
    }

    pub fn on_error(&mut self, req: RequestId, err: String) {
        if self.loading == Some(req) {
            self.loading = None;
            self.rows = Docs::default();
            self.rows_changed();
            self.error = Some(err);
        } else if self.counting == Some(req) {
            self.counting = None;
            self.pending_last = false;
            self.error = Some(err);
        }
    }

    /// Rows at the current drill level.
    fn resolved(&self) -> Rows<'_> {
        resolve(&self.rows, &self.drill)
    }

    /// Column names at the current drill level (before hiding), cached. At
    /// the top level they come from the raw documents, so finding them
    /// parses nothing.
    fn columns(&mut self, resolved: &Rows) -> Vec<String> {
        self.columns_cache
            .get_or_insert_with(|| match resolved {
                Rows::Top(docs) => raw_columns(docs.raw()),
                Rows::Drilled(rows) => {
                    let mut cols = format::columns(rows.iter().filter_map(|r| r.value.as_document()));
                    if rows.iter().any(|r| !matches!(r.value, Bson::Document(_))) {
                        cols.insert(0, "(value)".to_string());
                    }
                    cols
                }
            })
            .clone()
    }
}

/// Field names across raw documents, in first-seen order, `_id` first.
fn raw_columns(docs: &[RawDocumentBuf]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut cols = Vec::new();
    for d in docs {
        for (k, _) in d.iter().flatten() {
            let k = k.as_str();
            if !seen.contains(k) {
                seen.insert(k.to_string());
                cols.push(k.to_string());
            }
        }
    }
    if let Some(pos) = cols.iter().position(|c| c == "_id") {
        let id = cols.remove(pos);
        cols.insert(0, id);
    }
    cols
}

/// Rows at a drill level. At the top level each row is a document, fetched
/// (and parsed) only when asked for, so drawing the rows in view parses
/// only those.
enum Rows<'a> {
    Top(&'a Docs),
    Drilled(Vec<Row<'a>>),
}

impl<'a> Rows<'a> {
    fn len(&self) -> usize {
        match self {
            Rows::Top(d) => d.len(),
            Rows::Drilled(r) => r.len(),
        }
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn get(&self, i: usize) -> Option<Row<'a>> {
        match self {
            Rows::Top(d) => d.get(i).map(|value| Row { origin: i, value, path: String::new() }),
            Rows::Drilled(r) => r.get(i).cloned(),
        }
    }
}

/// The rows one drill step below `len` rows given by `get`.
fn step_rows<'a>(len: usize, get: impl Fn(usize) -> Option<Row<'a>>, step: &Step) -> Vec<Row<'a>> {
    match step {
        Step::Field(f) => (0..len)
            .filter_map(get)
            .filter_map(|r| match r.value {
                Bson::Document(d) => Some(Row { origin: r.origin, value: d.get(f).unwrap_or(&NULL), path: join(&r.path, f) }),
                _ => None,
            })
            .collect(),
        Step::Elements { row, field } => match get(*row) {
            Some(r) => match r.value.as_document().and_then(|d| d.get(field)) {
                Some(Bson::Array(items)) => {
                    let base = join(&r.path, field);
                    items.iter().enumerate().map(|(i, x)| Row { origin: r.origin, value: x, path: format!("{base}.{i}") }).collect()
                }
                _ => Vec::new(),
            },
            None => Vec::new(),
        },
        Step::Cell { row, field } => match get(*row) {
            Some(r) => match r.value.as_document().and_then(|d| d.get(field)) {
                Some(v @ Bson::Document(_)) => vec![Row { origin: r.origin, value: v, path: join(&r.path, field) }],
                _ => Vec::new(),
            },
            None => Vec::new(),
        },
    }
}

/// Rows of `docs` at the drill level described by `drill`.
fn resolve<'a>(docs: &'a Docs, drill: &[Step]) -> Rows<'a> {
    let Some((first, rest)) = drill.split_first() else { return Rows::Top(docs) };
    let mut rows = step_rows(docs.len(), |i| docs.get(i).map(|value| Row { origin: i, value, path: String::new() }), first);
    for step in rest {
        let next = step_rows(rows.len(), |i| rows.get(i).cloned(), step);
        rows = next;
    }
    Rows::Drilled(rows)
}

impl ResultView {
    /// Dotted prefix shared by every row, when the drill path is all fields.
    fn order_key(&self) -> String {
        format!("{:?}", self.drill)
    }

    /// `cols` in the order the user dragged them into at this drill level.
    fn ordered(&self, mut cols: Vec<String>) -> Vec<String> {
        if let Some(order) = self.col_order.get(&self.order_key()) {
            cols.sort_by_key(|c| order.iter().position(|o| o == c).unwrap_or(usize::MAX));
        }
        cols
    }

    /// Move column `from` to just before `before` (`None`: to the end).
    fn move_column(&mut self, from: &str, before: Option<&str>) {
        let all = self.columns_cache.clone().unwrap_or_default();
        let mut order = self.ordered(all);
        if !order.iter().any(|c| c == from) || before == Some(from) {
            return;
        }
        order.retain(|c| c != from);
        let at = before.and_then(|b| order.iter().position(|c| c == b)).unwrap_or(order.len());
        order.insert(at, from.to_string());
        self.col_order.insert(self.order_key(), order);
    }

    fn shared_prefix(&self) -> Option<String> {
        self.drill.iter().try_fold(String::new(), |acc, s| match s {
            Step::Field(f) => Some(join(&acc, f)),
            _ => None,
        })
    }

    fn selected_origins(&self) -> Vec<usize> {
        if self.selected.is_empty() {
            return Vec::new();
        }
        // At the top level a row is its document: no need to resolve them all.
        if self.drill.is_empty() {
            return self.selected.iter().copied().filter(|i| *i < self.rows.len()).collect();
        }
        let resolved = self.resolved();
        let set: BTreeSet<usize> = self.selected.iter().filter_map(|i| resolved.get(*i).map(|r| r.origin)).collect();
        set.into_iter().collect()
    }

    fn selected_docs(&self) -> Vec<&Document> {
        self.selected_origins().into_iter().filter_map(|o| self.rows.doc(o)).collect()
    }

    /// The selected document when exactly one row is selected. Cheap for
    /// any selection size, so it can run every frame.
    fn single_selected_doc(&self) -> Option<Document> {
        if self.selected.len() != 1 {
            return None;
        }
        match self.selected_docs().as_slice() {
            [d] => Some((*d).clone()),
            _ => None,
        }
    }

    fn selected_ids(&self) -> Vec<Bson> {
        self.selected_docs().iter().filter_map(|d| d.get("_id").cloned()).collect()
    }

    /// Path and value of the selected cell (the last one clicked), if any.
    fn selected_field(&self) -> Option<(String, Bson)> {
        let col = self.selected_col.as_deref()?;
        let row = *self.selected.iter().next()?;
        let rows = self.resolved();
        let r = rows.get(row)?;
        let (path, value) = if col == "(value)" {
            (r.path.clone(), r.value.clone())
        } else {
            (join(&r.path, col), r.value.as_document()?.get(col)?.clone())
        };
        (!path.is_empty() && path != "_id").then_some((path, value))
    }

    fn query(&self) -> Document {
        match &self.source {
            Source::Find { spec, .. } => spec.filter.clone(),
            Source::Static { .. } => Document::new(),
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, net: &mut Net) -> Vec<(u64, ViewAction)> {
        // A new request replaces one still loading (fetch cancels it): a new
        // query mustn't wait for a slow old one.
        if self.needs_fetch {
            self.fetch(net);
        }
        let mut actions = Vec::new();
        // Scrolling through a big page parses row after row: start the cache
        // over now and then. Drilled into a column, every row is in use.
        if self.drill.is_empty() && self.rows.parsed() > MAX_PARSED {
            self.rows = Docs::shared(self.rows.raw().clone());
        }
        if self.json_all && ui.input(|i| i.pointer.any_pressed()) && ui.rect_contains_pointer(ui.max_rect()) {
            self.json_all = false;
        }
        let find_focused = self.find.is_some() && ui.memory(|m| m.has_focus(self.find_id()));
        if net.keys_enabled && (find_focused || ui.rect_contains_pointer(ui.max_rect())) {
            ui.ctx().data_mut(|d| d.insert_temp(egui::Id::new(FIND_CLAIM), true));
            if ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::F)) {
                self.mode = Mode::Json;
                self.find.get_or_insert_default().focus = true;
            }
        }
        self.toolbar(ui, net, &mut actions);
        ui.add_space(2.0);
        self.breadcrumb(ui);
        if self.mode == Mode::Json && !self.rows.is_empty() {
            self.find_bar(ui);
        }

        let status_h = 26.0;
        let body_h = (ui.available_height() - status_h).max(60.0);
        ui.allocate_ui(vec2(ui.available_width(), body_h), |ui| {
            if let Some(err) = &self.error {
                ui.colored_label(theme::ERROR, format!("{} {err}", icon::WARNING_CIRCLE));
            } else if self.rows.is_empty() && self.loading.is_some() {
                ui.centered_and_justified(|ui| ui.spinner());
            } else if self.rows.is_empty() {
                let r = ui.centered_and_justified(|ui| ui.weak("No documents")).response.interact(Sense::click());
                if self.editable().is_some() {
                    r.context_menu(|ui| self.empty_menu(ui, &mut actions));
                }
            } else {
                // The JSON text is a full second copy of the page: keep it
                // only while the JSON view is showing.
                if self.mode != Mode::Json {
                    self.json_lines = None;
                    self.json_building = None;
                    self.find = None;
                    self.json_all = false;
                }
                match self.mode {
                    Mode::Table => self.table(ui, net, &mut actions),
                    Mode::Tree => self.tree(ui, net, &mut actions),
                    Mode::Json => self.json(ui),
                }
            }
        });
        if net.keys_enabled {
            self.keyboard(ui, net, &mut actions);
        }
        self.status_bar(ui, net);
        actions.into_iter().map(|a| (self.id, a)).collect()
    }

    /// Paging, document buttons and the view mode, on one row when they fit.
    /// When the view is too narrow, a group that doesn't fit starts a new
    /// row, so the buttons wrap instead of running off the edge.
    fn toolbar(&mut self, ui: &mut Ui, net: &mut Net, actions: &mut Vec<ViewAction>) {
        // Each group's width comes from the last time it was drawn; when one
        // changes, this pass is discarded and redrawn with the new widths.
        let w = self.toolbar_widths;
        // Item spacing either side of a separator.
        let gap = ui.spacing().item_spacing.x * 2.0 + 6.0;
        let rows = toolbar_rows(w, gap, ui.available_width());
        let mut measured = [0.0; 3];
        for r in 0..=rows[1].max(rows[2]) {
            ui.horizontal(|ui| {
                if rows[0] == r {
                    measured[0] = ui.horizontal(|ui| self.paging(ui, net)).response.rect.width();
                }
                if rows[1] == r {
                    if rows[0] == r {
                        ui.separator();
                    }
                    measured[1] = ui.horizontal(|ui| self.document_buttons(ui, actions)).response.rect.width();
                }
                if rows[2] == r {
                    measured[2] = ui
                        .with_layout(Layout::right_to_left(Align::Center), |ui| {
                            self.mode_picker(ui);
                            ui.min_rect().width()
                        })
                        .inner;
                }
            });
        }
        if measured.iter().zip(w).any(|(m, w)| (m - w).abs() > 0.5) {
            self.toolbar_widths = measured;
            ui.ctx().request_discard("result view toolbar widths changed");
        }
    }

    fn paging(&mut self, ui: &mut Ui, net: &mut Net) {
        let loading = self.loading.is_some();
        if ui.add_enabled(!loading, egui::Button::new(icon::ARROWS_CLOCKWISE)).on_hover_text("Refresh view (⌘R)").clicked() {
            self.fetch(net);
        }
        if loading
            && ui.button(icon::STOP).on_hover_text("Cancel").clicked()
            && let Some(id) = self.loading.take()
        {
            net.send(Origin::None, None, Command::Cancel(id));
        }
        ui.separator();
        let at_start = self.offset == 0;
        let page = self.page_size as u64;
        if ui.add_enabled(!at_start && !loading, egui::Button::new(icon::CARET_LINE_LEFT)).on_hover_text("First page").clicked() {
            self.offset = 0;
            self.fetch(net);
        }
        if ui.add_enabled(!at_start && !loading, egui::Button::new(icon::CARET_LEFT)).on_hover_text("Previous page").clicked() {
            self.offset = self.offset.saturating_sub(page);
            self.fetch(net);
        }
        if ui.add_enabled(self.has_more && !loading, egui::Button::new(icon::CARET_RIGHT)).on_hover_text("Next page").clicked() {
            self.offset += page;
            self.fetch(net);
        }
        let last_hint = if self.total.is_some() { "Last page" } else { "Last page (counts the documents first)" };
        if ui.add_enabled(self.can_go_last(), egui::Button::new(icon::CARET_LINE_RIGHT)).on_hover_text(last_hint).clicked() {
            self.go_last(net);
        }
        // Page size: type any number (Enter applies) or pick one from ▾.
        let mut size = self.page_size;
        let field_id = egui::Id::new((self.id, "page-size"));
        if !ui.memory(|m| m.has_focus(field_id)) {
            self.page_text = self.page_size.to_string();
        }
        let field = ui.add(egui::TextEdit::singleline(&mut self.page_text).id(field_id).desired_width(48.0).horizontal_align(Align::RIGHT))
            .on_hover_text(format!("Documents per page (1–{MAX_PAGE_SIZE}); press Enter to apply"));
        self.page_text.retain(|c| c.is_ascii_digit());
        if field.lost_focus() {
            size = parse_page_size(&self.page_text, self.page_size);
            self.page_text = size.to_string();
        }
        ui.menu_button(icon::CARET_DOWN, |ui| {
            for s in PAGE_SIZES {
                if ui.selectable_label(s == self.page_size, s.to_string()).clicked() {
                    size = s;
                    ui.close();
                }
            }
        });
        if size != self.page_size {
            self.page_size = size;
            self.offset = self.offset / size as u64 * size as u64;
            self.fetch(net);
        }
        let shown = self.rows.len() as u64;
        ui.label(if shown == 0 { "No documents".to_string() } else { format!("Documents {} to {}", self.offset + 1, self.offset + shown) });
    }

    fn document_buttons(&mut self, ui: &mut Ui, actions: &mut Vec<ViewAction>) {
        let editable = self.editable().map(|(c, ns)| (c, ns.clone()));
        // Every frame: look at the documents only for a single selection.
        // ⌘A on 50,000 rows would otherwise parse and clone them all.
        let one = self.single_selected_doc();
        let any = !self.selected.is_empty();
        let can = editable.is_some();
        if ui.add_enabled(can, egui::Button::new(icon::FILE_PLUS)).on_hover_text("Insert document (⌘D)").clicked()
            && let Some((conn, ns)) = editable.clone()
        {
            actions.push(ViewAction::Insert { conn, ns, template: None });
        }
        if ui.add_enabled(can && one.is_some(), egui::Button::new(icon::PENCIL_SIMPLE)).on_hover_text("Edit document (⌘J)").clicked()
            && let (Some((conn, ns)), Some(doc)) = (editable.clone(), one.clone())
        {
            actions.push(ViewAction::Edit { conn, ns, doc });
        }
        if ui.add_enabled(can && one.is_some(), egui::Button::new(icon::COPY)).on_hover_text("Clone document").clicked()
            && let (Some((conn, ns)), Some(doc)) = (editable.clone(), one)
        {
            actions.push(ViewAction::Clone { conn, ns, doc });
        }
        if ui.add_enabled(can, egui::Button::new(icon::PENCIL_LINE)).on_hover_text("Update dialog (⌘U)").clicked()
            && let Some((conn, ns)) = editable.clone()
        {
            actions.push(ViewAction::Bulk { conn, ns, delete: false, selected: self.selected_ids(), query: self.query(), field: self.selected_field() });
        }
        let del = egui::Button::new(RichText::new(icon::TRASH).color(if any { theme::ERROR } else { theme::MUTED }));
        if ui.add_enabled(can && any, del).on_hover_text("Delete selected documents (⇧⌫)").clicked()
            && let Some((conn, ns)) = editable.clone()
        {
            actions.push(ViewAction::Delete { conn, ns, ids: self.selected_ids() });
        }
        if ui.add_enabled(can, egui::Button::new(icon::BROOM)).on_hover_text("Delete dialog (F8)").clicked()
            && let Some((conn, ns)) = editable
        {
            actions.push(ViewAction::Bulk { conn, ns, delete: true, selected: self.selected_ids(), query: self.query(), field: None });
        }
    }

    fn mode_picker(&mut self, ui: &mut Ui) {
        let loading = self.loading.is_some();
        egui::ComboBox::from_id_salt((self.id, "mode")).width(110.0).selected_text(self.mode.label()).show_ui(ui, |ui| {
            for m in [Mode::Table, Mode::Tree, Mode::Json] {
                ui.selectable_value(&mut self.mode, m, m.label());
            }
        });
        if loading {
            ui.spinner();
        }
    }

    fn breadcrumb(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            let root = RichText::new(&self.title).strong();
            if self.drill.is_empty() {
                ui.label(root);
            } else if ui.link(root).on_hover_text("Back to the top level (Esc steps out one level)").clicked() {
                self.set_drill(Vec::new());
            }
            let mut cut = None;
            let n = self.drill.len();
            for (i, step) in self.drill.iter().enumerate() {
                ui.label(RichText::new(icon::CARET_RIGHT).color(theme::MUTED));
                let label = match step {
                    Step::Field(f) => f.clone(),
                    Step::Elements { row, field } => format!("{field} [row {}]", row + 1),
                    Step::Cell { row, field } => format!("{field} (row {})", row + 1),
                };
                if i + 1 == n {
                    ui.label(RichText::new(label).strong());
                } else if ui.link(label).clicked() {
                    cut = Some(i + 1);
                }
            }
            if let Some(c) = cut {
                let mut d = self.drill.clone();
                d.truncate(c);
                self.set_drill(d);
            }
        });
    }

    fn click_row(&mut self, i: usize, modifiers: Modifiers) {
        if modifiers.command {
            if !self.selected.remove(&i) {
                self.selected.insert(i);
            }
            self.anchor = Some(i);
        } else if modifiers.shift
            && let Some(a) = self.anchor
        {
            self.selected = (a.min(i)..=a.max(i)).collect();
        } else {
            self.selected = BTreeSet::from([i]);
            self.anchor = Some(i);
        }
    }

    /// Step Into Column (`cell = false`) or Step Into Cell (`cell = true`).
    fn step_into(&mut self, row: usize, col: &str, cell: bool) -> bool {
        let value = self.resolved().get(row).and_then(|r| r.value.as_document()).and_then(|d| d.get(col)).cloned();
        let step = match (value, cell) {
            (Some(Bson::Document(_)), false) => Step::Field(col.to_string()),
            (Some(Bson::Document(_)), true) => Step::Cell { row, field: col.to_string() },
            (Some(Bson::Array(_)), _) => Step::Elements { row, field: col.to_string() },
            _ => return false,
        };
        let mut d = self.drill.clone();
        d.push(step);
        self.set_drill(d);
        true
    }

    /// The direction the view is sorted by `path`, if it is.
    fn sorted_by(&self, path: &str) -> Option<i32> {
        match &self.source {
            Source::Find { spec, .. } => match spec.sort.as_ref()?.get(path)? {
                Bson::Int32(n) => Some(n.signum()),
                Bson::Int64(n) => Some(n.signum() as i32),
                Bson::Double(f) => Some(if *f < 0.0 { -1 } else { 1 }),
                _ => None,
            },
            Source::Static { .. } => self.local_sort.as_ref().filter(|(p, ..)| p == path).map(|(_, d, _)| *d),
        }
    }

    /// Sort by one field, or (`dir = None`) go back to the query's own order.
    /// The server sorts a query; a static result is sorted here. Hidden
    /// columns and the drill level stay as they are.
    fn sort_by(&mut self, path: String, dir: Option<i32>, actions: &mut Vec<ViewAction>) {
        match &mut self.source {
            Source::Find { spec, .. } => {
                spec.sort = dir.map(|d| doc! { path: d });
                actions.push(ViewAction::SortChanged(spec.sort.clone().unwrap_or_default()));
            }
            Source::Static { docs, .. } => {
                self.local_sort = dir.map(|d| {
                    let mut order: Vec<usize> = (0..docs.len()).collect();
                    order.sort_by(|&a, &b| {
                        let o = compare(lookup(&docs[a], &path), lookup(&docs[b], &path));
                        if d < 0 { o.reverse() } else { o }
                    });
                    (path, d, order)
                });
            }
        }
        self.offset = 0;
        self.refresh();
    }

    /// Menu for an empty result: inserting and pasting still make sense.
    fn empty_menu(&self, ui: &mut Ui, actions: &mut Vec<ViewAction>) {
        let Some((conn, ns)) = self.editable() else { return };
        if ui.add(egui::Button::new("Insert Document…").shortcut_text("⌘D")).clicked() {
            actions.push(ViewAction::Insert { conn, ns: ns.clone(), template: None });
            ui.close();
        }
        if ui.add(egui::Button::new("Paste Document(s)…").shortcut_text("⌘V")).clicked() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::RequestPaste);
            ui.close();
        }
    }

    /// Right-click under the rows: the selected row's menu (its selected
    /// cell, if any), or what makes sense with no row.
    fn background_menu(&mut self, ui: &mut Ui, net: &mut Net, rows: &Rows, actions: &mut Vec<ViewAction>) {
        if let Some(i) = self.selected.iter().next().copied()
            && let Some(Row { origin, value, path }) = rows.get(i)
        {
            let col = self.selected_col.clone();
            let cell = col.as_deref().and_then(|c| if c == "(value)" { (!matches!(value, Bson::Document(_))).then_some(value) } else { value.as_document().and_then(|d| d.get(c)) });
            self.cell_menu(ui, net, i, origin, col.as_deref(), &path, cell, actions);
            return;
        }
        self.empty_menu(ui, actions);
        ui.separator();
        if ui.add(egui::Button::new("Refresh View").shortcut_text("⌘R")).clicked() {
            self.fetch(net);
            ui.close();
        }
        let editable = self.editable().map(|(c, n)| (c, n.clone()));
        if ui.add_enabled(editable.is_some(), egui::Button::new(format!("{}  Update Dialog…", icon::PENCIL_LINE)).shortcut_text("⌘U")).clicked()
            && let Some((conn, ns)) = editable.clone()
        {
            actions.push(ViewAction::Bulk { conn, ns, delete: false, selected: Vec::new(), query: self.query(), field: None });
            ui.close();
        }
        if ui.add_enabled(editable.is_some(), egui::Button::new(RichText::new(format!("{}  Delete Dialog…", icon::BROOM)).color(theme::ERROR)).shortcut_text("F8")).clicked()
            && let Some((conn, ns)) = editable.clone()
        {
            actions.push(ViewAction::Bulk { conn, ns, delete: true, selected: Vec::new(), query: self.query(), field: None });
            ui.close();
        }
        ui.separator();
        if ui.add_enabled(editable.is_some(), egui::Button::new(format!("{}  Export Documents…", icon::EXPORT))).clicked()
            && let Source::Find { conn, spec, .. } = &self.source
        {
            actions.push(ViewAction::Export { conn: *conn, spec: spec.clone() });
            ui.close();
        }
    }

    /// The right-click menu for a cell (`col` is `None` in the Tree view).
    #[allow(clippy::too_many_arguments)]
    fn cell_menu(&mut self, ui: &mut Ui, net: &mut Net, row: usize, origin: usize, col: Option<&str>, path_prefix: &str, cell: Option<&Bson>, actions: &mut Vec<ViewAction>) {
        let doc = self.rows.doc(origin).cloned();
        let id = doc.as_ref().and_then(|d| d.get("_id")).cloned();
        let editable = self.editable().map(|(c, n)| (c, n.clone()));
        let field_path = col.map(|c| if c == "(value)" { path_prefix.to_string() } else { join(path_prefix, c) }).filter(|p| !p.is_empty());
        let can_write = editable.is_some() && id.is_some();

        ui.menu_button(format!("{}  Field", icon::TEXT_COLUMNS), |ui| {
            let enabled = can_write && field_path.as_deref().is_some_and(|p| p != "_id");
            if ui.add_enabled(enabled, egui::Button::new("Edit Value…")).clicked()
                && let (Some((conn, ns)), Some(id), Some(path)) = (editable.clone(), id.clone(), field_path.clone())
            {
                actions.push(ViewAction::EditValue { conn, ns, id, path, value: cell.cloned().unwrap_or(Bson::Null) });
                ui.close();
            }
            if ui.add_enabled(enabled, egui::Button::new("Set to null")).clicked()
                && let (Some((conn, ns)), Some(id), Some(path)) = (editable.clone(), id.clone(), field_path.clone())
            {
                actions.push(ViewAction::UpdateOne { conn, ns, id, update: doc! { "$set": { path: Bson::Null } } });
                ui.close();
            }
            if ui.add_enabled(enabled && cell.is_some(), egui::Button::new(RichText::new("Remove Field").color(theme::ERROR))).clicked()
                && let (Some((conn, ns)), Some(id), Some(path)) = (editable.clone(), id.clone(), field_path.clone())
            {
                actions.push(ViewAction::UpdateOne { conn, ns, id, update: doc! { "$unset": { path: "" } } });
                ui.close();
            }
        });
        ui.menu_button(format!("{}  Document", icon::FILE_TEXT), |ui| {
            if ui.add_enabled(editable.is_some(), egui::Button::new("Insert Document…").shortcut_text("⌘D")).clicked()
                && let Some((conn, ns)) = editable.clone()
            {
                actions.push(ViewAction::Insert { conn, ns, template: None });
                ui.close();
            }
            if ui.add_enabled(editable.is_some() && !self.selected.is_empty(), egui::Button::new("Delete Document…").shortcut_text("⇧⌫")).clicked()
                && let Some((conn, ns)) = editable.clone()
            {
                actions.push(ViewAction::Delete { conn, ns, ids: self.selected_ids() });
                ui.close();
            }
            if ui.add_enabled(doc.is_some(), egui::Button::new("View Document (JSON)…").shortcut_text("F3")).clicked()
                && let Some(d) = doc.clone()
            {
                actions.push(ViewAction::View { doc: d });
                ui.close();
            }
            if ui.add_enabled(can_write, egui::Button::new("Edit Document (JSON)…").shortcut_text("⌘J")).clicked()
                && let (Some((conn, ns)), Some(d)) = (editable.clone(), doc.clone())
            {
                actions.push(ViewAction::Edit { conn, ns, doc: d });
                ui.close();
            }
            if ui.add_enabled(editable.is_some() && doc.is_some(), egui::Button::new("Clone Document…")).clicked()
                && let (Some((conn, ns)), Some(d)) = (editable.clone(), doc.clone())
            {
                actions.push(ViewAction::Clone { conn, ns, doc: d });
                ui.close();
            }
        });
        if let Some(c) = col {
            ui.menu_button(format!("{}  Column", icon::COLUMNS), |ui| {
                let sort_path = self.shared_prefix().filter(|_| c != "(value)").map(|p| join(&p, c));
                let can_sort = sort_path.is_some();
                if ui.add_enabled(can_sort, egui::Button::new(format!("{}  Sort Ascending", icon::SORT_ASCENDING))).clicked()
                    && let Some(p) = sort_path.clone()
                {
                    self.sort_by(p, Some(1), actions);
                    ui.close();
                }
                if ui.add_enabled(can_sort, egui::Button::new(format!("{}  Sort Descending", icon::SORT_DESCENDING))).clicked()
                    && let Some(p) = sort_path
                {
                    self.sort_by(p, Some(-1), actions);
                    ui.close();
                }
                ui.separator();
                if ui.button("Hide Column").clicked() {
                    self.hidden.insert(c.to_string());
                    ui.close();
                }
                if ui.add_enabled(!self.hidden.is_empty(), egui::Button::new("Show All Columns")).clicked() {
                    self.hidden.clear();
                    ui.close();
                }
                let key = self.order_key();
                if ui.add_enabled(self.col_order.contains_key(&key), egui::Button::new("Reset Column Order")).on_hover_text("Drag a column header to move it").clicked() {
                    self.col_order.remove(&key);
                    ui.close();
                }
            });
        }
        ui.menu_button(format!("{}  Copy", icon::CLIPBOARD_TEXT), |ui| {
            if ui.add_enabled(cell.is_some(), egui::Button::new("Copy Value")).clicked()
                && let Some(v) = cell
            {
                let text = match v {
                    Bson::String(s) => s.clone(),
                    other => format::value_to_shell_pretty(other, Style::Display),
                };
                ui.ctx().copy_text(text);
                ui.close();
            }
            if ui.add_enabled(field_path.is_some(), egui::Button::new("Copy Field Path")).clicked()
                && let Some(p) = &field_path
            {
                ui.ctx().copy_text(p.clone());
                ui.close();
            }
            if ui.add(egui::Button::new("Copy Document(s)").shortcut_text("⌘C")).clicked() {
                ui.ctx().copy_text(self.copy_text());
                ui.close();
            }
            if ui.add_enabled(id.is_some(), egui::Button::new("Copy _id")).clicked()
                && let Some(i) = &id
            {
                ui.ctx().copy_text(format::to_shell(i, Style::Display));
                ui.close();
            }
        });
        if ui.add_enabled(editable.is_some(), egui::Button::new(format!("{}  Paste Document(s)…", icon::CLIPBOARD)).shortcut_text("⌘V")).clicked() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::RequestPaste);
            ui.close();
        }
        ui.separator();
        let can_step = |v: Option<&Bson>| matches!(v, Some(Bson::Document(_) | Bson::Array(_)));
        if let Some(c) = col.filter(|c| *c != "(value)") {
            if ui.add_enabled(can_step(cell), egui::Button::new(format!("{}  Step Into Column", icon::TABLE)).shortcut_text("↩")).clicked() {
                self.step_into(row, c, false);
                ui.close();
            }
            if ui.add_enabled(can_step(cell), egui::Button::new(format!("{}  Step Into Cell", icon::TABLE)).shortcut_text("⇧↩")).clicked() {
                self.step_into(row, c, true);
                ui.close();
            }
            ui.separator();
        }
        if ui.add(egui::Button::new("Refresh View").shortcut_text("⌘R")).clicked() {
            self.fetch(net);
            ui.close();
        }
        if ui.add_enabled(can_write, egui::Button::new("Refresh Document").shortcut_text("⇧⌘R")).clicked() {
            self.refresh_doc(net, origin);
            ui.close();
        }
        ui.separator();
        if ui.add_enabled(editable.is_some(), egui::Button::new(format!("{}  Update Dialog…", icon::PENCIL_LINE)).shortcut_text("⌘U")).clicked()
            && let Some((conn, ns)) = editable.clone()
        {
            let field = field_path.clone().filter(|p| p != "_id").zip(cell.cloned());
            actions.push(ViewAction::Bulk { conn, ns, delete: false, selected: self.selected_ids(), query: self.query(), field });
            ui.close();
        }
        if ui.add_enabled(editable.is_some(), egui::Button::new(RichText::new(format!("{}  Delete Dialog…", icon::BROOM)).color(theme::ERROR)).shortcut_text("F8")).clicked()
            && let Some((conn, ns)) = editable.clone()
        {
            actions.push(ViewAction::Bulk { conn, ns, delete: true, selected: self.selected_ids(), query: self.query(), field: None });
            ui.close();
        }
        ui.separator();
        if ui.add_enabled(editable.is_some(), egui::Button::new(format!("{}  Export Documents…", icon::EXPORT))).clicked()
            && let Source::Find { conn, spec, .. } = &self.source
        {
            actions.push(ViewAction::Export { conn: *conn, spec: spec.clone() });
            ui.close();
        }
    }

    /// Selected documents as shell text: one document, or an array of them, so
    /// pasting the text back yields the same documents.
    fn copy_text(&self) -> String {
        let docs = self.selected_docs();
        match docs.as_slice() {
            [] => String::new(),
            [one] => format::doc_to_shell(one, Style::Display),
            many => {
                let arr = Bson::Array(many.iter().map(|d| Bson::Document((*d).clone())).collect());
                format::value_to_shell_pretty(&arr, Style::Display)
            }
        }
    }

    fn table(&mut self, ui: &mut Ui, net: &mut Net, actions: &mut Vec<ViewAction>) {
        // Borrow the rows through our own Arc handle, so `self` stays free
        // for the context menu and nothing is cloned per frame.
        let data = self.rows.clone();
        let rows = resolve(&data, &self.drill);
        let all = self.columns(&rows);
        let mut columns = self.ordered(all);
        columns.retain(|c| !self.hidden.contains(c));
        let mut clicked: Option<(usize, String, Modifiers)> = None;
        let mut double: Option<(usize, String)> = None;
        let modifiers = ui.input(|i| i.modifiers);
        let depth = self.drill.len();
        let selected = self.selected.clone();
        let selected_col = self.selected_col.clone();
        // Each column's field path, when it can be sorted, and its sort.
        let prefix = self.shared_prefix();
        let sorts: Vec<_> = columns
            .iter()
            .map(|c| {
                let path = prefix.as_ref().filter(|_| c != "(value)").map(|p| join(p, c));
                let dir = path.as_deref().and_then(|p| self.sorted_by(p));
                (path, dir)
            })
            .collect();
        let mut header_click: Option<(String, Option<i32>)> = None;
        // The space under the last row answers a right-click too. Added
        // before the table, so rows and headers drawn over it come first.
        let background = ui.interact(ui.available_rect_before_wrap(), ui.id().with((self.id, "table-bg", depth)), Sense::click());
        // Header cells as drawn, for dropping a dragged header between them.
        let mut header_rects: Vec<(String, egui::Rect)> = Vec::new();
        let mut drag_started: Option<String> = None;
        let mut drag_stopped = false;
        let widths = self.col_widths.clone();
        // The table keeps widths by position: a new order starts a new
        // table, with each column at the width it had.
        let order_salt = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            columns.hash(&mut h);
            h.finish()
        };

        egui::ScrollArea::horizontal().id_salt((self.id, "hscroll", depth)).auto_shrink([false, false]).show(ui, |ui| {
            let mut table = TableBuilder::new(ui)
                .id_salt((self.id, "table", depth, order_salt))
                .striped(true)
                .resizable(true)
                .sense(Sense::click())
                .cell_layout(Layout::left_to_right(Align::Center))
                .auto_shrink([false, false])
                .min_scrolled_height(0.0);
            for c in &columns {
                let w = widths.get(c).copied().unwrap_or(if c == "_id" { 210.0 } else { 160.0 });
                table = table.column(Column::initial(w).at_least(40.0).clip(true).resizable(true));
            }
            table = table.column(Column::remainder().at_least(20.0));
            table
                .header(24.0, |mut header| {
                    for (c, (path, dir)) in columns.iter().zip(&sorts) {
                        let mut resp = None;
                        let mut rect = egui::Rect::NOTHING;
                        header.col(|ui| {
                            // The whole cell, at the column's width (what
                            // `col` returns is only the part used).
                            rect = ui.max_rect();
                            ui.add(egui::Label::new(RichText::new(c).color(theme::TEXT).strong()).selectable(false));
                            if let Some(d) = dir {
                                ui.label(RichText::new(if *d > 0 { icon::CARET_UP } else { icon::CARET_DOWN }).color(theme::ACCENT));
                            }
                            // Click sorts, drag moves the column. (The column
                            // resize handles are added later, so they win at
                            // the edges.)
                            resp = Some(ui.interact(ui.max_rect(), ui.id().with(("header", c)), Sense::click_and_drag()));
                        });
                        header_rects.push((c.clone(), rect));
                        let Some(resp) = resp else { continue };
                        if resp.drag_started() {
                            drag_started = Some(c.clone());
                        }
                        if resp.drag_stopped() {
                            drag_stopped = true;
                        }
                        let cursor = if resp.dragged() { egui::CursorIcon::Grabbing } else if path.is_some() { egui::CursorIcon::PointingHand } else { egui::CursorIcon::Grab };
                        let resp = resp.on_hover_cursor(cursor);
                        // Click: ascending, then descending, then unsorted.
                        if let Some(p) = path
                            && resp.clicked()
                        {
                            let next = match dir {
                                None => Some(1),
                                Some(d) if *d > 0 => Some(-1),
                                Some(_) => None,
                            };
                            header_click = Some((p.clone(), next));
                        }
                    }
                    header.col(|_| {});
                })
                .body(|body| {
                    body.rows(22.0, rows.len(), |mut row| {
                        let i = row.index();
                        let Some(Row { origin, value, path }) = rows.get(i) else { return };
                        row.set_selected(selected.contains(&i));
                        for c in &columns {
                            let cell = if c == "(value)" {
                                (!matches!(value, Bson::Document(_))).then_some(value)
                            } else {
                                value.as_document().and_then(|d| d.get(c))
                            };
                            let outline = selected.contains(&i) && selected_col.as_deref() == Some(c);
                            let (_, resp) = row.col(|ui| {
                                if let Some(v) = cell {
                                    cell_ui(ui, v);
                                }
                                // The cell's own painter: clipped to the table, so a
                                // row just past the bottom doesn't draw over the footer.
                                if outline {
                                    ui.painter().rect_stroke(ui.max_rect().shrink(1.0), 0.0, egui::Stroke::new(1.0, theme::ACCENT), egui::StrokeKind::Inside);
                                }
                            });
                            if resp.double_clicked() {
                                double = Some((i, c.clone()));
                            } else if resp.clicked() {
                                clicked = Some((i, c.clone(), modifiers));
                            } else if resp.secondary_clicked() && !selected.contains(&i) {
                                clicked = Some((i, c.clone(), Modifiers::NONE));
                            }
                            resp.context_menu(|ui| {
                                ui.set_min_width(230.0);
                                self.cell_menu(ui, net, i, origin, Some(c), &path, cell, actions);
                            });
                        }
                        let (_, resp) = row.col(|_| {});
                        if resp.clicked() {
                            clicked = Some((i, String::new(), modifiers));
                        }
                    });
                });
        });

        if let Some((path, dir)) = header_click {
            self.sort_by(path, dir, actions);
        }
        // Remember widths (by name) so a moved column keeps its own.
        if !ui.is_sizing_pass() {
            for (c, r) in &header_rects {
                self.col_widths.insert(c.clone(), r.width());
            }
        }
        if let Some(c) = drag_started {
            self.dragging_col = Some(c);
        }
        #[cfg(test)]
        {
            self.header_rects = header_rects.clone();
        }
        if let Some(from) = self.dragging_col.clone() {
            // Drop before the first header whose middle is right of the pointer.
            let x = ui.ctx().pointer_latest_pos().map(|p| p.x);
            let before = x.and_then(|x| header_rects.iter().find(|(_, r)| r.center().x > x)).map(|(c, _)| c.clone());
            if drag_stopped || !ui.input(|i| i.pointer.any_down()) {
                self.dragging_col = None;
                self.move_column(&from, before.as_deref());
            } else if let Some(first) = header_rects.first().map(|(_, r)| *r) {
                // Where it would land: a line between two headers.
                let line_x = match &before {
                    Some(b) => header_rects.iter().find(|(c, _)| c == b).map(|(_, r)| r.left()),
                    None => header_rects.last().map(|(_, r)| r.right()),
                };
                if let Some(lx) = line_x {
                    let painter = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new((self.id, "col-drop"))));
                    painter.line_segment([pos2(lx, first.top()), pos2(lx, first.bottom())], egui::Stroke::new(2.0, theme::ACCENT));
                    if let Some(p) = ui.ctx().pointer_latest_pos() {
                        painter.text(p + vec2(12.0, 8.0), Align2::LEFT_TOP, &from, egui::FontId::proportional(13.0), theme::TEXT);
                    }
                }
            }
        }
        background.context_menu(|ui| {
            ui.set_min_width(230.0);
            self.background_menu(ui, net, &rows, actions);
        });
        if let Some((i, col, m)) = clicked {
            self.click_row(i, m);
            self.selected_col = (!col.is_empty()).then_some(col);
        }
        if let Some((i, col)) = double {
            self.selected = BTreeSet::from([i]);
            self.selected_col = Some(col.clone());
            // Containers drill in; other cells open the document for editing.
            if (col == "(value)" || !self.step_into(i, &col, false))
                && let (Some((conn, ns)), Some(doc)) = (self.editable().map(|(c, n)| (c, n.clone())), rows.get(i).and_then(|r| r.value.as_document()).filter(|_| depth == 0))
            {
                actions.push(ViewAction::Edit { conn, ns, doc: doc.clone() });
            }
        }
    }

    fn tree(&mut self, ui: &mut Ui, net: &mut Net, actions: &mut Vec<ViewAction>) {
        let data = self.rows.clone();
        let rows = resolve(&data, &self.drill);
        let mut clicked = None;
        let modifiers = ui.input(|i| i.modifiers);
        let header_rect = ui.available_rect_before_wrap();
        let key_w = (header_rect.width() * 0.35).clamp(160.0, 360.0);
        ui.horizontal(|ui| {
            ui.add_sized([key_w, 20.0], egui::Label::new(RichText::new("Key").strong()));
            ui.label(RichText::new("Value").strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_sized([100.0, 20.0], egui::Label::new(RichText::new("Type").strong()));
            });
        });
        ui.separator();
        // Only rows in view are drawn. Each row's height is remembered once
        // drawn (open nodes are taller), which places the rest.
        let default_h = 20.0 + ui.spacing().item_spacing.y;
        if self.tree_heights.len() != rows.len() {
            self.tree_heights = vec![default_h; rows.len()];
        }
        egui::ScrollArea::vertical().id_salt((self.id, "tree", self.drill.len())).auto_shrink([false, false]).show_viewport(ui, |ui, viewport| {
            let total: f32 = self.tree_heights.iter().sum();
            ui.set_min_height(total);
            let left = ui.max_rect().left();
            let mut y = 0.0;
            let mut i = 0;
            while i < rows.len() && y + self.tree_heights[i] < viewport.min.y {
                y += self.tree_heights[i];
                i += 1;
            }
            ui.add_space(y);
            while i < rows.len() && y < viewport.max.y {
                let Some(Row { origin, value, path }) = rows.get(i) else { break };
                let label = match value.as_document().and_then(|d| d.get("_id")) {
                    Some(id) => format!("({}) {}", self.offset as usize + i + 1, format::cell_text(id)),
                    None => format!("({})", self.offset as usize + i + 1),
                };
                let id = ui.make_persistent_id((self.id, "node", self.offset, i, self.drill.len()));
                let top = ui.cursor().top();
                let r = tree_node(ui, id, &label, value, left, key_w, self.selected.contains(&i));
                if r.clicked() || (r.secondary_clicked() && !self.selected.contains(&i)) {
                    clicked = Some(i);
                }
                r.context_menu(|ui| {
                    ui.set_min_width(230.0);
                    self.cell_menu(ui, net, i, origin, None, &path, None, actions);
                });
                let h = (ui.cursor().top() - top).max(1.0);
                self.tree_heights[i] = h;
                y += h;
                i += 1;
            }
        });
        if let Some(i) = clicked {
            self.click_row(i, modifiers);
        }
    }

    fn json(&mut self, ui: &mut Ui) {
        // The text is split into lines once and only the lines in view are
        // laid out, so a 50,000 document page scrolls as smoothly as 50.
        if self.json_lines.is_none() {
            if let Some(rx) = &self.json_building {
                match rx.try_recv() {
                    Ok(text) => {
                        self.json_lines = Some(Arc::new(text));
                        self.json_building = None;
                        // Matches are offsets into the old text: drop them
                        // before anything draws or scrolls with them.
                        if let Some(f) = &mut self.find {
                            f.searched = None;
                            f.running = None;
                            f.matches.clear();
                            f.current = 0;
                            f.scroll_to = false;
                        }
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {}
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => self.json_building = None,
                }
            } else {
                // Formatting tens of thousands of documents takes a while:
                // do it off the UI thread and show a spinner meanwhile.
                let (tx, rx) = std::sync::mpsc::channel();
                let (rows, offset, ctx) = (self.rows.raw().clone(), self.offset as usize, ui.ctx().clone());
                std::thread::spawn(move || {
                    let _ = tx.send(json_lines(&rows, offset));
                    ctx.request_repaint();
                });
                self.json_building = Some(rx);
            }
        }
        let Some(lines) = &self.json_lines else {
            ui.centered_and_justified(|ui| ui.spinner());
            return;
        };
        let font = theme::mono(13.0);
        let row_h = ui.fonts_mut(|f| f.row_height(&font)).round();
        // show_rows sizes rows as row_h plus this spacing, so it must be zero
        // here, not just inside, or the rows drawn fall short of the bottom.
        ui.spacing_mut().item_spacing.y = 0.0;
        let mut area = egui::ScrollArea::both().id_salt((self.id, "json")).auto_shrink([false, false]);
        if let Some(f) = &mut self.find
            && std::mem::take(&mut f.scroll_to)
            && let Some(&(line, at, _)) = f.matches.get(f.current)
        {
            area = area.vertical_scroll_offset((line as f32 * row_h - ui.available_height() / 3.0).max(0.0));
            let prefix = if line < lines.len() { lines.line(line).get(..at).unwrap_or_default() } else { "" };
            let x = ui.fonts_mut(|fo| fo.layout_no_wrap(prefix.to_string(), font.clone(), theme::TEXT).size().x);
            let w = ui.available_width();
            area = area.horizontal_scroll_offset(if x > w - 120.0 { x - w / 2.0 } else { 0.0 });
        }
        let find = self.find.as_ref().filter(|f| !f.matches.is_empty());
        let all = self.json_all;
        let out = area.show_rows(ui, row_h, lines.len(), |ui, range| {
            let top = range.start;
            for i in range {
                let line = lines.line(i);
                // A blank line still needs a glyph to get a height.
                let text = if line.is_empty() { " " } else { line };
                let mut job = highlight::layout_job(text, font.clone(), f32::INFINITY);
                // A fixed line height, so every row is exactly row_h even
                // when a fallback font (Thai, emoji) is taller.
                for s in &mut job.sections {
                    s.format.line_height = Some(row_h);
                }
                if all {
                    highlight::mark(&mut job, &[(0..text.len(), theme::SELECTION)]);
                }
                if let Some(f) = find {
                    let first = f.matches.partition_point(|m| m.0 < i);
                    let marks: Vec<_> = f.matches[first..]
                        .iter()
                        .enumerate()
                        .take_while(|(_, m)| m.0 == i)
                        .map(|(k, m)| (m.1..m.2, if first + k == f.current { CURRENT_MATCH } else { MATCH }))
                        .collect();
                    highlight::mark(&mut job, &marks);
                }
                ui.add(egui::Label::new(job).extend().selectable(true));
            }
            (top, ui.min_rect().bottom(), ui.clip_rect().bottom())
        });
        let (top, _drawn, _visible) = out.inner;
        self.json_top = top;
        #[cfg(test)]
        {
            self.json_fill = Some((_drawn, _visible));
        }
    }

    #[cfg(test)]
    pub fn find_open(&self) -> bool {
        self.find.is_some() && self.mode == Mode::Json
    }

    fn find_id(&self) -> egui::Id {
        egui::Id::new((self.id, "find"))
    }

    /// The find bar over the JSON view: the text, match case, "n of m", and
    /// previous/next. ↩ and ⇧↩ step through matches, Esc closes it.
    fn find_bar(&mut self, ui: &mut Ui) {
        let id = self.find_id();
        let Some(f) = &mut self.find else { return };
        let (mut step, mut close) = (0i64, false);
        ui.horizontal(|ui| {
            ui.label(RichText::new(icon::MAGNIFYING_GLASS).color(theme::MUTED));
            let r = ui.add(egui::TextEdit::singleline(&mut f.text).id(id).hint_text("Find in these documents").desired_width(280.0));
            if std::mem::take(&mut f.focus) {
                r.request_focus();
            }
            if r.lost_focus() {
                if ui.input(|i| i.key_pressed(Key::Enter)) {
                    step = if ui.input(|i| i.modifiers.shift) { -1 } else { 1 };
                    r.request_focus();
                } else if ui.input(|i| i.key_pressed(Key::Escape)) {
                    close = true;
                }
            }
            if ui.selectable_label(f.case, "Aa").on_hover_text("Match case").clicked() {
                f.case = !f.case;
            }
            let any = !f.matches.is_empty();
            if ui.add_enabled(any, egui::Button::new(icon::CARET_UP)).on_hover_text("Previous match (⇧↩)").clicked() {
                step = -1;
            }
            if ui.add_enabled(any, egui::Button::new(icon::CARET_DOWN)).on_hover_text("Next match (↩)").clicked() {
                step = 1;
            }
            if f.text.is_empty() {
            } else if self.json_lines.is_none() || f.running.is_some() {
                ui.weak("Searching…");
            } else if !any {
                ui.colored_label(theme::ERROR, "No matches");
            } else {
                let more = if f.matches.len() >= MAX_MATCHES { "+" } else { "" };
                ui.weak(format!("{} of {}{more}", thousands(f.current as u64 + 1), thousands(f.matches.len() as u64)));
            }
            if ui.button(icon::X).on_hover_text("Close (Esc)").clicked() {
                close = true;
            }
        });
        if close {
            self.find = None;
            return;
        }
        let key = (f.text.clone(), f.case);
        if let Some(lines) = &self.json_lines
            && f.searched.as_ref() != Some(&key)
            && f.running.as_ref().map(|r| &r.key) != Some(&key)
        {
            f.running = Some(Search::start(lines.clone(), key, ui.ctx().clone()));
        }
        if let Some(r) = &f.running
            && let Ok(matches) = r.rx.try_recv()
        {
            let key = f.running.take().map(|r| r.key.clone()).unwrap_or_default();
            f.matches = matches;
            f.current = f.matches.iter().position(|m| m.0 >= self.json_top).unwrap_or(0);
            f.scroll_to = !f.matches.is_empty();
            f.searched = Some(key);
        }
        if step != 0 && !f.matches.is_empty() {
            let n = f.matches.len() as i64;
            f.current = (f.current as i64 + step).rem_euclid(n) as usize;
            f.scroll_to = true;
        }
    }

    fn keyboard(&mut self, ui: &mut Ui, net: &mut Net, actions: &mut Vec<ViewAction>) {
        if ui.memory(|m| m.focused().is_some()) {
            return;
        }
        let editable = self.editable().map(|(c, n)| (c, n.clone()));
        // ⌘C and ⌘V arrive as Copy/Paste events, not as key presses.
        // ⌥⌘C belongs to the tree (Copy Name).
        let alt = ui.input(|i| i.modifiers.alt);
        for e in ui.input(|i| i.events.clone()) {
            match e {
                Event::Copy if self.json_all && !alt => {
                    if let Some(lines) = &self.json_lines {
                        ui.ctx().copy_text(lines.text.clone());
                    }
                }
                Event::Copy if !self.selected.is_empty() && !alt => ui.ctx().copy_text(self.copy_text()),
                Event::Paste(text) => {
                    if let Some((conn, ns)) = editable.clone() {
                        actions.push(ViewAction::Paste { conn, ns, text });
                    }
                }
                _ => {}
            }
        }
        let key = |ui: &mut Ui, m: Modifiers, k: Key| ui.input_mut(|i| i.consume_key(m, k));
        if key(ui, Modifiers::COMMAND | Modifiers::SHIFT, Key::R) {
            if let [o] = self.selected_origins().as_slice() {
                self.refresh_doc(net, *o);
            }
        } else if key(ui, Modifiers::COMMAND, Key::R) {
            self.fetch(net);
        }
        if key(ui, Modifiers::COMMAND, Key::A) {
            if self.mode == Mode::Json {
                // Lines off screen aren't laid out, so egui can't select them:
                // select-all is ours, and a drag selection would fight it.
                self.json_all = true;
                ui.ctx().with_plugin(|s: &mut egui::text_selection::LabelSelectionState| s.clear_selection());
            } else {
                self.selected = (0..self.resolved().len()).collect();
                self.anchor = Some(0);
            }
        }
        if self.json_all && ui.input(|i| i.key_pressed(Key::Escape)) {
            self.json_all = false;
        } else if key(ui, Modifiers::NONE, Key::Escape) && !self.drill.is_empty() {
            let mut d = self.drill.clone();
            d.pop();
            self.set_drill(d);
        }
        let Some((conn, ns)) = editable else {
            if key(ui, Modifiers::NONE, Key::F3)
                && let [d] = self.selected_docs().as_slice()
            {
                actions.push(ViewAction::View { doc: (*d).clone() });
            }
            return;
        };
        if key(ui, Modifiers::COMMAND, Key::D) {
            actions.push(ViewAction::Insert { conn, ns: ns.clone(), template: None });
        }
        if key(ui, Modifiers::COMMAND, Key::U) {
            actions.push(ViewAction::Bulk { conn, ns: ns.clone(), delete: false, selected: self.selected_ids(), query: self.query(), field: self.selected_field() });
        }
        if key(ui, Modifiers::NONE, Key::F8) {
            actions.push(ViewAction::Bulk { conn, ns: ns.clone(), delete: true, selected: self.selected_ids(), query: self.query(), field: None });
        }
        if self.selected.is_empty() {
            return;
        }
        if key(ui, Modifiers::SHIFT, Key::Backspace) || key(ui, Modifiers::NONE, Key::Delete) {
            let ids = self.selected_ids();
            if !ids.is_empty() {
                actions.push(ViewAction::Delete { conn, ns: ns.clone(), ids });
            }
        }
        let single = self.single_selected_doc();
        if key(ui, Modifiers::NONE, Key::F3)
            && let Some(d) = single.clone()
        {
            actions.push(ViewAction::View { doc: d });
        }
        if key(ui, Modifiers::COMMAND, Key::J)
            && let Some(d) = single
        {
            actions.push(ViewAction::Edit { conn, ns: ns.clone(), doc: d });
        }
        let row = self.selected.iter().next().copied();
        if let (Some(row), Some(col)) = (row, self.selected_col.clone()) {
            if key(ui, Modifiers::SHIFT, Key::Enter) {
                self.step_into(row, &col, true);
            } else if key(ui, Modifiers::NONE, Key::Enter) {
                self.step_into(row, &col, false);
            }
        }
    }

    fn status_bar(&mut self, ui: &mut Ui, net: &mut Net) {
        ui.separator();
        ui.horizontal(|ui| {
            match self.selected.len() {
                0 => ui.weak("0 documents selected"),
                1 => ui.label("1 document selected"),
                n => ui.label(format!("{n} documents selected")),
            };
            if !self.hidden.is_empty() {
                ui.separator();
                if ui.link(format!("{} hidden column{} — show", self.hidden.len(), if self.hidden.len() == 1 { "" } else { "s" })).clicked() {
                    self.hidden.clear();
                }
            }
            if let Source::Static { truncated: true, .. } = &self.source {
                ui.colored_label(theme::WARN, format!("{} showing the first {} documents", icon::WARNING, self.total.unwrap_or(0)));
            }
            // What this view is waiting for, once it has taken more than a
            // moment: the query, a count or a document refresh.
            let now = std::time::Instant::now();
            let waiting = [self.loading, self.counting, self.refreshing]
                .into_iter()
                .flatten()
                .filter_map(|id| net.pending.get(&id).map(|s| (id, s.what.clone(), s.started)))
                .min_by_key(|(_, _, t)| *t);
            let shown = waiting.clone().filter(|(_, _, t)| now.saturating_duration_since(*t) >= theme::SHOW_BUSY_AFTER);
            match (&shown, &waiting) {
                (Some(_), _) => ui.ctx().request_repaint_after(std::time::Duration::from_millis(33)),
                (None, Some((_, _, t))) => ui.ctx().request_repaint_after((*t + theme::SHOW_BUSY_AFTER).saturating_duration_since(now)),
                _ => {}
            }
            let mut discard = None;
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let Some(ms) = self.elapsed_ms {
                    ui.label(format!("{}  {}", icon::TIMER, elapsed_text(ms)));
                    ui.separator();
                }
                match (&self.source, self.total, self.counting) {
                    (_, _, Some(_)) if shown.is_none() => {
                        ui.spinner();
                    }
                    (_, _, Some(_)) => {}
                    (Source::Find { conn, spec, .. }, total, None) => {
                        let label = match total {
                            Some(t) => format!("{}  {} documents", icon::HASH, thousands(t)),
                            None => format!("{}  Count Documents", icon::HASH),
                        };
                        if ui.button(label).clicked() {
                            let cmd = Command::Count { ns: spec.ns.clone(), filter: spec.filter.clone() };
                            self.counting = Some(net.send(Origin::View(self.id, ViewReq::Count), Some(*conn), cmd));
                        }
                    }
                    (Source::Static { .. }, Some(t), None) => {
                        ui.label(format!("{}  {} documents", icon::HASH, thousands(t)));
                    }
                    _ => {}
                }
                // Right to left: drawn as ⊗ ▬▬ Running find on db.c… 4s.
                if let Some((id, what, started)) = &shown {
                    ui.separator();
                    ui.label(RichText::new(theme::elapsed(now.saturating_duration_since(*started))).color(theme::MUTED));
                    ui.label(format!("{}…", what.as_deref().unwrap_or("Working")));
                    theme::busy_bar(ui, 80.0);
                    if theme::discard_button(ui) {
                        discard = Some(*id);
                    }
                }
            });
            if let Some(id) = discard {
                net.send(Origin::None, None, Command::Cancel(id));
                self.on_cancelled(id);
            }
        });
    }
}

const MATCH: Color32 = Color32::from_rgba_premultiplied(0x5a, 0x4a, 0x14, 0xa0);
const CURRENT_MATCH: Color32 = Color32::from_rgb(0xb0, 0x7a, 0x10);

/// Where `needle` occurs in the JSON lines, as (line, start, end), leaving
/// out the `// n` document number lines. Stops at MAX_MATCHES, or with
/// nothing when `cancel` is set.
fn find_matches(lines: &JsonText, needle: &str, case: bool, cancel: &std::sync::atomic::AtomicBool) -> Vec<(usize, usize, usize)> {
    let mut out = Vec::new();
    if needle.is_empty() {
        return out;
    }
    let lower = needle.to_lowercase();
    for i in 0..lines.len() {
        let line = lines.line(i);
        if i % 4096 == 0 && cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Vec::new();
        }
        if line.starts_with("// ") {
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
            // Lowercasing can change a character's byte length (İ, ẞ), so
            // map offsets in the lowercased line back to the line's own
            // character boundaries, and drop a match that starts or ends
            // inside one character's lowercase form.
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

/// The JSON view's lines: each document in shell syntax, numbered. Each
/// document is parsed, formatted and dropped in turn.
fn json_lines(rows: &[RawDocumentBuf], offset: usize) -> JsonText {
    let mut out = JsonText::default();
    for (i, raw) in rows.iter().enumerate() {
        if i > 0 {
            out.push("");
        }
        out.push(&format!("// {}", offset + i + 1));
        let text = match Document::try_from(raw.as_ref()) {
            Ok(d) => format::value_to_shell_pretty(&Bson::Document(d), Style::Display),
            Err(e) => format!("(unreadable BSON: {e})"),
        };
        for line in text.lines() {
            out.push(line);
        }
    }
    out.text.shrink_to_fit();
    out.starts.shrink_to_fit();
    out
}

fn cell_ui(ui: &mut Ui, v: &Bson) {
    let kind = format::kind(v);
    ui.label(RichText::new(theme::type_icon(kind)).color(theme::MUTED).size(12.0));
    let r = ui.add(egui::Label::new(RichText::new(format::cell_text(v)).color(theme::type_color(kind))).truncate().selectable(false));
    if let Bson::ObjectId(oid) = v {
        r.on_hover_text(format::object_id_details(oid));
    }
}

/// One tree row (and its children when open). Returns the row's response.
fn tree_node(ui: &mut Ui, id: egui::Id, key: &str, v: &Bson, left: f32, key_w: f32, selected: bool) -> egui::Response {
    let children: Vec<(String, &Bson)> = match v {
        Bson::Document(d) => d.iter().map(|(k, x)| (k.clone(), x)).collect(),
        Bson::Array(a) => a.iter().enumerate().map(|(i, x)| (format!("[{i}]"), x)).collect(),
        _ => Vec::new(),
    };
    let container = matches!(v, Bson::Document(_) | Bson::Array(_));
    let mut state = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 20.0), Sense::click());
    if selected {
        ui.painter().rect_filled(rect, 0.0, theme::SELECTION);
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, 0.0, Color32::from_white_alpha(8));
    }
    let painter = ui.painter_at(rect);
    let text_y = rect.center().y;
    if container {
        let arrow = if state.is_open() { icon::CARET_DOWN } else { icon::CARET_RIGHT };
        painter.text(pos2(rect.left() + 2.0, text_y), Align2::LEFT_CENTER, arrow, egui::FontId::proportional(12.0), theme::MUTED);
        if resp.clicked() || resp.double_clicked() {
            state.toggle(ui);
        }
    }
    let kind = format::kind(v);
    painter.text(pos2(rect.left() + 18.0, text_y), Align2::LEFT_CENTER, format!("{}  {key}", theme::type_icon(kind)), egui::FontId::proportional(13.0), theme::TEXT);
    let value_x = left + key_w;
    let type_x = rect.right() - 100.0;
    let value_clip = egui::Rect::from_x_y_ranges(value_x..=type_x - 8.0, rect.y_range());
    ui.painter_at(value_clip).text(pos2(value_x, text_y), Align2::LEFT_CENTER, format::cell_text(v), theme::mono(12.5), theme::type_color(kind));
    painter.text(pos2(type_x, text_y), Align2::LEFT_CENTER, kind.label(), egui::FontId::proportional(12.0), theme::MUTED);
    state.store(ui.ctx());
    let resp = match v {
        Bson::ObjectId(oid) => resp.on_hover_text(format::object_id_details(oid)),
        _ => resp,
    };
    if container && state.is_open() {
        ui.indent(id.with("children"), |ui| {
            for (i, (k, child)) in children.iter().enumerate() {
                tree_node(ui, id.with(i), k, child, left, key_w, false);
            }
        });
    }
    resp
}

/// A typed page size, capped at the maximum; anything else keeps `current`.
/// The row each toolbar group (paging, document buttons, view mode) goes
/// on: the first of these layouts that fits.
///   one row:    paging | buttons      mode
///   two rows:   paging                mode
///               buttons
///   two rows:   paging
///               buttons               mode
///   three rows, one group each.
fn toolbar_rows([paging, buttons, mode]: [f32; 3], gap: f32, available: f32) -> [usize; 3] {
    let fits = |w: f32| w <= available;
    if fits(paging + gap + buttons + gap + mode) {
        [0, 0, 0]
    } else if fits(paging + gap + mode) && fits(buttons) {
        [0, 1, 0]
    } else if fits(buttons + gap + mode) {
        [0, 1, 1]
    } else {
        [0, 1, 2]
    }
}

fn parse_page_size(text: &str, current: i64) -> i64 {
    match text.trim().parse::<i64>() {
        Ok(n) if n > 0 => n.min(MAX_PAGE_SIZE),
        _ => current,
    }
}

fn elapsed_text(ms: u64) -> String {
    format!("{:02}:{:02}:{:02}.{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

/// The value at a dotted path; a number steps into an array.
fn lookup<'a>(doc: &'a Document, path: &str) -> Option<&'a Bson> {
    let mut parts = path.split('.');
    let mut v = doc.get(parts.next()?)?;
    for p in parts {
        v = match v {
            Bson::Document(d) => d.get(p)?,
            Bson::Array(a) => a.get(p.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(v)
}

/// MongoDB's sort order across types, with a missing field sorting as null.
/// Arrays compare element by element (the server uses their smallest or
/// largest element), which is close enough for results already in hand.
fn compare(a: Option<&Bson>, b: Option<&Bson>) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    fn rank(v: Option<&Bson>) -> u8 {
        match v {
            Some(Bson::MinKey) => 0,
            None | Some(Bson::Null | Bson::Undefined) => 1,
            Some(Bson::Int32(_) | Bson::Int64(_) | Bson::Double(_) | Bson::Decimal128(_)) => 2,
            Some(Bson::String(_) | Bson::Symbol(_)) => 3,
            Some(Bson::Document(_)) => 4,
            Some(Bson::Array(_)) => 5,
            Some(Bson::Binary(_)) => 6,
            Some(Bson::ObjectId(_)) => 7,
            Some(Bson::Boolean(_)) => 8,
            Some(Bson::DateTime(_)) => 9,
            Some(Bson::Timestamp(_)) => 10,
            Some(Bson::RegularExpression(_)) => 11,
            Some(Bson::MaxKey) => 13,
            Some(_) => 12,
        }
    }
    fn number(v: &Bson) -> f64 {
        match v {
            Bson::Int32(n) => *n as f64,
            Bson::Int64(n) => *n as f64,
            Bson::Double(f) => *f,
            Bson::Decimal128(d) => d.to_string().parse().unwrap_or(f64::NAN),
            _ => f64::NAN,
        }
    }
    fn first_difference(pairs: impl Iterator<Item = Ordering>) -> Option<Ordering> {
        pairs.into_iter().find(|o| o.is_ne())
    }
    let (Some(x), Some(y)) = (a, b) else { return rank(a).cmp(&rank(b)) };
    if rank(a) != rank(b) {
        return rank(a).cmp(&rank(b));
    }
    match (x, y) {
        (Bson::String(x) | Bson::Symbol(x), Bson::String(y) | Bson::Symbol(y)) => x.cmp(y),
        (Bson::Document(x), Bson::Document(y)) => first_difference(x.iter().zip(y).map(|((kx, vx), (ky, vy))| compare(Some(vx), Some(vy)).then_with(|| kx.cmp(ky))))
            .unwrap_or(x.len().cmp(&y.len())),
        (Bson::Array(x), Bson::Array(y)) => first_difference(x.iter().zip(y).map(|(p, q)| compare(Some(p), Some(q)))).unwrap_or(x.len().cmp(&y.len())),
        (Bson::Binary(x), Bson::Binary(y)) => x.bytes.len().cmp(&y.bytes.len()).then_with(|| x.bytes.cmp(&y.bytes)),
        (Bson::ObjectId(x), Bson::ObjectId(y)) => x.bytes().cmp(&y.bytes()),
        (Bson::Boolean(x), Bson::Boolean(y)) => x.cmp(y),
        (Bson::DateTime(x), Bson::DateTime(y)) => x.timestamp_millis().cmp(&y.timestamp_millis()),
        (Bson::Timestamp(x), Bson::Timestamp(y)) => (x.time, x.increment).cmp(&(y.time, y.increment)),
        _ if rank(a) == 2 => number(x).total_cmp(&number(y)),
        _ => Ordering::Equal,
    }
}

pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(docs: Vec<Document>) -> ResultView {
        let mut v = ResultView::new(1, "t", Source::Static { docs: docs.clone(), truncated: false });
        v.rows = Docs::from_docs(&docs);
        v
    }

    fn paths(v: &ResultView) -> Vec<(usize, String)> {
        let rows = v.resolved();
        (0..rows.len()).filter_map(|i| rows.get(i)).map(|r| (r.origin, r.path)).collect()
    }

    #[test]
    fn drilling_tracks_paths() {
        let mut v = view(vec![
            doc! { "_id": 1, "cfg": { "a": 1, "deep": { "x": 1 } }, "tags": ["x", "y"] },
            doc! { "_id": 2, "cfg": { "a": 2 }, "tags": [] },
        ]);
        v.drill = vec![Step::Field("cfg".into())];
        assert_eq!(paths(&v), [(0, "cfg".to_string()), (1, "cfg".to_string())]);
        assert_eq!(v.shared_prefix().as_deref(), Some("cfg"));
        v.drill = vec![Step::Elements { row: 0, field: "tags".into() }];
        assert_eq!(paths(&v), [(0, "tags.0".to_string()), (0, "tags.1".to_string())]);
        assert_eq!(v.shared_prefix(), None);
        v.selected = BTreeSet::from([0, 1]);
        assert_eq!(v.selected_docs().len(), 1);
        v.drill = vec![Step::Field("cfg".into()), Step::Cell { row: 0, field: "deep".into() }];
        assert_eq!(paths(&v), [(0, "cfg.deep".to_string())]);
    }

    #[test]
    fn step_into_picks_the_right_step() {
        let mut v = view(vec![doc! { "o": { "a": 1 }, "arr": [1, 2], "s": "x" }]);
        assert!(!v.step_into(0, "s", false));
        assert!(v.step_into(0, "arr", false));
        assert_eq!(v.drill, [Step::Elements { row: 0, field: "arr".into() }]);
        v.set_drill(Vec::new());
        assert!(v.step_into(0, "o", true));
        assert_eq!(v.drill, [Step::Cell { row: 0, field: "o".into() }]);
    }

    #[test]
    fn copying_many_documents_pastes_back() {
        let mut v = view(vec![doc! { "_id": 1, "a": "x" }, doc! { "_id": 2 }]);
        v.selected = BTreeSet::from([0, 1]);
        let back = rongo_shell::parse_documents_text(&v.copy_text()).unwrap();
        assert_eq!(back, vec![doc! { "_id": 1, "a": "x" }, doc! { "_id": 2 }]);
    }

    /// ⌘V arrives as a Paste event; an editable view turns it into a Paste
    /// action for the app to confirm. A read-only view ignores it.
    #[test]
    fn paste_event_becomes_a_paste_action() {
        use std::collections::HashMap;
        use std::sync::Arc;
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let core = rongo_core::CoreHandle::start(rt.handle().clone(), Arc::new(|| {}));
        let (mut pending, mut next) = (HashMap::new(), 0);
        let conn = ConnId(uuid::Uuid::nil());
        let ns = Namespace::new("db", "c");
        let mut editable = ResultView::new(1, "c", Source::Find { conn, spec: FindSpec::all(ns.clone(), 50), max: None });
        let mut read_only = view(vec![doc! { "a": 1 }]);
        let ctx = egui::Context::default();
        for (v, expect) in [(&mut editable, true), (&mut read_only, false)] {
            let input = egui::RawInput { events: vec![Event::Paste("{ a: 1 }".into())], ..Default::default() };
            let mut actions = Vec::new();
            let mut out = ctx.run_ui(input, |ui| {
                let mut net = Net { core: &core, pending: &mut pending, next_view_id: &mut next, keys_enabled: true };
                // Collected from every pass, as the app does: input arrives in
                // the first, and a pass can be redrawn (the toolbar re-measuring).
                actions.extend(v.ui(ui, &mut net));
            });
            out.textures_delta.clear();
            let pasted = actions.iter().any(|(_, a)| matches!(a, ViewAction::Paste { text, ns: n, .. } if text == "{ a: 1 }" && *n == ns));
            assert_eq!(pasted, expect);
        }
    }

    #[test]
    fn find_matches_in_json_lines() {
        let lines = JsonText::from_lines(&["// 1", "{", r#"    "brand" : "DGM","#, r#"    "group" : "dgm-DGM","#, r#"    "name" : "ไทย DGM""#, "}"]);
        let go = std::sync::atomic::AtomicBool::new(false);
        assert_eq!(find_matches(&lines, "dgm", false, &go), [(2, 15, 18), (3, 15, 18), (3, 19, 22), (4, 24, 27)]);
        assert_eq!(find_matches(&lines, "DGM", true, &go), [(2, 15, 18), (3, 19, 22), (4, 24, 27)]);
        // The document number lines are not searched.
        assert_eq!(find_matches(&lines, "1", false, &go), []);
        assert_eq!(find_matches(&lines, "", false, &go), []);
        assert_eq!(find_matches(&lines, "ไทย", false, &go), [(4, 14, 23)]);
        // İ lowercases to 3 bytes and ẞ to 2: the line keeps its length but
        // offsets shift. Matches still land on the line's own characters.
        let tricky = JsonText::from_lines(&["ẞİx İ ab"]);
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

    /// A page request discarded from the status bar stops the spinner and
    /// keeps the rows already on screen.
    #[test]
    fn cancelled_page_keeps_the_rows() {
        let raw = |n: i32| (0..n).map(|i| RawDocumentBuf::try_from(&doc! { "_id": i }).unwrap()).collect::<Vec<_>>();
        let mut v = ResultView::new(1, "c", Source::Find { conn: ConnId(uuid::Uuid::nil()), spec: FindSpec::all(Namespace::new("db", "c"), 50), max: None });
        v.needs_fetch = false;
        v.loading = Some(1);
        v.on_page(1, raw(5), false, 1);
        v.loading = Some(2);
        v.counting = Some(3);
        v.on_cancelled(2);
        assert_eq!((v.loading, v.rows.len(), v.error.as_deref()), (None, 5, None));
        assert_eq!(v.counting, Some(3), "only the cancelled request");
        v.on_cancelled(3);
        assert_eq!(v.counting, None);
    }

    /// Last page works before Count Documents: it counts first, then
    /// jumps. Paging elsewhere before the count returns cancels the jump.
    #[test]
    fn last_page_counts_first() {
        use std::collections::HashMap;
        use std::sync::Arc;
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let core = rongo_core::CoreHandle::start(rt.handle().clone(), Arc::new(|| {}));
        let (mut pending, mut next) = (HashMap::new(), 0);
        let conn = ConnId(uuid::Uuid::nil());
        let raw = |n: i32| (0..n).map(|i| RawDocumentBuf::try_from(&doc! { "_id": i }).unwrap()).collect::<Vec<_>>();
        let mut v = ResultView::new(1, "c", Source::Find { conn, spec: FindSpec::all(Namespace::new("db", "c"), 50), max: None });
        v.page_size = 50;
        v.needs_fetch = false;
        v.loading = Some(100);
        v.on_page(100, raw(50), true, 1);
        assert_eq!(v.total, None);
        assert!(v.can_go_last(), "enabled before counting");

        let mut net = Net { core: &core, pending: &mut pending, next_view_id: &mut next, keys_enabled: true };
        v.go_last(&mut net);
        let req = v.counting.expect("pressing Last starts a count");
        assert!(!v.can_go_last(), "disabled while it counts");
        assert_eq!(v.offset, 0);
        v.on_count(req, 1234);
        assert_eq!((v.offset, v.needs_fetch), (1200, true), "jumped to the page holding document 1234");

        // Counted already: Last jumps straight there.
        v.offset = 0;
        v.needs_fetch = false;
        v.loading = None;
        v.go_last(&mut net);
        assert_eq!(v.offset, 1200);
        assert!(v.counting.is_none(), "no second count");

        // Next page pressed while the count runs: the count does not move it.
        v.set_spec(FindSpec::all(Namespace::new("db", "c"), 50));
        v.loading = None;
        v.has_more = true;
        v.go_last(&mut net);
        let req = v.counting.unwrap();
        v.offset = 50;
        v.fetch(&mut net);
        v.loading = None;
        v.on_count(req, 1234);
        assert_eq!(v.offset, 50, "the later Next page wins");
        assert_eq!(v.total, Some(1234));
    }

    /// Replies that belong to an older state of the view are not applied
    /// to the new one: rows clicked while the next page loaded, a count for
    /// the previous query, a Refresh Document for the previous page. A new
    /// query starts at once, cancelling the one still loading.
    #[test]
    fn stale_replies_and_selections_do_not_carry_over() {
        use std::collections::HashMap;
        use std::sync::Arc;
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let core = rongo_core::CoreHandle::start(rt.handle().clone(), Arc::new(|| {}));
        let (mut pending, mut next) = (HashMap::new(), 0);
        let ctx = egui::Context::default();
        let conn = ConnId(uuid::Uuid::nil());
        let ns = Namespace::new("db", "c");
        let raw = |n: i32| (0..n).map(|i| RawDocumentBuf::try_from(&doc! { "_id": i }).unwrap()).collect::<Vec<_>>();
        let mut v = ResultView::new(1, "c", Source::Find { conn, spec: FindSpec::all(ns.clone(), 50), max: None });
        v.needs_fetch = false;
        v.loading = Some(100);
        v.on_page(100, raw(5), false, 1);

        // Next page requested; a row of the old page is clicked meanwhile.
        v.loading = Some(101);
        v.click_row(2, Modifiers::NONE);
        v.on_page(101, raw(5), false, 1);
        assert!(v.selected.is_empty(), "the click was on the old page");

        // Count Documents, then a new query before the count returns.
        v.counting = Some(102);
        v.set_spec(FindSpec { filter: doc! { "x": 1 }, ..FindSpec::all(ns.clone(), 50) });
        v.on_count(102, 999);
        assert_eq!(v.total, None, "the old query's count");

        // The new query starts even though a page is still loading.
        v.loading = Some(103);
        let mut frame = |v: &mut ResultView| {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                let mut net = Net { core: &core, pending: &mut pending, next_view_id: &mut next, keys_enabled: true };
                v.ui(ui, &mut net);
            });
            out.textures_delta.clear();
        };
        frame(&mut v);
        assert!(v.loading.is_some_and(|l| l != 103), "a new request replaced the slow one");

        // Refresh Document on this page, then the page changes, then the
        // reply (document gone) arrives: it must not delete row 0 of the new page.
        let id = v.loading.unwrap();
        v.on_page(id, raw(5), false, 1);
        v.refreshing = Some(200);
        v.refresh();
        frame(&mut v);
        let id = v.loading.unwrap();
        v.on_page(id, raw(5), false, 1);
        v.on_doc(200, 0, Vec::new());
        assert_eq!(v.rows.len(), 5);
    }

    #[test]
    fn typed_page_sizes() {
        assert_eq!(parse_page_size("75", 50), 75);
        assert_eq!(parse_page_size("0", 50), 50);
        assert_eq!(parse_page_size("", 50), 50);
        assert_eq!(parse_page_size("999999", 50), MAX_PAGE_SIZE);
    }

    #[test]
    fn formats_numbers_and_time() {
        assert_eq!(thousands(25000), "25,000");
        assert_eq!(thousands(999), "999");
        assert_eq!(elapsed_text(35), "00:00:00.035");
        assert_eq!(elapsed_text(3_723_004), "01:02:03.004");
    }
}

#[cfg(test)]
mod perf {
    use super::*;
    use std::time::Instant;

    /// Clicking a column header sorts by it: ascending, descending, then
    /// back to the original order. A static result sorts in place; a query
    /// is re-run with the sort. Hidden columns stay hidden.
    #[test]
    fn header_click_sorts() {
        use std::collections::HashMap;
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let core = rongo_core::CoreHandle::start(rt.handle().clone(), std::sync::Arc::new(|| {}));
        let (mut pending, mut next) = (HashMap::new(), 0);
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        // One frame; returns the actions and where the header `name` is drawn.
        let mut frame = |v: &mut ResultView, events: Vec<Event>| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0))),
                events,
                ..Default::default()
            };
            let mut actions = Vec::new();
            let mut out = ctx.run_ui(input, |ui| {
                let mut net = Net { core: &core, pending: &mut pending, next_view_id: &mut next, keys_enabled: true };
                // Collected from every pass, as the app does: input arrives in
                // the first, and a pass can be redrawn (the toolbar re-measuring).
                actions.extend(v.ui(ui, &mut net));
            });
            out.textures_delta.clear();
            let header = out.shapes.iter().find_map(|s| match &s.shape {
                egui::Shape::Text(t) if t.galley.text() == "name" => Some(t.pos + t.galley.rect.center().to_vec2()),
                _ => None,
            });
            (actions, header)
        };
        let mut click_header = |v: &mut ResultView| {
            let (_, at) = frame(v, vec![]);
            let at = at.expect("the name header is drawn");
            frame(v, vec![Event::PointerMoved(at)]);
            let button = |pressed| Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
            frame(v, vec![button(true)]);
            let (actions, _) = frame(v, vec![button(false)]);
            frame(v, vec![]);
            actions
        };
        let names = |v: &ResultView| (0..v.rows.len()).map(|i| v.rows.get(i).unwrap().as_document().unwrap().get("name").cloned()).collect::<Vec<_>>();
        let s = |t: &str| Some(Bson::String(t.into()));

        let docs = vec![doc! { "name": "b", "img": 1 }, doc! { "name": "c" }, doc! { "img": 2 }, doc! { "name": "a" }, doc! { "name": 5 }];
        let mut v = ResultView::new(1, "t", Source::Static { docs, truncated: false });
        v.hidden.insert("img".into());
        click_header(&mut v);
        assert_eq!(names(&v), vec![None, Some(Bson::Int32(5)), s("a"), s("b"), s("c")], "missing, then numbers, then strings");
        assert!(v.hidden.contains("img"), "sorting keeps hidden columns hidden");
        click_header(&mut v);
        assert_eq!(names(&v), vec![s("c"), s("b"), s("a"), Some(Bson::Int32(5)), None]);
        click_header(&mut v);
        assert_eq!(names(&v), vec![s("b"), s("c"), None, s("a"), Some(Bson::Int32(5))], "third click: original order");

        let mut v = big_view(3);
        let actions = click_header(&mut v);
        let Source::Find { spec, .. } = &v.source else { unreachable!() };
        assert_eq!(spec.sort, Some(doc! { "name": 1 }), "the query is re-run sorted");
        assert!(actions.iter().any(|(_, a)| matches!(a, ViewAction::SortChanged(s) if *s == doc! { "name": 1 })), "the Sort field shows it");
    }

    /// A query running for more than a moment shows in the view's own status
    /// line (not in a bar across the window): what it is, a moving bar, the
    /// time, and ⊗, which cancels it and keeps the rows on screen.
    #[test]
    fn slow_query_shows_in_the_status_line() {
        use std::collections::HashMap;
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let core = rongo_core::CoreHandle::start(rt.handle().clone(), std::sync::Arc::new(|| {}));
        let (mut pending, mut next) = (HashMap::new(), 0);
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut v = big_view(5);
        v.loading = Some(77);
        let started = std::time::Instant::now() - std::time::Duration::from_millis(2300);
        pending.insert(77, crate::net::Sent { what: Some("Running find on d.c".into()), started, ..crate::net::Sent::new(Origin::View(1, ViewReq::Page)) });
        let mut frame = |v: &mut ResultView, events: Vec<Event>| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0))),
                events,
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| {
                let mut net = Net { core: &core, pending: &mut pending, next_view_id: &mut next, keys_enabled: true };
                v.ui(ui, &mut net);
            });
            out.textures_delta.clear();
            out.shapes.iter().filter_map(|s| match &s.shape {
                egui::Shape::Text(t) => Some((t.galley.text().to_string(), t.pos + t.galley.rect.center().to_vec2())),
                _ => None,
            }).collect::<Vec<_>>()
        };
        frame(&mut v, vec![]);
        let texts = frame(&mut v, vec![]);
        let find = |label: &str| texts.iter().find(|(t, _)| t == label).map(|(_, p)| *p);
        let what = find("Running find on d.c…").expect("the query shows");
        let selected = find("0 documents selected").expect("status line");
        assert!((what.y - selected.y).abs() < 4.0, "on the status line: {what:?} vs {selected:?}");
        assert!(find("2s").is_some(), "time taken: {texts:?}");
        let x = texts.iter().filter(|(t, p)| t == icon::X_CIRCLE && (p.y - selected.y).abs() < 6.0).map(|(_, p)| *p).next().expect("⊗ on the status line");
        let press = |pressed| Event::PointerButton { pos: x, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        frame(&mut v, vec![Event::PointerMoved(x), press(true)]);
        frame(&mut v, vec![press(false)]);
        assert_eq!((v.loading, v.rows.len()), (None, 5), "cancelled, rows kept");
        let texts = frame(&mut v, vec![]);
        assert!(!texts.iter().any(|(t, _)| t.starts_with("Running find")), "gone: {texts:?}");
    }

    /// Right-click in the empty space under the rows opens a menu, as in
    /// Studio 3T: the selected row's menu when a row is selected, otherwise
    /// the one for the whole result (Insert, Refresh View, Export…).
    #[test]
    fn right_click_under_the_rows() {
        use std::collections::HashMap;
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let core = rongo_core::CoreHandle::start(rt.handle().clone(), std::sync::Arc::new(|| {}));
        let (mut pending, mut next) = (HashMap::new(), 0);
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut frame = |v: &mut ResultView, events: Vec<Event>| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0))),
                events,
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| {
                let mut net = Net { core: &core, pending: &mut pending, next_view_id: &mut next, keys_enabled: true };
                v.ui(ui, &mut net);
            });
            out.textures_delta.clear();
            out.shapes.iter().filter_map(|s| match &s.shape {
                egui::Shape::Text(t) => Some(t.galley.text().to_string()),
                _ => None,
            }).collect::<Vec<_>>()
        };
        let empty = pos2(600.0, 700.0);
        let right_click = |v: &mut ResultView, frame: &mut dyn FnMut(&mut ResultView, Vec<Event>) -> Vec<String>| {
            let button = |pressed| Event::PointerButton { pos: empty, button: egui::PointerButton::Secondary, pressed, modifiers: Modifiers::NONE };
            frame(v, vec![Event::PointerMoved(empty)]);
            frame(v, vec![button(true)]);
            frame(v, vec![button(false)]);
            let mut texts = Vec::new();
            for _ in 0..3 {
                texts = frame(v, vec![]);
            }
            frame(v, vec![Event::Key { key: Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE }]);
            for _ in 0..3 {
                frame(v, vec![]);
            }
            texts
        };
        let has = |texts: &[String], label: &str| texts.iter().any(|t| t.contains(label));

        let mut v = big_view(3);
        frame(&mut v, vec![]);
        let texts = right_click(&mut v, &mut frame);
        assert!(has(&texts, "Insert Document") && has(&texts, "Refresh View") && has(&texts, "Export Documents"), "{texts:?}");
        assert!(!has(&texts, "Copy Value"), "no row, so no cell items");

        v.selected = BTreeSet::from([1]);
        v.selected_col = Some("name".into());
        let texts = right_click(&mut v, &mut frame);
        assert!(has(&texts, "Field") && has(&texts, "Copy") && has(&texts, "Refresh Document"), "the selected row's menu: {texts:?}");
        assert_eq!(v.selected, BTreeSet::from([1]), "the selection stays");
    }

    /// Dragging a header moves its column: it lands where the blue line
    /// shows, keeps its own width, and stays put on the next page. A plain
    /// click still sorts (header_click_sorts).
    #[test]
    fn header_drag_moves_the_column() {
        use std::collections::HashMap;
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let core = rongo_core::CoreHandle::start(rt.handle().clone(), std::sync::Arc::new(|| {}));
        let (mut pending, mut next) = (HashMap::new(), 0);
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let names = ["_id", "name", "img", "zone"];
        // One frame; returns each header's rect (text centre and the cell's width), left to right.
        let mut frame = |v: &mut ResultView, events: Vec<Event>| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0))),
                events,
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| {
                let mut net = Net { core: &core, pending: &mut pending, next_view_id: &mut next, keys_enabled: true };
                v.ui(ui, &mut net);
            });
            out.textures_delta.clear();
            let mut heads: Vec<(String, egui::Pos2)> = out
                .shapes
                .iter()
                .filter_map(|s| match &s.shape {
                    egui::Shape::Text(t) if names.contains(&t.galley.text()) && t.pos.y < 120.0 => Some((t.galley.text().to_string(), t.pos + t.galley.rect.center().to_vec2())),
                    _ => None,
                })
                .collect();
            heads.sort_by(|a, b| a.1.x.total_cmp(&b.1.x));
            heads
        };
        let order = |h: &[(String, egui::Pos2)]| h.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>();
        let at = |h: &[(String, egui::Pos2)], n: &str| h.iter().find(|(c, _)| c == n).unwrap().1;
        let docs: Vec<Document> = (0..3).map(|i| doc! { "_id": i, "name": format!("n{i}"), "img": i, "zone": "z" }).collect();
        let mut v = ResultView::new(1, "t", Source::Static { docs, truncated: false });
        for _ in 0..3 {
            frame(&mut v, vec![]);
        }
        let heads = frame(&mut v, vec![]);
        assert_eq!(order(&heads), ["_id", "name", "img", "zone"]);
        let id_width = v.col_widths["_id"];
        assert!(id_width > 200.0, "_id starts wider: {id_width}");

        // Drag _id to between img and zone.
        let from = at(&heads, "_id");
        let to = pos2((at(&heads, "img").x + at(&heads, "zone").x) / 2.0 + 30.0, from.y);
        let button = |pos, pressed| Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        frame(&mut v, vec![Event::PointerMoved(from)]);
        frame(&mut v, vec![button(from, true)]);
        for k in 1..=6 {
            frame(&mut v, vec![Event::PointerMoved(from + (to - from) * (k as f32 / 6.0))]);
        }
        assert_eq!(v.dragging_col.as_deref(), Some("_id"), "a drag, not a click");
        frame(&mut v, vec![button(to, false)]);
        for _ in 0..3 {
            frame(&mut v, vec![]);
        }
        let heads = frame(&mut v, vec![]);
        assert_eq!(order(&heads), ["name", "img", "_id", "zone"]);
        assert!(v.dragging_col.is_none());
        assert!((v.col_widths["_id"] - id_width).abs() < 1.0, "_id kept its width: {} vs {id_width}", v.col_widths["_id"]);
        assert!(v.col_widths["name"] < 200.0, "the first column didn't take _id's width");
        let Source::Static { .. } = &v.source else { unreachable!() };
        assert!(v.local_sort.is_none(), "dragging didn't sort");

        // The order holds for the next page of the same result.
        v.refresh();
        frame(&mut v, vec![]);
        let heads = frame(&mut v, vec![]);
        assert_eq!(order(&heads), ["name", "img", "_id", "zone"]);

        // Dragging the edge between two headers still resizes, and moves nothing.
        let name_w = v.col_widths["name"];
        let cell = v.header_rects.iter().find(|(c, _)| c == "name").unwrap().1;
        let edge = pos2(cell.right() + 2.0, cell.center().y);
        frame(&mut v, vec![Event::PointerMoved(edge)]);
        frame(&mut v, vec![button(edge, true)]);
        for k in 1..=6 {
            frame(&mut v, vec![Event::PointerMoved(edge + vec2(10.0 * k as f32, 0.0))]);
        }
        frame(&mut v, vec![button(edge + vec2(60.0, 0.0), false)]);
        frame(&mut v, vec![]);
        let heads = frame(&mut v, vec![]);
        assert_eq!(order(&heads), ["name", "img", "_id", "zone"], "resizing moved nothing");
        assert!(v.col_widths["name"] > name_w + 40.0, "name was widened: {name_w} -> {}", v.col_widths["name"]);
    }

    /// The selected cell's outline covers the whole cell and is clipped like
    /// the cell's text, so it can't show through below the table.
    #[test]
    fn cell_outline_stays_inside_the_table() {
        use std::collections::HashMap;
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let core = rongo_core::CoreHandle::start(rt.handle().clone(), std::sync::Arc::new(|| {}));
        let (mut pending, mut next) = (HashMap::new(), 0);
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut v = big_view(50);
        v.selected = BTreeSet::from([2]);
        v.selected_col = Some("name".into());
        let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0))), ..Default::default() };
        let mut out = ctx.run_ui(input, |ui| {
            let mut net = Net { core: &core, pending: &mut pending, next_view_id: &mut next, keys_enabled: true };
            v.ui(ui, &mut net);
        });
        out.textures_delta.clear();

        let text = out
            .shapes
            .iter()
            .find_map(|s| match &s.shape {
                egui::Shape::Text(t) if t.galley.text() == "Position 2" => Some((s.clip_rect, t.galley.rect.translate(t.pos.to_vec2()))),
                _ => None,
            })
            .expect("the selected cell is drawn");
        let outlines: Vec<_> = out
            .shapes
            .iter()
            .filter_map(|s| match &s.shape {
                egui::Shape::Rect(r) if r.stroke.color == theme::ACCENT && r.fill == Color32::TRANSPARENT => Some((s.clip_rect, r.rect)),
                _ => None,
            })
            .collect();
        assert_eq!(outlines.len(), 1, "one outlined cell");
        let (clip, rect) = outlines[0];
        assert_eq!(clip, text.0, "clipped like the cell's text");
        assert!(rect.contains_rect(text.1) && rect.width() > text.1.width() + 40.0, "the whole cell, not just its text: {rect:?} vs {:?}", text.1);
    }

    /// In a narrow view every toolbar button stays on screen, wrapping onto
    /// another row rather than running off the edge or under the view mode.
    /// With room, they keep to one row.
    #[test]
    fn toolbar_wraps_when_narrow() {
        use std::collections::HashMap;
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let core = rongo_core::CoreHandle::start(rt.handle().clone(), std::sync::Arc::new(|| {}));
        let (mut pending, mut next) = (HashMap::new(), 0);
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut v = big_view(50);
        let buttons = [
            icon::ARROWS_CLOCKWISE,
            icon::CARET_LINE_LEFT,
            icon::CARET_LEFT,
            icon::CARET_RIGHT,
            icon::CARET_LINE_RIGHT,
            icon::CARET_DOWN,
            "Documents 1 to 50",
            icon::FILE_PLUS,
            icon::PENCIL_SIMPLE,
            icon::COPY,
            icon::PENCIL_LINE,
            icon::TRASH,
            icon::BROOM,
            "Table View",
        ];
        let mut draw = |width: f32| {
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 700.0));
            let mut shapes = Vec::new();
            for _ in 0..3 {
                let mut out = ctx.run_ui(egui::RawInput { screen_rect: Some(screen), ..Default::default() }, |ui| {
                    let mut net = Net { core: &core, pending: &mut pending, next_view_id: &mut next, keys_enabled: true };
                    v.ui(ui, &mut net);
                });
                out.textures_delta.clear();
                shapes = out.shapes;
            }
            let text = |s: &egui::epaint::ClippedShape| match &s.shape {
                egui::Shape::Text(t) => Some((t.galley.text().to_string(), t.galley.rect.translate(t.pos.to_vec2()), s.clip_rect)),
                _ => None,
            };
            let title = shapes.iter().filter_map(text).find(|(t, ..)| t == "big").expect("the title under the toolbar").1;
            let found: Vec<_> = shapes.iter().filter_map(text).filter(|(t, r, _)| r.bottom() <= title.top() && buttons.contains(&t.as_str())).collect();
            (screen, found)
        };

        let (screen, found) = draw(600.0);
        for b in buttons {
            let hits: Vec<_> = found.iter().filter(|(t, ..)| t == b).collect();
            assert_eq!(hits.len(), 1, "{b:?} drawn once: {found:?}");
            let (_, rect, clip) = hits[0];
            assert!(screen.contains_rect(*rect) && clip.contains_rect(*rect), "{b:?} fully visible: {rect:?} in {clip:?}");
        }
        for (i, (a, ra, _)) in found.iter().enumerate() {
            for (b, rb, _) in &found[i + 1..] {
                assert!(!ra.intersects(*rb), "{a:?} and {b:?} overlap");
            }
        }
        let rows = |found: &[(String, egui::Rect, egui::Rect)]| {
            let mut ys: Vec<i32> = found.iter().map(|(_, r, _)| r.center().y.round() as i32).collect();
            ys.sort();
            ys.dedup_by(|a, b| (*a - *b).abs() < 4);
            ys.len()
        };
        assert_eq!(rows(&found), 2, "wrapped onto a second row");
        let row_of = |b: &str| found.iter().find(|(t, ..)| t == b).unwrap().1.center().y;
        assert_eq!(row_of("Table View"), row_of(icon::ARROWS_CLOCKWISE), "the view mode stays up with the paging");
        assert!(row_of(icon::TRASH) > row_of(icon::ARROWS_CLOCKWISE), "the document buttons go below");

        let (_, found) = draw(1400.0);
        assert_eq!(found.len(), buttons.len());
        assert_eq!(rows(&found), 1, "one row with room: {found:?}");
    }

    fn big_view(n: usize) -> ResultView {
        let docs: Vec<Document> = (0..n)
            .map(|i| doc! { "_id": rongo_core::bson::oid::ObjectId::new(), "name": format!("Position {i}"), "img": [], "position": i as i32, "bannerType": "advertisers", "meta": { "a": 1, "b": "x" } })
            .collect();
        // Arrives the way a server page does: moved in by on_page.
        let conn = ConnId(uuid::Uuid::nil());
        let mut v = ResultView::new(1, "big", Source::Find { conn, spec: FindSpec::all(Namespace::new("d", "c"), n as i64), max: None });
        v.page_size = n as i64;
        v.needs_fetch = false;
        v.loading = Some(7);
        v.on_page(7, docs.iter().map(|d| RawDocumentBuf::try_from(d).unwrap()).collect(), false, 1);
        v
    }

    /// Milliseconds per frame, after the first, for each view mode. Run with
    /// `cargo test -p rongo --release perf -- --ignored --nocapture`.
    fn frame_ms(mode: Mode, n: usize) -> f64 {
        use std::collections::HashMap;
        use std::sync::Arc;
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let core = rongo_core::CoreHandle::start(rt.handle().clone(), Arc::new(|| {}));
        let (mut pending, mut next) = (HashMap::new(), 0);
        let mut v = big_view(n);
        v.mode = mode;
        let arrived = Instant::now();
        v.selected = BTreeSet::from([3]);
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut frame = |v: &mut ResultView| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0))),
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| {
                let mut net = Net { core: &core, pending: &mut pending, next_view_id: &mut next, keys_enabled: true };
                v.ui(ui, &mut net);
            });
            out.textures_delta.clear();
        };
        frame(&mut v);
        let first_ms = arrived.elapsed().as_secs_f64() * 1000.0;
        frame(&mut v);
        while v.json_building.is_some() {
            std::thread::sleep(std::time::Duration::from_millis(5));
            frame(&mut v);
        }
        // The frames really had the data to draw.
        assert_eq!(v.rows.len(), n);
        if mode == Mode::Json {
            assert!(v.json_lines.as_ref().is_some_and(|l| l.len() > n), "JSON lines were built");
        }
        if mode == Mode::Tree {
            assert_eq!(v.tree_heights.len(), n);
        }
        let started = Instant::now();
        let frames = 10;
        for _ in 0..frames {
            frame(&mut v);
        }
        let ms = started.elapsed().as_secs_f64() * 1000.0 / frames as f64;
        LAST_FIRST_FRAME_MS.with(|f| f.set(first_ms));
        ms
    }

    thread_local! {
        static LAST_FIRST_FRAME_MS: std::cell::Cell<f64> = const { std::cell::Cell::new(0.0) };
    }

    /// Leaving the JSON view frees its text.
    #[test]
    fn json_text_is_dropped_when_leaving_the_json_view() {
        use std::collections::HashMap;
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let core = rongo_core::CoreHandle::start(rt.handle().clone(), std::sync::Arc::new(|| {}));
        let (mut pending, mut next) = (HashMap::new(), 0);
        let mut v = big_view(200);
        let ctx = egui::Context::default();
        let mut frame = |v: &mut ResultView| {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                let mut net = Net { core: &core, pending: &mut pending, next_view_id: &mut next, keys_enabled: true };
                v.ui(ui, &mut net);
            });
            out.textures_delta.clear();
        };
        v.mode = Mode::Json;
        while v.json_lines.is_none() {
            frame(&mut v);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        v.mode = Mode::Table;
        frame(&mut v);
        assert!(v.json_lines.is_none() && v.json_building.is_none());
    }

    /// The JSON view's lines reach the bottom of the visible area: rows are
    /// drawn at the height the scroll area assumed for them.
    #[test]
    fn json_lines_fill_the_view() {
        use std::collections::HashMap;
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let core = rongo_core::CoreHandle::start(rt.handle().clone(), std::sync::Arc::new(|| {}));
        let (mut pending, mut next) = (HashMap::new(), 0);
        let mut v = big_view(2_000);
        v.mode = Mode::Json;
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut frame = |v: &mut ResultView| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0))),
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| {
                let mut net = Net { core: &core, pending: &mut pending, next_view_id: &mut next, keys_enabled: true };
                v.ui(ui, &mut net);
            });
            out.textures_delta.clear();
        };
        while v.json_lines.is_none() {
            frame(&mut v);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        frame(&mut v);
        frame(&mut v);
        let (drawn, visible) = v.json_fill.expect("JSON view drawn");
        assert!(drawn >= visible - 1.0, "lines end at {drawn}, the view at {visible}");
    }

    /// A new page's text arrives while the find bar still holds matches for
    /// the old one: they are dropped, not drawn at the old byte offsets
    /// (which panicked inside a Thai character).
    #[test]
    fn find_matches_do_not_outlive_their_text() {
        use std::collections::HashMap;
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let core = rongo_core::CoreHandle::start(rt.handle().clone(), std::sync::Arc::new(|| {}));
        let (mut pending, mut next) = (HashMap::new(), 0);
        let first = vec![doc! { "name": "abcdefgh" }];
        let mut v = ResultView::new(1, "t", Source::Static { docs: first.clone(), truncated: false });
        v.rows = Docs::from_docs(&first);
        v.needs_fetch = false;
        v.mode = Mode::Json;
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut frame = |v: &mut ResultView| {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                let mut net = Net { core: &core, pending: &mut pending, next_view_id: &mut next, keys_enabled: true };
                v.ui(ui, &mut net);
            });
            out.textures_delta.clear();
        };
        let settle = |v: &mut ResultView, frame: &mut dyn FnMut(&mut ResultView)| {
            for _ in 0..500 {
                frame(v);
                if v.json_lines.is_some() && v.find.as_ref().is_none_or(|f| f.running.is_none()) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        };
        settle(&mut v, &mut frame);
        v.find = Some(Find { text: "gh".into(), ..Default::default() });
        settle(&mut v, &mut frame);
        assert_eq!(v.find.as_ref().unwrap().matches.len(), 1);
        // The next page: same line, Thai text where the match was.
        v.rows = Docs::from_docs(&[doc! { "name": "ภาษาไทย" }]);
        v.rows_changed();
        for _ in 0..200 {
            frame(&mut v);
        }
        assert!(v.find.as_ref().unwrap().matches.is_empty());
    }

    /// Rows are parsed only when used: drawing a 50,000-row Table or Tree
    /// view parses the rows in view, not the page, and scrolling through it
    /// keeps the parsed cache bounded.
    #[test]
    fn only_rows_in_use_are_parsed() {
        use std::collections::HashMap;
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let core = rongo_core::CoreHandle::start(rt.handle().clone(), std::sync::Arc::new(|| {}));
        let (mut pending, mut next) = (HashMap::new(), 0);
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        for mode in [Mode::Table, Mode::Tree, Mode::Json] {
            let mut v = big_view(50_000);
            v.mode = mode;
            let mut frame = |v: &mut ResultView, events: Vec<Event>| {
                let input = egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0))),
                    events,
                    ..Default::default()
                };
                let mut out = ctx.run_ui(input, |ui| {
                    let mut net = Net { core: &core, pending: &mut pending, next_view_id: &mut next, keys_enabled: true };
                    v.ui(ui, &mut net);
                });
                out.textures_delta.clear();
            };
            frame(&mut v, vec![Event::PointerMoved(pos2(600.0, 500.0))]);
            frame(&mut v, vec![]);
            assert!(v.rows.parsed() < 100, "{}: {} rows parsed to draw one screen", mode.label(), v.rows.parsed());
            for _ in 0..60 {
                frame(&mut v, vec![Event::MouseWheel { unit: egui::MouseWheelUnit::Page, delta: egui::vec2(0.0, -3.0), modifiers: Modifiers::NONE, phase: egui::TouchPhase::Move }]);
                assert!(v.rows.parsed() <= MAX_PARSED + 100, "{}: {} rows parsed", mode.label(), v.rows.parsed());
            }
            // Everything selected: the toolbar and shortcuts must not look at
            // every selected document each frame.
            let cmd_a = Event::Key { key: Key::A, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::COMMAND };
            frame(&mut v, vec![cmd_a]);
            for _ in 0..3 {
                frame(&mut v, vec![]);
                assert!(v.rows.parsed() <= MAX_PARSED + 100, "{}, all selected: {} rows parsed", mode.label(), v.rows.parsed());
            }
        }
    }

    /// ⌘A in the JSON view selects all of its text, including lines not on
    /// screen, and ⌘C copies it; a click clears it. In the Table view ⌘A
    /// selects every document.
    #[test]
    fn command_a_selects_everything() {
        use std::collections::HashMap;
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let core = rongo_core::CoreHandle::start(rt.handle().clone(), std::sync::Arc::new(|| {}));
        let (mut pending, mut next) = (HashMap::new(), 0);
        let mut v = big_view(500);
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut frame = |v: &mut ResultView, events: Vec<Event>| -> Option<String> {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0))),
                events,
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| {
                let mut net = Net { core: &core, pending: &mut pending, next_view_id: &mut next, keys_enabled: true };
                v.ui(ui, &mut net);
            });
            out.textures_delta.clear();
            out.platform_output.commands.into_iter().find_map(|c| match c {
                egui::OutputCommand::CopyText(t) => Some(t),
                _ => None,
            })
        };
        let cmd_a = Event::Key { key: Key::A, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::COMMAND };

        frame(&mut v, vec![cmd_a.clone()]);
        assert_eq!(v.selected.len(), 500, "Table view: ⌘A selected every document");

        v.mode = Mode::Json;
        v.selected.clear();
        while v.json_lines.is_none() {
            frame(&mut v, vec![]);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        frame(&mut v, vec![cmd_a]);
        let copied = frame(&mut v, vec![Event::Copy]).expect("⌘C copied something");
        let lines = v.json_lines.as_ref().unwrap();
        assert_eq!(copied, lines.text, "the whole JSON text, not just what is on screen");
        assert!(copied.contains("Position 499"));

        frame(&mut v, vec![Event::PointerMoved(pos2(600.0, 500.0))]);
        let click = |pressed| Event::PointerButton { pos: pos2(600.0, 500.0), button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        frame(&mut v, vec![click(true)]);
        frame(&mut v, vec![click(false)]);
        assert_ne!(frame(&mut v, vec![Event::Copy]).as_deref(), Some(copied.as_str()), "a click cleared select-all");
    }

    /// ⌘F over a view opens the find bar on the JSON view; typing finds,
    /// scrolls to the first match, and ↩ steps to the next.
    #[test]
    fn find_bar_finds_and_scrolls() {
        use std::collections::HashMap;
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let core = rongo_core::CoreHandle::start(rt.handle().clone(), std::sync::Arc::new(|| {}));
        let (mut pending, mut next) = (HashMap::new(), 0);
        let mut v = big_view(2_000);
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        let mut frame = |v: &mut ResultView, events: Vec<Event>| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 900.0))),
                events,
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| {
                let mut net = Net { core: &core, pending: &mut pending, next_view_id: &mut next, keys_enabled: true };
                v.ui(ui, &mut net);
            });
            out.textures_delta.clear();
        };
        let key = |key: Key, modifiers: Modifiers| Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers };
        frame(&mut v, vec![Event::PointerMoved(pos2(600.0, 500.0))]);
        frame(&mut v, vec![key(Key::F, Modifiers::COMMAND)]);
        assert!(v.find_open(), "⌘F opened the find bar on the JSON view");
        while v.json_lines.is_none() {
            frame(&mut v, vec![]);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        frame(&mut v, vec![Event::Text("position 1".into())]);
        frame(&mut v, vec![]);
        while v.find.as_ref().is_some_and(|f| f.running.is_some()) {
            std::thread::sleep(std::time::Duration::from_millis(2));
            frame(&mut v, vec![]);
        }
        let f = v.find.as_ref().unwrap();
        assert_eq!(f.text, "position 1", "the field had focus");
        // "Position 1", "Position 10".."19", "Position 100".. and the
        // "position" fields' values 1, 10.. don't match ("position" : 1).
        assert_eq!(f.matches.len(), 1 + 10 + 100 + 1000);
        assert_eq!(f.current, 0);
        let first = f.matches[0].0;
        frame(&mut v, vec![key(Key::Enter, Modifiers::NONE)]);
        frame(&mut v, vec![]);
        let f = v.find.as_ref().unwrap();
        assert_eq!(f.current, 1, "↩ went to the next match");
        let second = f.matches[1].0;
        assert!(second > first + 5);
        // Jumping to the last match scrolls far down.
        let last = f.matches.len() - 1;
        v.find.as_mut().unwrap().current = last - 1;
        frame(&mut v, vec![key(Key::Enter, Modifiers::NONE)]);
        frame(&mut v, vec![]);
        let (current, line) = { let f = v.find.as_ref().unwrap(); (f.current, f.matches[f.current].0) };
        assert_eq!(current, last);
        assert!(v.json_top <= line && line < v.json_top + 60, "match line {line} is in view (top {})", v.json_top);
        frame(&mut v, vec![key(Key::Escape, Modifiers::NONE)]);
        assert!(v.find.is_none(), "Esc closed the find bar");
    }

    #[test]
    #[ignore = "timing; run explicitly in release mode"]
    fn report_find_time_for_2m_lines() {
        let lines = JsonText::from_lines(&(0..2_000_000).map(|i| format!(r#"        "en" : "https://cdn.example/img/game/DGM_{i}.webp","#)).collect::<Vec<_>>());
        for (needle, case) in [("dgm_19999", false), ("DGM_19999", true), ("webp", false)] {
            let t = Instant::now();
            let n = find_matches(&lines, needle, case, &std::sync::atomic::AtomicBool::new(false)).len();
            eprintln!("{needle:>10} case={case}: {n} matches in {:.1} ms", t.elapsed().as_secs_f64() * 1000.0);
        }
    }

    #[test]
    #[ignore = "timing; run explicitly in release mode"]
    fn report_frame_times_for_50k_rows() {
        for mode in [Mode::Table, Mode::Tree, Mode::Json] {
            let ms = frame_ms(mode, 50_000);
            let first = LAST_FIRST_FRAME_MS.with(|f| f.get());
            eprintln!("{:>10}: {ms:.2} ms/frame, first frame with the data {first:.1} ms", mode.label());
        }
    }

    /// A frame with 50,000 rows must cost about what a frame with 500 does
    /// (only visible rows may be laid out). Debug builds are slow, so this
    /// compares the two instead of using an absolute budget.
    #[test]
    fn frame_cost_does_not_grow_with_row_count() {
        for mode in [Mode::Table, Mode::Tree, Mode::Json] {
            let small = frame_ms(mode, 500);
            let big = frame_ms(mode, 50_000);
            assert!(big < small * 3.0 + 5.0, "{}: {big:.1} ms with 50k rows vs {small:.1} ms with 500", mode.label());
        }
    }
}
