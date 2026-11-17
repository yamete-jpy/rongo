//! A page of documents with Table, Tree and JSON views. Used by collection
//! tabs, shell result tabs and info tabs.

mod docs;
mod drill;
pub(crate) mod find;
mod json_text;
mod json_view;
mod keys;
mod menus;
mod sort;
mod table;
mod toolbar;
mod tree;

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use egui::{Key, Modifiers, Sense, Ui, vec2};
use egui_phosphor::regular as icon;
use rongo_core::bson::{Bson, Document, RawDocumentBuf, doc};
use rongo_core::{Command, ConnId, FindSpec, Namespace, RequestId};

use crate::net::{Net, Origin, ViewReq};
use crate::theme;

use docs::{Docs, MAX_PARSED, Places, raw_id};
use drill::{DocKey, Level, Rows, Step, compute_columns, join};
use find::Find;
use json_text::Build;
pub(crate) use json_text::TextLines;
pub(crate) use json_view::line_view;
pub use menus::LOCKED;
use sort::{LocalSort, SortKey, SortKeys, lookup, lookup_raw, page_order, sort_keys, sort_order};
use table::RowDrag;
pub use toolbar::thousands;
use tree::TreeHeights;

pub enum Source {
    /// Pages are fetched from the server. `max` caps the total, from a
    /// cursor's `limit()`.
    Find { conn: ConnId, spec: FindSpec, max: Option<u64> },
    /// Documents already in hand (shell output, command replies).
    Static { docs: Vec<Document>, truncated: bool },
    /// Raw documents already in hand (a Collection History preview), parsed
    /// only as their rows are drawn. `truncated`: the first of more.
    Raw { docs: Arc<Vec<RawDocumentBuf>>, truncated: bool },
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

/// Temp-data key a result view sets while it wants ⌘F (pointer over it, or
/// its find bar focused). The app reads it to leave ⌘F to the view.
pub const FIND_CLAIM: &str = "result-view-find";

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
    /// The view re-ran its query sorted, from a column menu.
    SortChanged(Document),
}

const PAGE_SIZES: [i64; 6] = [20, 50, 100, 200, 500, 1000];
const MAX_PAGE_SIZE: i64 = crate::settings::MAX_PAGE_SIZE;
static DEFAULT_PAGE_SIZE: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(50);

/// Page size for views opened from now on.
pub fn set_default_page_size(n: i64) {
    DEFAULT_PAGE_SIZE.store(n.clamp(1, MAX_PAGE_SIZE), std::sync::atomic::Ordering::Relaxed);
}

pub struct ResultView {
    pub id: u64,
    pub title: String,
    pub source: Source,
    /// The view's connection is locked: nothing here offers a write.
    pub read_only: bool,
    paging: Paging,
    /// The page's documents. Shared, so a frame can borrow them without
    /// cloning 50,000 documents.
    rows: Docs,
    /// Where the page's documents are in the whole result.
    places: Places,
    pub error: Option<String>,
    pub mode: Mode,
    selection: Selection,
    drill: Vec<Step>,
    /// The rows sorted here from a column header.
    local_sort: Option<LocalSort>,
    /// The sort keys last read, kept for the next click on the same column.
    sort_keys: Option<SortKeys>,
    /// The rows at the current drill level, resolved once per change of
    /// the rows or the drill (`level`).
    level: std::cell::RefCell<Option<Arc<Level>>>,
    json: JsonState,
    /// Measured height of each Tree view row, for drawing only visible rows.
    tree_heights: TreeHeights,
    /// Counts changes to which document is on which row (`order_changed`).
    rows_gen: u64,
    /// `rows_gen` when the open right-click menu was opened: its row may
    /// show another document once the count moves on, so it closes.
    menu_gen: Option<u64>,
    /// Reads of single documents in flight (Refresh Document, and after a
    /// write to one document), with the `_id` each re-reads.
    refreshing: Vec<(RequestId, Bson)>,
    /// Documents to re-read on the next frame (`refresh_id`).
    rereads: Vec<Bson>,
    cols: Columns,
    /// Width of each toolbar group (paging, document buttons, view mode)
    /// as last drawn, to decide which share a row.
    toolbar_widths: [f32; 3],
    /// Header cells as last drawn.
    #[cfg(test)]
    header_rects: Vec<(String, egui::Rect)>,
}

/// Which page is shown, and the requests that fetch and count it.
struct Paging {
    page_size: i64,
    /// The page size field's text while it is being typed.
    page_text: String,
    offset: u64,
    has_more: bool,
    needs_fetch: bool,
    loading: Option<RequestId>,
    counting: Option<RequestId>,
    elapsed_ms: Option<u64>,
    total: Option<u64>,
    /// Last page was pressed before the count was known: jump there when
    /// the count arrives.
    pending_last: bool,
}

