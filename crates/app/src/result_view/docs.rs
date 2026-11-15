//! The page of documents a view shows, kept as raw BSON.

use std::sync::Arc;

use rongo_core::bson::{Bson, Document, RawDocument, RawDocumentBuf};

/// Where a page's documents (in arrival order) are in the whole result.
#[derive(Clone, Debug)]
pub(super) enum Places {
    /// A query's page: the server's documents from this offset on.
    From(usize),
    /// A page of a static result: which of its documents, in ascending
    /// order (as `page_order` picks them).
    Listed(Vec<usize>),
}

impl Places {
    /// The place of the page's document at arrival index `a`.
    pub(super) fn of(&self, a: usize) -> usize {
        match self {
            Places::From(start) => start + a,
            Places::Listed(own) => own.get(a).copied().unwrap_or(usize::MAX),
        }
    }

    /// The arrival index of the document at `place`, if it is on the page.
    pub(super) fn find(&self, place: usize, len: usize) -> Option<usize> {
        match self {
            Places::From(start) => place.checked_sub(*start).filter(|a| *a < len),
            Places::Listed(own) => own.binary_search(&place).ok(),
        }
    }
}

/// Parsed documents kept at once before the cache starts over. Scrolling
/// parses rows as they come into view.
pub(super) const MAX_PARSED: usize = 500;

/// A page of documents, kept as raw BSON and parsed one at a time when first
/// used. Parsed, nested documents take up to twenty times their raw size, and
/// most of a 50,000-document page is never on screen. Clones share the raw
/// bytes and the parsed cache.
///
/// The documents stay in the order they arrived. A sort made here only lays
/// an order of rows over them, so sorting and unsorting move no bytes and
/// keep what was parsed. Row numbers (`get`, `doc`, `Row::origin`) are in
/// that order; `raw` is in arrival order.
#[derive(Clone, Default)]
pub(super) struct Docs {
    inner: Arc<DocsInner>,
    /// For each row, the index of its document in arrival order (a
    /// permutation); `None`: the rows are in arrival order.
    pub(super) order: Option<Arc<[usize]>>,
    /// Which content this is: new for every page made and every document
    /// replaced or removed in place (`set`), never reused, so what is
    /// derived from the documents (a drill level) knows when to be made
    /// again. An order of rows laid over them keeps it.
    pub(super) version: u64,
}

/// The next `Docs::version`.
fn docs_version() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

#[derive(Default)]
struct DocsInner {
    raw: Arc<Vec<RawDocumentBuf>>,
    parsed: Vec<std::sync::OnceLock<Box<Bson>>>,
    count: std::sync::atomic::AtomicUsize,
}

impl Docs {
    pub(super) fn new(raw: Vec<RawDocumentBuf>) -> Self {
        Self::shared(Arc::new(raw))
    }

    pub(super) fn shared(raw: Arc<Vec<RawDocumentBuf>>) -> Self {
        let parsed = (0..raw.len()).map(|_| std::sync::OnceLock::new()).collect();
        Self { inner: Arc::new(DocsInner { raw, parsed, count: Default::default() }), order: None, version: docs_version() }
    }

    /// A document that cannot be written as BSON (a NUL in a key) stays as
    /// an empty row, so the rows still line up with `docs`.
    pub(super) fn from_docs<'a>(docs: impl IntoIterator<Item = &'a Document>) -> Self {
        Self::new(docs.into_iter().map(|d| RawDocumentBuf::try_from(d).unwrap_or_else(|_| RawDocumentBuf::new())).collect())
    }

    pub(super) fn len(&self) -> usize {
        self.inner.raw.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.inner.raw.is_empty()
    }

    /// Where row `i`'s document is in arrival order.
    pub(super) fn arrival(&self, i: usize) -> Option<usize> {
        match &self.order {
            Some(order) => order.get(i).copied(),
            None => (i < self.len()).then_some(i),
        }
    }

    /// The row the document at arrival index `a` is on.
    pub(super) fn row_of(&self, a: usize) -> Option<usize> {
        match &self.order {
            Some(order) => order.iter().position(|&x| x == a),
            None => (a < self.len()).then_some(a),
        }
    }

    /// Row `i`'s document as it arrived, unparsed.
    pub(super) fn raw_doc(&self, i: usize) -> Option<&RawDocumentBuf> {
        self.inner.raw.get(self.arrival(i)?)
    }

    pub(super) fn get(&self, i: usize) -> Option<&Bson> {
        let a = self.arrival(i)?;
        let cell = self.inner.parsed.get(a)?;
        Some(cell.get_or_init(|| {
            self.inner.count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Box::new(match Document::try_from(self.inner.raw[a].as_ref()) {
                Ok(d) => Bson::Document(d),
                Err(e) => Bson::String(format!("(unreadable BSON: {e})")),
            })
        }))
    }

    pub(super) fn doc(&self, i: usize) -> Option<&Document> {
        self.get(i)?.as_document()
    }

    /// The documents in the order they arrived.
    pub(super) fn raw(&self) -> &Arc<Vec<RawDocumentBuf>> {
        &self.inner.raw
    }

    /// How many documents have been parsed.
    pub(super) fn parsed(&self) -> usize {
        self.inner.count.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The same documents with the rows in `order` (`None`: as they
    /// arrived). Nothing is copied, and what was parsed stays parsed.
    pub(super) fn ordered(&self, order: Option<Arc<[usize]>>) -> Self {
        Self { inner: self.inner.clone(), order, version: self.version }
    }

    /// The same rows with nothing parsed yet (the same content).
    pub(super) fn unparsed(&self) -> Self {
        Self { version: self.version, ..Self::shared(self.inner.raw.clone()).ordered(self.order.clone()) }
    }

    /// Where the document with this `_id` is in arrival order.
    pub(super) fn find(&self, id: &Bson) -> Option<usize> {
        self.inner.raw.iter().position(|d| raw_id(d).as_ref() == Some(id))
    }

    /// Replace the document at arrival index `a`, or remove it for `None`.
    /// The other rows keep their order and what was parsed of them. Done
    /// in place, copying no other document, unless something else still
    /// holds the page (a JSON text being built from it, or a `Weak` to its
    /// documents): then on a copy of the page, as before.
    pub(super) fn set(&mut self, a: usize, doc: Option<RawDocumentBuf>) {
        if a >= self.len() {
            return;
        }
        if Arc::get_mut(&mut self.inner).and_then(|inner| Arc::get_mut(&mut inner.raw)).is_none() {
            *self = Self::new((*self.inner.raw).clone()).ordered(self.order.clone());
        }
        let Some(inner) = Arc::get_mut(&mut self.inner) else { return };
        let Some(raw) = Arc::get_mut(&mut inner.raw) else { return };
        if inner.parsed[a].get().is_some() {
            inner.count.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
        match doc {
            Some(d) => {
                raw[a] = d;
                inner.parsed[a] = std::sync::OnceLock::new();
            }
            None => {
                raw.remove(a);
                inner.parsed.remove(a);
                self.order = self.order.as_ref().map(|o| o.iter().filter(|&&i| i != a).map(|&i| if i > a { i - 1 } else { i }).collect());
            }
        }
        self.version = docs_version();
    }
}

/// A raw document's `_id`, read without parsing the rest.
pub(super) fn raw_id(d: &RawDocument) -> Option<Bson> {
    d.get("_id").ok().flatten().and_then(|v| Bson::try_from(v).ok())
}
