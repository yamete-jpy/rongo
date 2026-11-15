//! Drilling into nested values: the steps, and the rows a drill level shows.

use std::sync::Arc;

use rongo_core::bson::{Bson, RawDocumentBuf};
use rongo_core::format;

use super::docs::Docs;

/// One level of drilling into nested values.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Step {
    /// The same field of every row (Step Into Column on objects).
    Field(String),
    /// The elements of one row's array field. `row` is among the rows of
    /// the level above, and moves with `doc` (see `follow_drill`).
    Elements { row: usize, doc: DocKey, field: String },
    /// One row's object field (Step Into Cell on objects).
    Cell { row: usize, doc: DocKey, field: String },
}

/// The document a row step drills into, to find it again when the rows
/// change under the drill: its place in the whole result, and its `_id`,
/// which finds it on a query's page fetched again.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct DocKey {
    pub(super) place: usize,
    pub(super) id: Option<Bson>,
}

static NULL: Bson = Bson::Null;

/// A row at the current drill level. Its dotted path from the document
/// root is `Rows::path`, made only when asked for.
#[derive(Clone, Copy)]
pub(super) struct Row<'a> {
    /// Row number of the document it came from (see `Docs`).
    pub(super) origin: usize,
    pub(super) value: &'a Bson,
}

pub(super) fn join(prefix: &str, field: &str) -> String {
    if prefix.is_empty() { field.to_string() } else { format!("{prefix}.{field}") }
}

/// The cell in column `col` of a row whose value is `row`: that field of
/// it, or for "(value)" the value itself, unless it is a document (whose
/// fields are columns of their own).
pub(super) fn cell_in<'a>(row: &'a Bson, col: &str) -> Option<&'a Bson> {
    if col == "(value)" { (!matches!(row, Bson::Document(_))).then_some(row) } else { row.as_document().and_then(|d| d.get(col)) }
}

pub(super) fn compute_columns(resolved: &Rows) -> Vec<String> {
    match resolved {
        // In the order the documents arrived, whatever order a header sort
        // shows them in: sorting must not move the columns.
        Rows::Top(docs) => raw_columns(docs.raw()),
        Rows::Drilled { .. } => {
            let values = || (0..resolved.len()).filter_map(|i| resolved.get(i)).map(|r| r.value);
            let mut cols = format::columns(values().filter_map(Bson::as_document));
            if values().any(|v| !matches!(v, Bson::Document(_))) {
                cols.insert(0, "(value)".to_string());
            }
            cols
        }
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
/// only those. Below it the rows are a `Level`, resolved once per change,
/// and each row's value is looked up as it is asked for.
pub(super) enum Rows<'a> {
    Top(&'a Docs),
    Drilled {
        docs: &'a Docs,
        level: Arc<Level>,
        /// The array or object a row step drilled into (see `Base`).
        container: Option<&'a Bson>,
    },
}

/// One step from a value to a value inside it.
#[derive(Clone, Debug)]
enum Seg {
    Key(String),
    Index(usize),
}

/// Where a drill level's rows come from.
#[derive(Debug)]
enum Base {
    /// The page's documents (no row step): row `i` of the level is the
    /// document on row `rows[i]`.
    Docs,
    /// The elements of one document's array (`Step::Elements`): row `i` is
    /// element `rows[i]`. `origin` is the document's row, `at` where the
    /// array is in it.
    Elements { origin: usize, at: Vec<Seg> },
    /// One document's object (`Step::Cell`): a single row.
    Cell { origin: usize, at: Vec<Seg> },
}

/// A drill level's rows as positions, resolved once for the rows and the
/// drill it was made for (`ResultView::level`). A level reaches its rows
/// through its last row step (`Base`) and then the column steps after it
/// (`fields`); the steps before that only pick the row it drilled into.
#[derive(Debug)]
pub(super) struct Level {
    /// What it was resolved for: `ResultView::level_key`, and the drill.
    pub(super) key: (u64, u64, usize),
    pub(super) drill: Vec<Step>,
    base: Base,
    /// Path from the document root to the base's values ("" for documents;
    /// an array's elements add their index).
    base_path: String,
    /// Fields stepped into after the base, in turn.
    fields: Vec<String>,
    /// For each row, its document's row (`Base::Docs`) or its element of
    /// the array, among only those where every field step meets an object.
    pub(super) rows: Vec<usize>,
}

/// `v`, stepped into `fields` in turn as the column steps do: each step
/// needs an object, and a field it lacks is null.
fn walk_fields<'a>(mut v: &'a Bson, fields: &[String]) -> Option<&'a Bson> {
    for f in fields {
        v = v.as_document()?.get(f).unwrap_or(&NULL);
    }
    Some(v)
}

/// The value at `at` inside `v`.
fn walk<'a>(mut v: &'a Bson, at: &[Seg]) -> Option<&'a Bson> {
    for s in at {
        v = match s {
            Seg::Key(k) => v.as_document()?.get(k).unwrap_or(&NULL),
            Seg::Index(i) => v.as_array()?.get(*i)?,
        };
    }
    Some(v)
}