/// The selected rows, and the cell and the drag that go with them.
#[derive(Default)]
struct Selection {
    selected: BTreeSet<usize>,
    anchor: Option<usize>,
    /// Column of the last clicked cell, for Step Into and Field shortcuts.
    selected_col: Option<String>,
    /// A drag over the table's rows, while the button is down.
    row_drag: Option<RowDrag>,
}

impl Selection {
    /// Forget the selection, and with it the row a ⇧-click extends from.
    fn clear(&mut self) {
        self.selected.clear();
        self.anchor = None;
    }
}

/// The JSON view's text, and its find bar and select-all.
#[derive(Default)]
struct JsonState {
    /// The JSON view's text, one entry per line, built on background threads
    /// on first use. Kept while the rows are the same, also when another
    /// view mode is shown (the owner prefers speed to memory).
    lines: Option<Arc<TextLines>>,
    building: Option<Build>,
    /// ⌘A ⌘C in the JSON view: the text to copy (in `Style::Editable`),
    /// put on the clipboard once it is built.
    copy: Option<Build>,
    /// JSON texts started for the view.
    #[cfg(test)]
    builds: usize,
    find: Option<Find>,
    /// ⌘A in the JSON view: all of its text is selected, on screen or not.
    all: bool,
    /// First JSON line in view last frame, where a new search starts.
    top: usize,
    /// Bottom of the last JSON line drawn and of the visible area, last frame.
    #[cfg(test)]
    fill: Option<(f32, f32)>,
}

/// The Table view's columns: as found, hidden, ordered, sized and dragged.
#[derive(Default)]
struct Columns {
    /// Column names at the current drill level, computed once per change.
    cache: Option<Vec<String>>,
    /// Columns hidden from the column menu, per drill level (keyed as
    /// `order` is).
    hidden: HashMap<String, BTreeSet<String>>,
    /// Column order chosen by dragging headers, per drill level (keyed by
    /// the drill path). Columns not in the list keep their own order, after.
    order: HashMap<String, Vec<String>>,
    /// Each column's width as last drawn, so a column keeps its width when
    /// it moves (the table's own widths go by position).
    widths: HashMap<String, f32>,
    /// The header being dragged.
    dragging: Option<String>,
}

impl ResultView {
    pub fn new(id: u64, title: impl Into<String>, source: Source) -> Self {
        let total = match &source {
            Source::Static { docs, .. } => Some(docs.len() as u64),
            Source::Raw { docs, .. } => Some(docs.len() as u64),
            Source::Find { .. } => None,
        };
        Self {
            id,
            title: title.into(),
            source,
            read_only: false,
            paging: Paging {
                page_size: DEFAULT_PAGE_SIZE.load(std::sync::atomic::Ordering::Relaxed),
                page_text: String::new(),
                offset: 0,
                has_more: false,
                needs_fetch: true,
                loading: None,
                counting: None,
                elapsed_ms: None,
                total,
                pending_last: false,
            },
            rows: Docs::default(),
            places: Places::From(0),
            error: None,
            mode: Mode::Table,
            selection: Selection::default(),
            drill: Vec::new(),
            local_sort: None,
            sort_keys: None,
            level: Default::default(),
            json: JsonState::default(),
            tree_heights: TreeHeights::default(),
            rows_gen: 0,
            menu_gen: None,
            refreshing: Vec::new(),
            rereads: Vec::new(),
            cols: Columns::default(),
            toolbar_widths: [0.0; 3],
            #[cfg(test)]
            header_rects: Vec::new(),
        }
    }

    /// Replace the query and start again from the first page.
    pub fn set_spec(&mut self, spec: FindSpec) {
        if let Source::Find { spec: s, .. } = &mut self.source {
            *s = spec;
        }
        self.paging.offset = 0;
        self.paging.total = None;
        // A count still running was for the old query.
        self.paging.counting = None;
        self.paging.pending_last = false;
        // A new query shows in its own order.
        self.local_sort = None;
        self.set_drill(Vec::new());
        self.refresh();
    }

    /// After a write known to touch only the document with this `_id` (the
    /// document editor's Save, Edit Value, Set to null, Remove Field):
    /// re-read just that one, on the next frame. The rest of the page, the
    /// selection and the scroll position stay. A document that is not on
    /// the page needs nothing, and a page about to be fetched has it anyway.
    /// A page already on its way may have been read before the write, and
    /// landing after the re-read it would undo it: that page is fetched
    /// again instead (cancelling it), as a whole refresh does.
    pub fn refresh_id(&mut self, id: Bson) {
        if self.paging.loading.is_some() {
            self.refresh();
        } else if !self.paging.needs_fetch && !self.rereads.contains(&id) {
            self.rereads.push(id);
        }
    }

    pub fn refresh(&mut self) {
        self.paging.needs_fetch = true;
    }

    /// Drop cached text so it is rebuilt (after a display setting changed).
    pub fn relayout(&mut self) {
        self.json.lines = None;
        self.json.building = None;
    }

    /// Forget everything derived from the rows.
    fn rows_changed(&mut self) {
        self.cols.cache = None;
        self.order_changed();
    }

    /// Forget what follows the rows' order: the same documents, sorted
    /// another way. The columns stay, so a header stays where it was clicked.
    fn order_changed(&mut self) {
        self.json.lines = None;
        self.json.building = None;
        self.json.all = false;
        self.tree_heights.clear();
        self.rows_gen += 1;
    }

    fn set_drill(&mut self, drill: Vec<Step>) {
        self.drill = drill;
        self.cols.cache = None;
        // Made again for the new level, whatever was kept for an earlier one.
        *self.level.get_mut() = None;
        self.tree_heights.clear();
        self.selection.clear();
        self.selection.selected_col = None;
    }

    /// The collection this view reads from, if it is a find.
    fn source_ns(&self) -> Option<(ConnId, &Namespace)> {
        match &self.source {
            Source::Find { conn, spec, .. } => Some((*conn, &spec.ns)),
            Source::Static { .. } | Source::Raw { .. } => None,
        }
    }

    /// Where this view may write: its collection, unless the connection is
    /// locked. Reading (refresh, export) uses `source_ns`.
    fn editable(&self) -> Option<(ConnId, &Namespace)> {
        self.source_ns().filter(|_| !self.read_only)
    }

    // The writes the toolbar, the menus and the keys ask the app for. Each
    // is None where the view may not write (`editable`), so that rule is
    // kept here, not at every button.

    /// Insert Document, starting empty.
    fn insert(&self) -> Option<ViewAction> {
        let (conn, ns) = self.editable()?;
        Some(ViewAction::Insert { conn, ns: ns.clone(), template: None })
    }

    /// Edit Document, for `doc`.
    fn edit(&self, doc: Document) -> Option<ViewAction> {
        let (conn, ns) = self.editable()?;
        Some(ViewAction::Edit { conn, ns: ns.clone(), doc })
    }

    /// Clone Document, of `doc`.
    fn clone_doc(&self, doc: Document) -> Option<ViewAction> {
        let (conn, ns) = self.editable()?;
        Some(ViewAction::Clone { conn, ns: ns.clone(), doc })
    }

    /// Delete the documents with these `_id`s.
    fn delete(&self, ids: Vec<Bson>) -> Option<ViewAction> {
        let (conn, ns) = self.editable()?;
        Some(ViewAction::Delete { conn, ns: ns.clone(), ids })
    }

    /// The Update dialog (`delete`: the Delete dialog) for the view's query
    /// and the `selected` `_id`s; `field` pre-fills the `$set`.
    fn bulk(&self, delete: bool, selected: Vec<Bson>, field: Option<(String, Bson)>) -> Option<ViewAction> {
        let (conn, ns) = self.editable()?;
        Some(ViewAction::Bulk { conn, ns: ns.clone(), delete, selected, query: self.query(), field })
    }

    /// Edit Value, for the field at `path` of the document with this `_id`.
    fn edit_value(&self, id: Bson, path: String, value: Bson) -> Option<ViewAction> {
        let (conn, ns) = self.editable()?;
        Some(ViewAction::EditValue { conn, ns: ns.clone(), id, path, value })
    }

    /// `update`, applied to the document with this `_id` right away.
    fn update_one(&self, id: Bson, update: Document) -> Option<ViewAction> {
        let (conn, ns) = self.editable()?;
        Some(ViewAction::UpdateOne { conn, ns: ns.clone(), id, update })
    }

    /// Paste `text` as documents.
    fn paste(&self, text: String) -> Option<ViewAction> {
        let (conn, ns) = self.editable()?;
        Some(ViewAction::Paste { conn, ns: ns.clone(), text })
    }

    fn fetch(&mut self, net: &mut Net) {
        self.paging.needs_fetch = false;
        // Any page change overrides a Last page still waiting for its count.
        self.paging.pending_last = false;
        // Single documents read for the old page: the new one has them.
        self.refreshing.clear();
        self.rereads.clear();
        self.error = None;
        self.selection.clear();
        // A query's rows stay until its next page lands (`on_page` forgets
        // what was made of them), and so does the JSON text shown; but a
        // text still being built of them is no longer wanted, and none is
        // started until that page is in (`json`). The other sources have
        // their new rows below.
        self.json.building = None;
        self.json.all = false;
        match &self.source {
            Source::Find { conn, spec, max } => {
                let mut page = spec.clone();
                page.skip = spec.skip + self.paging.offset;
                page.limit = match max {
                    Some(m) => (m.saturating_sub(self.paging.offset) as i64).min(self.paging.page_size).max(0),
                    None => self.paging.page_size,
                };
                if page.limit == 0 {
                    self.rows = Docs::default();
                    self.places = Places::From(self.paging.offset as usize);
                    self.rows_changed();
                    self.follow_drill();
                    self.paging.has_more = false;
                    return;
                }
                if let Some(old) = self.paging.loading.take() {
                    net.send(Origin::None, None, Command::Cancel(old));
                }
                self.paging.loading = Some(net.send(Origin::View(self.id, ViewReq::Page), Some(*conn), Command::Find(page)));
            }
            Source::Static { docs, .. } => {
                let start = (self.paging.offset as usize).min(docs.len());
                let end = (start + self.paging.page_size as usize).min(docs.len());
                let (own, order) = page_order(self.local_sort.as_ref().map(|s| &s.order[..]), start, end);
                self.rows = Docs::from_docs(own.iter().map(|&i| &docs[i])).ordered(order);
                self.paging.has_more = end < docs.len();
                self.places = Places::Listed(own);
                self.rows_changed();
                self.follow_drill();
            }
            Source::Raw { docs, .. } => {
                let start = (self.paging.offset as usize).min(docs.len());
                let end = (start + self.paging.page_size as usize).min(docs.len());
                let (own, order) = page_order(self.local_sort.as_ref().map(|s| &s.order[..]), start, end);
                // One page holds them all: no second copy.
                let page = if own.len() == docs.len() { Docs::shared(docs.clone()) } else { Docs::new(own.iter().map(|&i| docs[i].clone()).collect()) };
                self.rows = page.ordered(order);
                self.paging.has_more = end < docs.len();
                self.places = Places::Listed(own);
                self.rows_changed();
                self.follow_drill();
            }
        }
    }