impl Level {
    /// The level `drill` (not empty) makes of `docs`.
    pub(super) fn resolve(docs: &Docs, drill: &[Step], key: (u64, u64, usize)) -> Level {
        let row_step = drill.iter().rposition(|s| !matches!(s, Step::Field(_)));
        let start = row_step.map_or(0, |k| k + 1);
        let fields: Vec<String> = drill[start..].iter().filter_map(|s| if let Step::Field(f) = s { Some(f.clone()) } else { None }).collect();
        let (base, base_path, rows) = match row_step.map(|k| (k, &drill[k])) {
            None => {
                let rows = (0..docs.len()).filter(|&i| docs.get(i).and_then(|d| walk_fields(d, &fields)).is_some()).collect();
                (Base::Docs, String::new(), rows)
            }
            Some((k, Step::Elements { row, field, .. } | Step::Cell { row, field, .. })) => {
                let cell = matches!(drill[k], Step::Cell { .. });
                // The row stepped into, at the level above.
                let above = (k > 0).then(|| Level::resolve(docs, &drill[..k], key));
                let picked = match &above {
                    None => docs.get(*row).map(|v| (*row, v, String::new(), Vec::new())),
                    Some(l) => l.row(docs, *row).map(|(o, v)| (o, v, l.path(*row), l.at(*row))),
                };
                let found = picked.and_then(|(origin, v, path, mut at)| {
                    let inner = v.as_document()?.get(field)?;
                    at.push(Seg::Key(field.clone()));
                    let path = join(&path, field);
                    match inner {
                        Bson::Array(items) if !cell => {
                            let rows = (0..items.len()).filter(|&i| walk_fields(&items[i], &fields).is_some()).collect();
                            Some((Base::Elements { origin, at }, path, rows))
                        }
                        Bson::Document(_) if cell => {
                            let rows = walk_fields(inner, &fields).map(|_| 0).into_iter().collect();
                            Some((Base::Cell { origin, at }, path, rows))
                        }
                        _ => None,
                    }
                });
                found.unwrap_or((Base::Docs, String::new(), Vec::new()))
            }
            Some(_) => unreachable!("a row step is Elements or Cell"),
        };
        Level { key, drill: drill.to_vec(), base, base_path, fields, rows }
    }

    /// The array or object the base's rows are in, if a row step made it.
    fn container<'a>(&self, docs: &'a Docs) -> Option<&'a Bson> {
        match &self.base {
            Base::Docs => None,
            Base::Elements { origin, at } | Base::Cell { origin, at } => walk(docs.get(*origin)?, at),
        }
    }

    fn origin(&self, i: usize) -> Option<usize> {
        let b = *self.rows.get(i)?;
        Some(match &self.base {
            Base::Docs => b,
            Base::Elements { origin, .. } | Base::Cell { origin, .. } => *origin,
        })
    }

    /// Row `i`: its document's row and its value.
    fn row<'a>(&self, docs: &'a Docs, i: usize) -> Option<(usize, &'a Bson)> {
        self.row_in(docs, self.container(docs), i)
    }

    /// `row`, with the container already found.
    fn row_in<'a>(&self, docs: &'a Docs, container: Option<&'a Bson>, i: usize) -> Option<(usize, &'a Bson)> {
        let b = *self.rows.get(i)?;
        let (origin, v) = match &self.base {
            Base::Docs => (b, docs.get(b)?),
            Base::Elements { origin, .. } => (*origin, container?.as_array()?.get(b)?),
            Base::Cell { origin, .. } => (*origin, container?),
        };
        Some((origin, walk_fields(v, &self.fields)?))
    }

    /// Row `i`'s dotted path from the document root.
    fn path(&self, i: usize) -> String {
        let mut path = match (&self.base, self.rows.get(i)) {
            (Base::Elements { .. }, Some(item)) => format!("{}.{item}", self.base_path),
            _ => self.base_path.clone(),
        };
        for f in &self.fields {
            path = join(&path, f);
        }
        path
    }

    /// Where row `i`'s value is in its document.
    fn at(&self, i: usize) -> Vec<Seg> {
        let mut at = match &self.base {
            Base::Docs => Vec::new(),
            Base::Elements { at, .. } => {
                let mut at = at.clone();
                at.extend(self.rows.get(i).map(|&item| Seg::Index(item)));
                at
            }
            Base::Cell { at, .. } => at.clone(),
        };
        at.extend(self.fields.iter().map(|f| Seg::Key(f.clone())));
        at
    }
}

impl<'a> Rows<'a> {
    /// The rows of `docs` at `level` (None: the top level).
    pub(super) fn new(docs: &'a Docs, level: Option<Arc<Level>>) -> Self {
        match level {
            None => Rows::Top(docs),
            Some(level) => Rows::Drilled { docs, container: level.container(docs), level },
        }
    }

    pub(super) fn len(&self) -> usize {
        match self {
            Rows::Top(d) => d.len(),
            Rows::Drilled { level, .. } => level.rows.len(),
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(super) fn get(&self, i: usize) -> Option<Row<'a>> {
        match self {
            Rows::Top(d) => d.get(i).map(|value| Row { origin: i, value }),
            Rows::Drilled { docs, level, container } => level.row_in(docs, *container, i).map(|(origin, value)| Row { origin, value }),
        }
    }

    /// Row `i`'s document row, without looking at its value.
    pub(super) fn origin(&self, i: usize) -> Option<usize> {
        match self {
            Rows::Top(d) => (i < d.len()).then_some(i),
            Rows::Drilled { level, .. } => level.origin(i),
        }
    }

    /// Row `i`'s dotted path from the document root ("" at the top level).
    pub(super) fn path(&self, i: usize) -> String {
        match self {
            Rows::Top(_) => String::new(),
            Rows::Drilled { level, .. } => level.path(i),
        }
    }
}