    /// Where row `i`'s document is in the whole result: a query's in the
    /// order the server sent it, a static result's in its own order.
    /// Unlike the row, it stays with the document through header sorts
    /// and, for a static result, page turns.
    fn place(&self, i: usize) -> Option<usize> {
        self.rows.arrival(i).map(|a| self.places.of(a))
    }

    /// How a row step remembers the document on row `i`.
    fn doc_key(&self, i: usize) -> Option<DocKey> {
        Some(DocKey { place: self.place(i)?, id: self.rows.raw_doc(i).and_then(|d| raw_id(d)) })
    }

    /// The row the remembered document is on now, if it is on the page: a
    /// query's found by `_id` (a page fetched again may hold it anywhere),
    /// a static result's by its place.
    fn find_row(&self, key: &DocKey) -> Option<usize> {
        let a = match (&self.source, &key.id) {
            (Source::Find { .. }, Some(id)) => self.rows.find(id)?,
            _ => self.places.find(key.place, self.rows.len())?,
        };
        self.rows.row_of(a)
    }

    /// The rows changed under the drill (another page, a sort, a refreshed
    /// document): a row step follows its document to the row it is on now,
    /// or, when the document is not on the page any more, the drill steps
    /// back out to the level above that step. Only the first row step can
    /// move: the steps after it are inside that one document.
    fn follow_drill(&mut self) {
        let Some(k) = self.drill.iter().position(|s| !matches!(s, Step::Field(_))) else { return };
        let (Step::Elements { doc, .. } | Step::Cell { doc, .. }) = &self.drill[k] else { return };
        let row = self.find_row(doc).and_then(|top| match k {
            0 => Some(top),
            // Under column drills: at most one row per document, in order.
            _ => Level::resolve(&self.rows, &self.drill[..k], self.level_key()).rows.binary_search(&top).ok(),
        });
        match row {
            Some(r) => {
                if let Step::Elements { row, .. } | Step::Cell { row, .. } = &mut self.drill[k] {
                    *row = r;
                }
            }
            None => {
                let above = self.drill[..k].to_vec();
                self.set_drill(above);
            }
        }
    }

    /// Re-read the document on row `origin` by `_id` (Refresh Document).
    fn refresh_doc(&mut self, net: &mut Net, origin: usize) {
        if let Some(id) = self.rows.raw_doc(origin).and_then(|d| raw_id(d)) {
            self.reread(net, id);
        }
    }

    /// Ask for the document with this `_id` again, as the page's query
    /// shows it (its projection), if it is on the page. A newer read of
    /// the same document replaces one still on its way.
    fn reread(&mut self, net: &mut Net, id: Bson) {
        let Source::Find { conn, spec, .. } = &self.source else { return };
        if self.rows.find(&id).is_none() {
            return;
        }
        let one = FindSpec { filter: doc! { "_id": id.clone() }, projection: spec.projection.clone(), ..FindSpec::all(spec.ns.clone(), 1) };
        let req = net.send(Origin::View(self.id, ViewReq::RefreshDoc(id.clone())), Some(*conn), Command::Find(one));
        self.refreshing.retain(|(_, i)| *i != id);
        self.refreshing.push((req, id));
    }

    pub fn on_page(&mut self, req: RequestId, docs: Vec<RawDocumentBuf>, has_more: bool, elapsed_ms: u64) {
        if self.paging.loading != Some(req) {
            return;
        }
        self.paging.loading = None;
        let capped = match &self.source {
            Source::Find { max: Some(m), .. } => self.paging.offset + docs.len() as u64 >= *m,
            _ => false,
        };
        self.rows = Docs::new(docs);
        self.places = Places::From(self.paging.offset as usize);
        match self.local_sort.take() {
            // A header sort stays on as pages turn: each page is sorted as it lands.
            Some(s) => self.sort_by(s.path, Some(s.dir)),
            // Keys read from an earlier page are of no more use.
            None => self.sort_keys = None,
        }
        // Rows clicked while this page loaded were rows of the old page.
        self.selection.clear();
        self.paging.has_more = has_more && !capped;
        self.paging.elapsed_ms = Some(elapsed_ms);
        self.rows_changed();
        self.follow_drill();
        // Nothing left at the drill level (an empty page, or an array that
        // is gone from the document): back out.
        if self.resolved().is_empty() && !self.drill.is_empty() {
            self.set_drill(Vec::new());
        }
    }

    /// The reply to `reread` for the document with this `_id`: replace it,
    /// or drop it if it is gone. Its row is found by the `_id`, wherever a
    /// header sort has put it since. The rest of the page stays as it is.
    pub fn on_doc(&mut self, req: RequestId, id: Bson, docs: Vec<RawDocumentBuf>) {
        // Only a read still wanted: not one for a page that has since
        // changed, nor one a newer read of the same document replaced.
        let Some(k) = self.refreshing.iter().position(|(r, _)| *r == req) else { return };
        self.refreshing.remove(k);
        // Not on the page (any more): nothing to update.
        let Some(at) = self.rows.find(&id) else { return };
        let doc = match docs.into_iter().next() {
            Some(d) if raw_id(&d).as_ref() == Some(&id) => Some(d),
            Some(_) => return,
            None => None,
        };
        let removed = doc.is_none();
        // Rows may move below (a removal, a header sort, a drill whose
        // rows the change reshapes): which document each selected row shows.
        let may_move = removed || self.local_sort.is_some() || !self.drill.is_empty();
        let (selected, anchor) = (self.selection.selected.clone(), self.selection.anchor);
        let before = if may_move && !(selected.is_empty() && anchor.is_none()) { self.row_places() } else { Vec::new() };
        let order_before = self.rows.order.clone();
        // Keys read before are stale for this document, and their `Weak`
        // would make `set` copy the page.
        self.sort_keys = None;
        self.rows.set(at, doc);
        // A header sort puts the document where its new value belongs.
        if let Some(s) = self.local_sort.take() {
            let order = sort_order(&self.keys_for(&s.path), s.dir);
            self.rows = self.rows.ordered(Some(order.clone()));
            self.local_sort = Some(LocalSort { order, ..s });
        }
        // What the documents' contents make: the columns and the JSON text.
        self.cols.cache = None;
        self.json.lines = None;
        self.json.building = None;
        if !may_move || (!removed && self.drill.is_empty() && self.rows.order.as_deref() == order_before.as_deref()) {
            // Every document on the row it was on: the selection, the Tree
            // view's heights and an open menu all still hold.
            return;
        }
        self.order_changed();
        let depth = self.drill.len();
        self.follow_drill();
        // The documents that were selected stay selected, wherever the sort
        // has put them; a document gone from the page leaves the selection.
        if self.drill.len() == depth {
            let moved = |a: usize| match removed {
                false => Some(a),
                true if a == at => None,
                true => Some(if a > at { a - 1 } else { a }),
            };
            self.reselect(&selected, anchor, &before, moved);
        }
    }

    /// For each row at the drill level: where its document is in arrival
    /// order, and which of that document's rows it is. A document's rows
    /// keep their order however the documents move, so this finds the same
    /// row again after they have.
    fn row_places(&self) -> Vec<(usize, usize)> {
        if self.drill.is_empty() {
            return (0..self.rows.len()).filter_map(|i| self.rows.arrival(i)).map(|a| (a, 0)).collect();
        }
        let rows = self.resolved();
        let mut out: Vec<(usize, usize)> = Vec::with_capacity(rows.len());
        for origin in (0..rows.len()).filter_map(|i| rows.origin(i)) {
            let a = self.rows.arrival(origin).unwrap_or(usize::MAX);
            let nth = match out.last() {
                Some(&(last, n)) if last == a => n + 1,
                _ => 0,
            };
            out.push((a, nth));
        }
        out
    }

    /// After the rows moved, select the rows showing what was selected
    /// (`selected` and `anchor`, rows then). `before` is `row_places` from
    /// then; `moved` says where a document's arrival index went (`None`: it
    /// is gone).
    fn reselect(&mut self, selected: &BTreeSet<usize>, anchor: Option<usize>, before: &[(usize, usize)], moved: impl Fn(usize) -> Option<usize>) {
        if selected.is_empty() && anchor.is_none() {
            return;
        }
        let now: HashMap<(usize, usize), usize> = self.row_places().into_iter().enumerate().map(|(row, p)| (p, row)).collect();
        let find = |row: usize| before.get(row).and_then(|&(a, n)| now.get(&(moved(a)?, n)).copied());
        self.selection.selected = selected.iter().filter_map(|&r| find(r)).collect();
        self.selection.anchor = anchor.and_then(find);
    }

    pub fn on_count(&mut self, req: RequestId, n: u64) {
        if self.paging.counting == Some(req) {
            self.paging.counting = None;
            let cap = match &self.source {
                Source::Find { max: Some(m), .. } => *m,
                _ => u64::MAX,
            };
            let total = n.min(cap);
            self.paging.total = Some(total);
            if std::mem::take(&mut self.paging.pending_last)
                && let Some(last) = self.last_offset()
                && last > self.paging.offset
            {
                self.paging.offset = last;
                self.paging.needs_fetch = true;
            }
        }
    }

    /// Offset of the last page, once the count is known.
    fn last_offset(&self) -> Option<u64> {
        let page = self.paging.page_size as u64;
        self.paging.total.filter(|t| *t > 0).map(|t| (t - 1) / page * page)
    }

    /// Last page is available whenever there are pages after this one,
    /// counted or not.
    fn can_go_last(&self) -> bool {
        self.paging.loading.is_none() && !self.paging.pending_last && (self.paging.has_more || self.last_offset().is_some_and(|l| l > self.paging.offset))
    }

    /// Go to the last page, counting the documents first when that is not
    /// known yet.
    fn go_last(&mut self, net: &mut Net) {
        if let Some(last) = self.last_offset().filter(|l| *l > self.paging.offset) {
            self.paging.offset = last;
            self.fetch(net);
        } else if self.paging.total.is_none()
            && let Source::Find { conn, spec, .. } = &self.source
        {
            self.paging.pending_last = true;
            if self.paging.counting.is_none() {
                let cmd = Command::Count { ns: spec.ns.clone(), filter: spec.filter.clone() };
                self.paging.counting = Some(net.send(Origin::View(self.id, ViewReq::Count), Some(*conn), cmd));
            }
        }
    }

    /// A request of this view was cancelled from the status bar: stop
    /// waiting for it and keep what is on screen.
    pub fn on_cancelled(&mut self, req: RequestId) {
        if self.paging.loading == Some(req) {
            self.paging.loading = None;
        }
        if self.paging.counting == Some(req) {
            self.paging.counting = None;
            self.paging.pending_last = false;
        }
        self.refreshing.retain(|(r, _)| *r != req);
    }

    pub fn on_error(&mut self, req: RequestId, err: String) {
        if self.paging.loading == Some(req) {
            self.paging.loading = None;
            self.rows = Docs::default();
            self.rows_changed();
            self.error = Some(err);
        } else if self.paging.counting == Some(req) {
            self.paging.counting = None;
            self.paging.pending_last = false;
            self.error = Some(err);
        } else if let Some(k) = self.refreshing.iter().position(|(r, _)| *r == req) {
            // The server won't read the document on its own (a projection
            // that needs the query, such as `arr.$`): fetch the page, so
            // the row doesn't stay as it was before the write.
            self.refreshing.remove(k);
            self.refresh();
        }
    }

    /// Rows at the current drill level.
    fn resolved(&self) -> Rows<'_> {
        Rows::new(&self.rows, self.level())
    }

    /// What a drill level is resolved for, besides the drill: the count of
    /// changes to which document is on which row, the documents' content
    /// (`Docs::version`, new for a document re-read in place too), and the
    /// order of rows over them.
    fn level_key(&self) -> (u64, u64, usize) {
        let order = self.rows.order.as_ref().map_or(0, |o| o.as_ptr() as usize);
        (self.rows_gen, self.rows.version, order)
    }

    /// The current drill level (None at the top), resolved once for these
    /// rows and this drill, then kept: each frame only looks up the rows it
    /// draws.
    fn level(&self) -> Option<Arc<Level>> {
        if self.drill.is_empty() {
            return None;
        }
        let key = self.level_key();
        let mut kept = self.level.borrow_mut();
        if let Some(l) = kept.as_ref().filter(|l| l.key == key && l.drill == self.drill) {
            return Some(l.clone());
        }
        let l = Arc::new(Level::resolve(&self.rows, &self.drill, key));
        *kept = Some(l.clone());
        Some(l)
    }

    /// Column names at the current drill level (before hiding), cached. At
    /// the top level they come from the raw documents, so finding them
    /// parses nothing.
    fn columns(&mut self, resolved: &Rows) -> Vec<String> {
        self.cols.cache.get_or_insert_with(|| compute_columns(resolved)).clone()
    }
}

impl ResultView {
    fn selected_origins(&self) -> Vec<usize> {
        if self.selection.selected.is_empty() {
            return Vec::new();
        }
        // At the top level a row is its document: no need to resolve them all.
        if self.drill.is_empty() {
            return self.selection.selected.iter().copied().filter(|i| *i < self.rows.len()).collect();
        }
        let resolved = self.resolved();
        let set: BTreeSet<usize> = self.selection.selected.iter().filter_map(|i| resolved.origin(*i)).collect();
        set.into_iter().collect()
    }

    #[cfg(test)]
    fn selected_docs(&self) -> Vec<&Document> {
        self.selected_origins().into_iter().filter_map(|o| self.rows.doc(o)).collect()
    }

    /// The selected document when exactly one row is selected. Cheap for
    /// any selection size, so it can run every frame; copied only by the
    /// action that takes it.
    fn single_selected(&self) -> Option<&Document> {
        if self.selection.selected.len() != 1 {
            return None;
        }
        let row = *self.selection.selected.first()?;
        self.rows.doc(self.resolved().origin(row)?)
    }

    /// The selected documents' `_id`s, read from the raw documents: after
    /// ⌘A, Delete, ⌘U and F8 parse nothing.
    fn selected_ids(&self) -> Vec<Bson> {
        self.selected_origins().into_iter().filter_map(|o| self.rows.raw_doc(o).and_then(|d| raw_id(d))).collect()
    }

    /// The row the field shortcuts, Step Into and the menu under the rows
    /// act on: the anchor, while it is selected (else the first selected).
    /// That is the row last clicked or ⌘-clicked; a ⇧-click extends a range
    /// from it and leaves it where it was (click row 2, ⇧-click row 7: row 2).
    fn current_row(&self) -> Option<usize> {
        self.selection.anchor.filter(|a| self.selection.selected.contains(a)).or_else(|| self.selection.selected.first().copied())
    }

    /// Path and value of the selected cell (the last one clicked), if any.
    fn selected_field(&self) -> Option<(String, Bson)> {
        let col = self.selection.selected_col.as_deref()?;
        let row = self.current_row()?;
        let rows = self.resolved();
        let r = rows.get(row)?;
        let (path, value) = if col == "(value)" {
            (rows.path(row), r.value.clone())
        } else {
            (join(&rows.path(row), col), r.value.as_document()?.get(col)?.clone())
        };
        (!path.is_empty() && path != "_id").then_some((path, value))
    }

    fn query(&self) -> Document {
        match &self.source {
            Source::Find { spec, .. } => spec.filter.clone(),
            Source::Static { .. } | Source::Raw { .. } => Document::new(),
        }
    }

    pub fn ui(&mut self, ui: &mut Ui, net: &mut Net) -> Vec<(u64, ViewAction)> {
        // A new request replaces one still loading (fetch cancels it): a new
        // query mustn't wait for a slow old one.
        if self.paging.needs_fetch {
            self.fetch(net);
        }
        for id in std::mem::take(&mut self.rereads) {
            self.reread(net, id);
        }
        // ⌘A ⌘C's text, once built (the status bar says "Copying…" until then).
        match self.json.copy.as_ref().map(|b| b.rx.try_recv()) {
            Some(Ok(text)) => {
                self.json.copy = None;
                ui.ctx().copy_text(text.text);
            }
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => self.json.copy = None,
            _ => {}
        }
        let mut actions = Vec::new();
        // Scrolling through a big page parses row after row: start the cache
        // over now and then. Drilled into a column, every row is in use.
        if self.drill.is_empty() && self.rows.parsed() > MAX_PARSED {
            self.rows = self.rows.unparsed();
        }
        if self.json.all && ui.input(|i| i.pointer.any_pressed()) && ui.rect_contains_pointer(ui.max_rect()) {
            self.json.all = false;
        }
        let find_focused = self.json.find.is_some() && ui.memory(|m| m.has_focus(self.find_id()));
        if net.keys_enabled && (find_focused || ui.rect_contains_pointer(ui.max_rect())) {
            ui.ctx().data_mut(|d| d.insert_temp(egui::Id::new(FIND_CLAIM), true));
            if ui.input_mut(|i| i.consume_key(Modifiers::COMMAND, Key::F)) {
                self.mode = Mode::Json;
                self.json.find.get_or_insert_default().focus = true;
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
            } else if self.rows.is_empty() && self.paging.loading.is_some() {
                ui.centered_and_justified(|ui| ui.spinner());
            } else if self.rows.is_empty() {
                let r = ui.centered_and_justified(|ui| ui.weak("No documents")).response.interact(Sense::click());
                if self.source_ns().is_some() {
                    r.context_menu(|ui| self.empty_menu(ui, &mut actions));
                }
            } else {
                // The JSON text stays while the rows do (coming back to the
                // JSON view shows it at once); its find bar and select-all go.
                if self.mode != Mode::Json {
                    self.json.find = None;
                    self.json.all = false;
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

    /// Step Into Column (`cell = false`) or Step Into Cell (`cell = true`).
    fn step_into(&mut self, row: usize, col: &str, cell: bool) -> bool {
        // Whether the value is an object or an array (else nothing to step into).
        let (array, origin) = match self.resolved().get(row) {
            Some(r) => match r.value.as_document().and_then(|d| d.get(col)) {
                Some(Bson::Document(_)) => (false, r.origin),
                Some(Bson::Array(_)) => (true, r.origin),
                _ => return false,
            },
            None => return false,
        };
        let step = match (array, cell) {
            (false, false) => Step::Field(col.to_string()),
            (array, _) => {
                let Some(doc) = self.doc_key(origin) else { return false };
                let field = col.to_string();
                if array { Step::Elements { row, doc, field } } else { Step::Cell { row, doc, field } }
            }
        };
        let mut d = self.drill.clone();
        d.push(step);
        self.set_drill(d);
        true
    }

    /// The direction the rows are sorted by `path`, if they are: by a header
    /// sort, or else by the query's own sort.
    fn sorted_by(&self, path: &str) -> Option<i32> {
        if let Some(s) = &self.local_sort {
            return (s.path == path).then_some(s.dir);
        }
        match &self.source {
            Source::Find { spec, .. } => match spec.sort.as_ref()?.get(path)? {
                Bson::Int32(n) => Some(n.signum()),
                Bson::Int64(n) => Some(n.signum() as i32),
                Bson::Double(f) => Some(if *f < 0.0 { -1 } else { 1 }),
                _ => None,
            },
            Source::Static { .. } | Source::Raw { .. } => None,
        }
    }

    /// The sort key at `path` of each document in hand: a query's page as
    /// it arrived, or a static result's documents. Read once, then kept
    /// while the documents and the path stay the same, so the next click
    /// (descending, or back on) only sorts.
    fn keys_for(&mut self, path: &str) -> Arc<[SortKey]> {
        let (from, n) = match &self.source {
            Source::Find { .. } => (Arc::downgrade(self.rows.raw()), self.rows.len()),
            Source::Raw { docs, .. } => (Arc::downgrade(docs), docs.len()),
            Source::Static { docs, .. } => (std::sync::Weak::new(), docs.len()),
        };
        // One key per document: keys of another length were read from
        // other documents (a static result swapped in place).
        if let Some(k) = &self.sort_keys
            && k.path == path
            && k.from.ptr_eq(&from)
            && k.keys.len() == n
        {
            return k.keys.clone();
        }
        let keys = match &self.source {
            Source::Find { .. } => sort_keys(self.rows.raw().iter().map(|d| SortKey::of(lookup_raw(d, path)))),
            Source::Raw { docs, .. } => sort_keys(docs.iter().map(|d| SortKey::of(lookup_raw(d, path)))),
            Source::Static { docs, .. } => sort_keys(docs.iter().map(|d| SortKey::of_bson(lookup(d, path)))),
        };
        self.sort_keys = Some(SortKeys { path: path.to_string(), from, keys: keys.clone() });
        keys
    }

    /// Sort by one field, or (`dir = None`) go back to the order the rows
    /// came in. Always done here, on the documents in hand: a query's page
    /// is sorted as it stands and nothing is asked of the server (that is
    /// `sort_query`). Hidden columns and the drill level stay as they are.
    fn sort_by(&mut self, path: String, dir: Option<i32>) {
        self.local_sort = match dir {
            Some(dir) => {
                let order = sort_order(&self.keys_for(&path), dir);
                Some(LocalSort { path, dir, order })
            }
            None => None,
        };
        match &self.source {
            Source::Find { .. } => {
                // The page stays as it is; only the order of its rows changes.
                let order = self.local_sort.as_ref().map(|s| s.order.clone());
                if self.rows.order.as_deref() != order.as_deref() {
                    // The same row numbers are other documents now.
                    self.selection.clear();
                }
                self.rows = self.rows.ordered(order);
                self.order_changed();
                self.follow_drill();
            }
            Source::Static { .. } | Source::Raw { .. } => {
                self.paging.offset = 0;
                self.refresh();
            }
        }
    }

    /// Re-run the query sorted by one field, from its first page: the whole
    /// collection in that order, which the server works out.
    fn sort_query(&mut self, path: String, dir: i32, actions: &mut Vec<ViewAction>) {
        let Source::Find { spec, .. } = &mut self.source else { return };
        spec.sort = Some(doc! { path: dir });
        actions.push(ViewAction::SortChanged(spec.sort.clone().unwrap_or_default()));
        self.local_sort = None;
        self.paging.offset = 0;
        self.refresh();
    }

    #[cfg(test)]
    pub fn find_open(&self) -> bool {
        self.json.find.is_some() && self.mode == Mode::Json
    }

    /// The documents shown, row by row, once a page is in (for app tests).
    #[cfg(test)]
    pub fn shown(&self) -> Option<Vec<Document>> {
        (self.paging.loading.is_none() && !self.paging.needs_fetch).then(|| (0..self.rows.len()).filter_map(|i| self.rows.doc(i).cloned()).collect())
    }

    #[cfg(test)]
    pub fn select_rows(&mut self, rows: &[usize]) {
        self.selection.selected = rows.iter().copied().collect();
        self.selection.anchor = rows.last().copied();
    }

    #[cfg(test)]
    pub fn selected_rows(&self) -> Vec<usize> {
        self.selection.selected.iter().copied().collect()
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod perf;
