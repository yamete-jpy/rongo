use super::*;
use super::find::find_matches;
use super::json_text::json_lines;
use egui::{Color32, Event, pos2};
use rongo_core::format::{self, Style};
use std::time::Instant;

/// Clicking a column header sorts by it: ascending, descending, then
/// back to the original order. Always the rows in hand: a query's page
/// is sorted where it stands, and nothing is sent to the server (the
/// column menu re-runs the query sorted). Hidden columns stay hidden.
#[test]
fn header_click_sorts() {
    let mut h = Harness::new();
    // One frame; returns the actions and where the header `name` is drawn.
    let mut frame = |v: &mut ResultView, events: Vec<Event>| {
        let (actions, out) = h.run(v, Some(egui::vec2(1400.0, 900.0)), events);
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
    v.hidden_mut().insert("img".into());
    click_header(&mut v);
    assert_eq!(names(&v), vec![None, Some(Bson::Int32(5)), s("a"), s("b"), s("c")], "missing, then numbers, then strings");
    assert!(v.hidden().contains("img"), "sorting keeps hidden columns hidden");
    click_header(&mut v);
    assert_eq!(names(&v), vec![s("c"), s("b"), s("a"), Some(Bson::Int32(5)), None]);
    click_header(&mut v);
    assert_eq!(names(&v), vec![s("b"), s("c"), None, s("a"), Some(Bson::Int32(5))], "third click: original order");

    // A query's page, as the server sent it.
    let page = |names: &[&str]| names.iter().enumerate().map(|(i, n)| RawDocumentBuf::try_from(&doc! { "_id": i as i32, "name": *n }).unwrap()).collect::<Vec<_>>();
    let query_sort = |v: &ResultView| match &v.source {
        Source::Find { spec, .. } => spec.sort.clone(),
        _ => unreachable!(),
    };
    let mut v = find_view(page(&["b", "c", "a"]));
    let actions = click_header(&mut v);
    assert_eq!(names(&v), vec![s("a"), s("b"), s("c")], "the page is sorted where it stands");
    assert!(v.paging.loading.is_none() && !v.paging.needs_fetch, "nothing is asked of the server");
    assert_eq!(query_sort(&v), None, "the query is as it was");
    assert!(!actions.iter().any(|(_, a)| matches!(a, ViewAction::SortChanged(_))), "and so is the Sort field");

    // The sort stays on as pages turn.
    v.paging.loading = Some(8);
    v.on_page(8, page(&["z", "x", "y"]), false, 1);
    assert_eq!(names(&v), vec![s("x"), s("y"), s("z")], "the next page lands sorted");
    click_header(&mut v);
    assert_eq!(names(&v), vec![s("z"), s("y"), s("x")]);
    // A row that is gone from the collection (Refresh Document) leaves
    // the page; the rest still go back to the order they arrived in.
    let first = v.rows.doc(0).and_then(|d| d.get("_id")).cloned().unwrap();
    v.refreshing = vec![(9, first.clone())];
    v.on_doc(9, first, vec![]);
    assert_eq!(names(&v), vec![s("y"), s("x")]);
    click_header(&mut v);
    assert_eq!(names(&v), vec![s("x"), s("y")], "third click: the order the page arrived in");
    assert!(v.paging.loading.is_none() && !v.paging.needs_fetch, "still without the server");

    // The query's own sort shows on the header; a click turns it round
    // (rather than doing nothing), and the cycle ends back at the query's.
    let mut v = find_view(page(&["c", "b", "a"]));
    if let Source::Find { spec, .. } = &mut v.source {
        spec.sort = Some(doc! { "name": -1 });
    }
    click_header(&mut v);
    assert_eq!(names(&v), vec![s("a"), s("b"), s("c")], "a descending query, clicked: ascending");
    click_header(&mut v);
    click_header(&mut v);
    assert!(v.local_sort.is_none(), "and round to the query's own order");
    assert_eq!(names(&v), vec![s("c"), s("b"), s("a")]);
    assert_eq!(query_sort(&v), Some(doc! { "name": -1 }));

    // The column menu's sort is the server's: the query runs again,
    // sorted, and the Sort field shows it. A header sort gives way.
    click_header(&mut v);
    let mut actions = Vec::new();
    v.sort_query("name".into(), 1, &mut actions);
    assert_eq!(query_sort(&v), Some(doc! { "name": 1 }));
    assert!(matches!(&actions[..], [ViewAction::SortChanged(s)] if *s == doc! { "name": 1 }), "the Sort field shows it");
    assert!(v.paging.needs_fetch && v.local_sort.is_none(), "the query is re-run, in the server's order");

    // A new query starts in its own order.
    let mut v = find_view(page(&["b", "a"]));
    click_header(&mut v);
    v.set_spec(FindSpec::all(Namespace::new("d", "c"), 50));
    assert!(v.local_sort.is_none());
    v.paging.needs_fetch = false;
    v.paging.loading = Some(10);
    v.on_page(10, page(&["b", "a"]), false, 1);
    assert_eq!(names(&v), vec![s("b"), s("a")], "the new query's page is shown as it came");
}

/// Each text drawn in a frame, with its centre.
type Texts = Vec<(String, egui::Pos2)>;

/// A view drawn headlessly, frame by frame, with the requests it sends.
struct Harness {
    _rt: tokio::runtime::Runtime,
    core: rongo_core::CoreHandle,
    pending: std::collections::HashMap<RequestId, crate::net::Sent>,
    next: u64,
    ctx: egui::Context,
    /// The text last put on the clipboard.
    copied: Option<String>,
}

impl Harness {
    fn new() -> Self {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let core = rongo_core::CoreHandle::start(rt.handle().clone(), std::sync::Arc::new(|| {}));
        let ctx = egui::Context::default();
        crate::theme::install(&ctx);
        Self { _rt: rt, core, pending: Default::default(), next: 0, ctx, copied: None }
    }

    fn net(&mut self) -> Net<'_> {
        Net { core: &self.core, pending: &mut self.pending, next_view_id: &mut self.next, keys_enabled: true }
    }

    /// One frame on a screen of `size` (`None`: egui's own default): the
    /// actions, and all it output. The actions are collected from every
    /// pass, as the app does: input arrives in the first, and a pass can be
    /// redrawn (the toolbar re-measuring).
    fn run(&mut self, v: &mut ResultView, size: Option<egui::Vec2>, events: Vec<Event>) -> (Vec<(u64, ViewAction)>, egui::FullOutput) {
        let input = egui::RawInput { screen_rect: size.map(|s| egui::Rect::from_min_size(egui::Pos2::ZERO, s)), events, ..Default::default() };
        let ctx = self.ctx.clone();
        let mut actions = Vec::new();
        let mut out = ctx.run_ui(input, |ui| {
            let mut net = self.net();
            actions.extend(v.ui(ui, &mut net));
        });
        out.textures_delta.clear();
        (actions, out)
    }

    /// One frame: the actions, and each text drawn with its centre.
    fn frame(&mut self, v: &mut ResultView, events: Vec<Event>) -> (Vec<(u64, ViewAction)>, Texts) {
        let (actions, mut out) = self.run(v, Some(egui::vec2(1400.0, 900.0)), events);
        for c in std::mem::take(&mut out.platform_output.commands) {
            if let egui::OutputCommand::CopyText(t) = c {
                self.copied = Some(t);
            }
        }
        fn texts(s: &egui::Shape, out: &mut Texts) {
            match s {
                egui::Shape::Text(t) => out.push((t.galley.text().to_string(), t.pos + t.galley.rect.center().to_vec2())),
                egui::Shape::Vec(v) => v.iter().for_each(|s| texts(s, out)),
                _ => {}
            }
        }
        let mut drawn = Vec::new();
        out.shapes.iter().for_each(|s| texts(&s.shape, &mut drawn));
        (actions, drawn)
    }

    /// A primary click at `at`.
    fn click(&mut self, v: &mut ResultView, at: egui::Pos2) {
        self.frame(v, vec![Event::PointerMoved(at)]);
        let button = |pressed| Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        self.frame(v, vec![button(true)]);
        self.frame(v, vec![button(false)]);
        self.frame(v, vec![]);
    }

    /// A right click at `at`; returns the texts drawn once its menu is open.
    fn right_click(&mut self, v: &mut ResultView, at: egui::Pos2) -> Texts {
        self.frame(v, vec![Event::PointerMoved(at)]);
        let button = |pressed| Event::PointerButton { pos: at, button: egui::PointerButton::Secondary, pressed, modifiers: Modifiers::NONE };
        self.frame(v, vec![button(true)]);
        self.frame(v, vec![button(false)]);
        self.frame(v, vec![]);
        self.frame(v, vec![]).1
    }

    /// A click on the drawn text `label` (a menu item).
    fn click_text(&mut self, v: &mut ResultView, texts: &Texts, label: &str) {
        let at = texts.iter().find(|(t, _)| t.contains(label)).unwrap_or_else(|| panic!("{label} drawn")).1;
        self.click(v, at);
    }

    fn key(&mut self, v: &mut ResultView, key: Key, modifiers: Modifiers) {
        self.frame(v, vec![Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }]);
        self.frame(v, vec![]);
    }
}

/// A query's page of `{ _id: i, name }` documents.
fn named_page(names: &[&str]) -> Vec<RawDocumentBuf> {
    names.iter().enumerate().map(|(i, n)| RawDocumentBuf::try_from(&doc! { "_id": i as i32, "name": *n }).unwrap()).collect()
}

fn raw_page(docs: &[Document]) -> Vec<RawDocumentBuf> {
    docs.iter().map(|d| RawDocumentBuf::try_from(d).unwrap()).collect()
}

/// Each row's `name`, top to bottom.
fn row_names(v: &ResultView) -> Vec<String> {
    (0..v.rows.len()).map(|i| v.rows.doc(i).and_then(|d| d.get_str("name").ok()).unwrap_or("-").to_string()).collect()
}

/// A header sort leaves the columns where they are, so a second click
/// on the same spot sorts the same column the other way. The columns
/// come from the documents as they arrived (a query's page or a static
/// result), not from the order a sort shows them in.
#[test]
fn header_sort_keeps_the_columns() {
    let mut h = Harness::new();
    let docs = vec![doc! { "_id": 1, "name": "b", "x": 1 }, doc! { "_id": 2, "y": 2, "name": "a" }];
    let views = [find_view(raw_page(&docs)), ResultView::new(1, "t", Source::Static { docs, truncated: false })];
    for (which, mut v) in ["query", "static"].into_iter().zip(views) {
        h.frame(&mut v, vec![]);
        h.frame(&mut v, vec![]);
        let columns = |v: &ResultView| v.header_rects.iter().map(|(c, _)| c.clone()).collect::<Vec<_>>();
        assert_eq!(columns(&v), ["_id", "name", "x", "y"], "{which}");
        let at = v.header_rects.iter().find(|(c, _)| c == "name").map(|(_, r)| r.center()).unwrap();
        h.click(&mut v, at);
        assert_eq!(v.sorted_by("name"), Some(1), "{which}");
        assert_eq!(row_names(&v), ["a", "b"], "{which}");
        assert_eq!(columns(&v), ["_id", "name", "x", "y"], "{which}: the columns stay put");
        h.click(&mut v, at);
        assert_eq!(v.sorted_by("name"), Some(-1), "{which}: the same spot again sorts name, descending");
        assert_eq!(row_names(&v), ["b", "a"], "{which}");
    }
}

/// Refresh Document finds its row by `_id` when the reply lands, even
/// after a header sort has moved it: it never removes or replaces
/// another row, and a reply for a document no longer on the page
/// changes nothing.
#[test]
fn refreshed_document_is_found_by_id_after_a_sort() {
    let mut h = Harness::new();
    let mut v = find_view(named_page(&["f", "e", "d", "c", "b", "a"]));
    // Refresh Document on `rows[at]`; returns the request and what it carries.
    let mut refresh = |v: &mut ResultView, at: usize| {
        let mut net = h.net();
        v.refresh_doc(&mut net, at);
        let req = v.refreshing.last().expect("a request went out").0;
        match &h.pending[&req].origin {
            Origin::View(_, ViewReq::RefreshDoc(asked)) => (req, asked.clone()),
            o => panic!("{o:?}"),
        }
    };

    // Row 5 ("a"), which another client has just deleted.
    let (req, asked) = refresh(&mut v, 5);
    // Before the reply lands, a header click moves the rows: row 5 is "f" now.
    v.sort_by("name".into(), Some(1));
    assert_eq!(row_names(&v), ["a", "b", "c", "d", "e", "f"]);
    v.on_doc(req, asked, vec![]);
    assert_eq!(row_names(&v), ["b", "c", "d", "e", "f"], "the deleted document left, and only it");

    // Row 0 ("b", _id 4), changed on the server; the rows move again first.
    let (req, asked) = refresh(&mut v, 0);
    v.sort_by("name".into(), Some(-1));
    v.on_doc(req, asked, raw_page(&[doc! { "_id": 4, "name": "bb" }]));
    let ids: Vec<_> = (0..v.rows.len()).map(|i| v.rows.doc(i).and_then(|d| d.get_i32("_id").ok())).collect();
    assert_eq!(ids, [0, 1, 2, 3, 4].map(Some), "nothing else replaced or dropped");
    assert_eq!(row_names(&v), ["f", "e", "d", "c", "bb"], "the refreshed document shows its change");

    // A reply for a document that is no longer on the page.
    let (req, asked) = refresh(&mut v, 0);
    v.paging.loading = Some(50);
    v.on_page(50, raw_page(&[doc! { "_id": 100, "name": "p" }, doc! { "_id": 101, "name": "q" }]), false, 1);
    v.on_doc(req, asked, vec![]);
    assert_eq!(row_names(&v), ["q", "p"]);
}

/// A header sort moves no document bytes: the page stays as it arrived
/// and only the rows' order changes. Sorting parses no document, and
/// the keys read for the first click serve the next ones.
#[test]
fn header_sort_moves_no_documents() {
    let mut v = big_view(2_000);
    let arrived = v.rows.raw().clone();
    v.sort_by("name".into(), Some(1));
    let keys = v.sort_keys.as_ref().map(|k| k.keys.clone()).expect("keys kept");
    for dir in [Some(1), Some(-1), None, Some(1)] {
        v.sort_by("name".into(), dir);
        assert!(Arc::ptr_eq(v.rows.raw(), &arrived), "{dir:?}: the page was copied");
        assert!(v.sort_keys.as_ref().is_some_and(|k| Arc::ptr_eq(&k.keys, &keys)), "{dir:?}: the keys were read again");
    }
    assert_eq!(v.rows.parsed(), 0, "sorting parsed documents");
    // Another column, or another page, has keys of its own.
    v.sort_by("position".into(), Some(1));
    assert!(v.sort_keys.as_ref().is_some_and(|k| !Arc::ptr_eq(&k.keys, &keys)));
    assert_eq!(v.rows.doc(0).and_then(|d| d.get_i32("position").ok()), Some(0));
}

/// With everything selected, the selected `_id`s (Delete, ⌘U, F8) are
/// read without parsing a document, and ⌘C parses each in turn without
/// keeping it (nor copying them all into one array first). The text is
/// what formatting them as one array writes.
#[test]
fn bulk_actions_parse_and_keep_nothing() {
    let docs: Vec<Document> = (0..300)
        .map(|i| doc! { "_id": i, "s": format!("line {i}\nnext \"quoted\""), "n": [1.5, { "d": 2.0 }, []], "e": {}, "when": rongo_core::bson::DateTime::from_millis(i as i64) })
        .collect();
    let mut v = find_view(raw_page(&docs));
    v.select_rows(&(0..300).collect::<Vec<_>>());
    assert_eq!(v.selected_ids(), (0..300).map(Bson::Int32).collect::<Vec<_>>());
    assert_eq!(v.rows.parsed(), 0, "reading the _ids parsed documents");
    let text = v.copy_text();
    assert_eq!(v.rows.parsed(), 0, "⌘C kept parsed documents");
    let whole = Bson::Array(docs.iter().cloned().map(Bson::Document).collect());
    assert_eq!(text, format::value_to_shell_pretty(&whole, Style::Editable));
    v.select_rows(&[4]);
    assert_eq!(v.copy_text(), format::doc_to_shell(&docs[4], Style::Editable), "one document: not an array");
}

/// An open Tree view node stays with its document when a header sort
/// moves the document to another row.
#[test]
fn tree_nodes_stay_open_with_their_documents() {
    let mut h = Harness::new();
    let mut v = find_view(raw_page(&[doc! { "_id": 0, "name": "b", "only_b": 1 }, doc! { "_id": 1, "name": "a", "only_a": 1 }]));
    v.mode = Mode::Tree;
    let has = |texts: &[(String, egui::Pos2)], t: &str| texts.iter().any(|(s, _)| s.ends_with(t));
    let (_, texts) = h.frame(&mut v, vec![]);
    let first = texts.iter().find(|(s, _)| s.ends_with("(1) 0")).map(|(_, p)| *p).expect("row 1 drawn");
    h.click(&mut v, first);
    let (_, texts) = h.frame(&mut v, vec![]);
    assert!(has(&texts, "only_b") && !has(&texts, "only_a"), "document 0 is open");
    v.sort_by("name".into(), Some(1));
    h.frame(&mut v, vec![]);
    let (_, texts) = h.frame(&mut v, vec![]);
    assert!(has(&texts, "(2) 0"), "document 0 is on row 2 now");
    assert!(has(&texts, "only_b") && !has(&texts, "only_a"), "and still the open one");
}

/// `{ _id: i, name, tags: ["t<i>a", "t<i>b"], meta: { deep: { k: i } } }`.
fn tagged(i: i32, name: &str) -> Document {
    doc! { "_id": i, "name": name, "tags": [format!("t{i}a"), format!("t{i}b")], "meta": { "deep": { "k": i } } }
}

/// The values of the rows at the current drill level, as text.
fn level_texts(v: &ResultView) -> Vec<String> {
    let rows = v.resolved();
    (0..rows.len()).filter_map(|i| rows.get(i)).map(|r| format::to_shell(r.value, Style::Display)).collect()
}

/// A query's page of `tagged` documents named, in order, c a e b d.
fn tagged_page() -> Vec<Document> {
    ["c", "a", "e", "b", "d"].iter().enumerate().map(|(i, n)| tagged(i as i32, n)).collect()
}

/// A drill into one row's value follows that document, not the row
/// number, when the page comes again (⌘R) with the documents in
/// another order.
#[test]
fn a_drill_follows_its_document_when_the_page_comes_again() {
    let page = tagged_page();
    let mut v = find_view(raw_page(&page));
    assert!(v.step_into(3, "tags", false));
    assert_eq!(level_texts(&v), ["\"t3a\"", "\"t3b\""]);
    let mut again = page.clone();
    again.reverse();
    v.paging.loading = Some(20);
    v.on_page(20, raw_page(&again), false, 1);
    assert_eq!(level_texts(&v), ["\"t3a\"", "\"t3b\""], "the page came again in another order");
    assert!(matches!(v.drill.as_slice(), [Step::Elements { row: 1, .. }]), "the breadcrumb says its row now: {:?}", v.drill);
}

/// Under a header sort, a refreshed document that moves takes the
/// others with it: a drill into one of them follows it.
#[test]
fn a_drill_follows_its_document_when_a_refresh_resorts() {
    let mut h = Harness::new();
    let mut v = find_view(raw_page(&tagged_page()));
    // Sorted by name: a(1) b(3) c(0) d(4) e(2). Into b's tags, on row 1.
    v.sort_by("name".into(), Some(1));
    assert!(v.step_into(1, "tags", false));
    assert_eq!(level_texts(&v), ["\"t3a\"", "\"t3b\""]);
    // Document 1 ("a") comes back from the server as "z": b moves up to row 0.
    v.refresh_doc(&mut h.net(), 0);
    let req = *h.pending.iter().find(|(_, s)| matches!(s.origin, Origin::View(_, ViewReq::RefreshDoc(_)))).expect("Refresh Document sent").0;
    v.on_doc(req, Bson::Int32(1), raw_page(&[tagged(1, "z")]));
    assert_eq!(row_names(&v), ["b", "c", "d", "e", "z"]);
    assert_eq!(level_texts(&v), ["\"t3a\"", "\"t3b\""], "a refreshed document moved it");
}

/// A one-document write answered while a page is on its way: that page
/// may have been read before the write, so it is asked for again (as
/// the whole refresh did) rather than undoing the document's re-read
/// when it lands after it. Each page request below stands for the
/// server: one sent after the write brings the written document.
#[test]
fn a_reread_is_not_undone_by_a_page_already_on_its_way() {
    let mut h = Harness::new();
    let mut v = find_view(named_page(&["a", "b", "c"]));
    h.frame(&mut v, vec![]);
    // ⌘R: a page goes out, read before the write.
    v.fetch(&mut h.net());
    let stale = v.paging.loading.expect("a page on its way");
    // The write to document 1 ("b" → "B") is done.
    v.refresh_id(Bson::Int32(1));
    h.frame(&mut v, vec![]);
    // The document's own reply, if one was asked for, lands first...
    if let Some((req, id)) = v.refreshing.last().cloned() {
        v.on_doc(req, id, raw_page(&[doc! { "_id": 1, "name": "B" }]));
    }
    // ...then the page read before the write.
    v.on_page(stale, named_page(&["a", "b", "c"]), false, 1);
    // A page asked for since was read after the write.
    if let Some(fresh) = v.paging.loading.filter(|l| *l != stale) {
        v.on_page(fresh, raw_page(&[doc! { "_id": 0, "name": "a" }, doc! { "_id": 1, "name": "B" }, doc! { "_id": 2, "name": "c" }]), false, 1);
    }
    assert_eq!(row_names(&v), ["a", "B", "c"], "the written document shows");
}

/// A re-read the server refuses (a projection it can't apply to one
/// document, such as a positional `arr.$`) fetches the page instead of
/// leaving the row as it was before the write.
#[test]
fn a_failed_reread_fetches_the_page() {
    let mut h = Harness::new();
    let mut v = find_view(named_page(&["a", "b", "c"]));
    h.frame(&mut v, vec![]);
    v.refresh_id(Bson::Int32(1));
    h.frame(&mut v, vec![]);
    let req = v.refreshing.last().expect("a re-read went out").0;
    v.on_error(req, "positional projection does not match the query".into());
    assert!(v.refreshing.is_empty(), "no longer waited for");
    h.frame(&mut v, vec![]);
    assert!(v.paging.loading.is_some(), "the page is asked for");
    assert_eq!(v.error, None, "the page shows, not the re-read's error");
}

/// Copy › Copy _id writes text that reads back as the same `_id`: a
/// whole-number double stays a double.
#[test]
fn copy_id_reads_back_as_the_same_id() {
    let mut h = Harness::new();
    let mut v = find_view(raw_page(&[doc! { "_id": 5.0, "name": "five" }]));
    h.frame(&mut v, vec![]);
    let (_, texts) = h.frame(&mut v, vec![]);
    let cell = texts.iter().find(|(t, _)| t == "five").map(|(_, p)| *p).expect("the row drawn");
    let texts = h.right_click(&mut v, cell);
    let copy = texts.iter().find(|(t, _)| t.ends_with("  Copy")).map(|(_, p)| *p).expect("the Copy menu");
    h.frame(&mut v, vec![Event::PointerMoved(copy)]);
    let (_, texts) = h.frame(&mut v, vec![]);
    h.click_text(&mut v, &texts, "Copy _id");
    let copied = h.copied.take().expect("copied");
    assert_eq!(rongo_shell::parse_value(&copied).unwrap(), Bson::Double(5.0), "{copied}");
}

/// A refreshed document replaces its own bytes only: the page's other
/// documents are not copied and stay parsed, the selection stays, and
/// one document refreshed twice keeps only the newer reply.
#[test]
fn a_refreshed_document_copies_no_other() {
    let mut h = Harness::new();
    let mut v = big_view(1_000);
    v.select_rows(&[5, 7]);
    let other = v.rows.raw()[0].as_bytes().as_ptr();
    let page = Arc::as_ptr(v.rows.raw());
    let id = v.rows.doc(3).and_then(|d| d.get("_id")).cloned().unwrap();
    v.refresh_id(id.clone());
    h.frame(&mut v, vec![]);
    let first = v.refreshing.last().expect("asked").0;
    // The same document again before the first reply: only the newer counts.
    v.refresh_id(id.clone());
    h.frame(&mut v, vec![]);
    let second = v.refreshing.last().expect("asked").0;
    assert_eq!(v.refreshing.len(), 1);
    let reply = |name: &str| raw_page(&[doc! { "_id": id.clone(), "name": name }]);
    // Rows on screen are parsed, row 3 among them.
    let parsed = v.rows.parsed();
    v.on_doc(second, id.clone(), reply("newer"));
    v.on_doc(first, id.clone(), reply("older"));
    assert_eq!(v.rows.parsed(), parsed - 1, "what was parsed stays parsed, but for the replaced row");
    assert_eq!(v.rows.doc(3).and_then(|d| d.get_str("name").ok()), Some("newer"));
    assert!(std::ptr::eq(Arc::as_ptr(v.rows.raw()), page) && std::ptr::eq(v.rows.raw()[0].as_bytes().as_ptr(), other), "the page was copied");
    assert_eq!(v.selected_rows(), [5, 7]);
    assert!(!v.paging.needs_fetch && v.paging.loading.is_none(), "and the page is not fetched");
}

/// A cell's menu, opened on a row of the page on screen while the next
/// page loads, never acts on the new page's document on that row once
/// the page lands.
#[test]
fn a_cell_menu_never_acts_on_the_next_pages_document() {
    let mut h = Harness::new();
    let page = |from: i32| -> Vec<RawDocumentBuf> { (from..from + 5).map(|i| RawDocumentBuf::try_from(&doc! { "_id": i, "name": format!("doc{i}") }).unwrap()).collect() };
    let mut v = find_view(page(0));
    v.paging.has_more = true;
    h.frame(&mut v, vec![]);
    let (_, texts) = h.frame(&mut v, vec![]);
    let cell = texts.iter().find(|(t, _)| t == "doc2").map(|(_, p)| *p).expect("row 2 drawn");
    // Next page; while it loads, the menu of a row of this page.
    v.paging.offset += 5;
    v.fetch(&mut h.net());
    let req = v.paging.loading.expect("the next page is asked for");
    let texts = h.right_click(&mut v, cell);
    assert!(texts.iter().any(|(t, _)| t.contains("Field")), "the menu is open");
    v.on_page(req, page(100), false, 1);
    let mut actions = Vec::new();
    let (acts, texts) = h.frame(&mut v, vec![]);
    actions.extend(acts);
    let still_open = texts.iter().find(|(t, _)| t.contains("Field")).map(|(_, p)| *p);
    if let Some(field) = still_open {
        // Still open: Field › Set to null.
        h.frame(&mut v, vec![Event::PointerMoved(field)]);
        let (acts, texts) = h.frame(&mut v, vec![]);
        actions.extend(acts);
        if let Some(at) = texts.iter().find(|(t, _)| t == "Set to null").map(|(_, p)| *p) {
            h.frame(&mut v, vec![Event::PointerMoved(at)]);
            for pressed in [true, false] {
                let (acts, _) = h.frame(&mut v, vec![Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE }]);
                actions.extend(acts);
            }
        }
    }
    let targets: Vec<_> = actions.iter().filter_map(|(_, a)| match a {
        ViewAction::UpdateOne { id, .. } => Some(id.clone()),
        _ => None,
    }).collect();
    assert!(targets.iter().all(|id| *id == Bson::Int32(2)), "the menu of document 2 wrote to {targets:?}");
    assert!(still_open.is_none(), "the menu closed as the page landed");
    // A menu opened on the new page works as ever.
    let (_, texts) = h.frame(&mut v, vec![]);
    let cell = texts.iter().find(|(t, _)| t == "doc102").map(|(_, p)| *p).expect("the new row 2 drawn");
    let texts = h.right_click(&mut v, cell);
    assert!(texts.iter().any(|(t, _)| t.contains("Field")), "a new menu opens");
}

/// A refreshed document keeps the selection: under a header sort the
/// selected documents stay selected on the rows they move to, and a
/// document gone from the server leaves the selection, the rest staying.
#[test]
fn a_refreshed_document_keeps_the_selection() {
    let mut h = Harness::new();
    let mut v = find_view(raw_page(&tagged_page()));
    // Sorted by name: a(1) b(3) c(0) d(4) e(2). Select b and d, d last.
    v.sort_by("name".into(), Some(1));
    v.click_row(1, Modifiers::NONE);
    v.click_row(3, Modifiers::COMMAND);
    let mut refresh = |v: &mut ResultView, row: usize, id: i32, reply: Vec<Document>| {
        v.refresh_doc(&mut h.net(), row);
        let req = *h.pending.iter().find(|(_, s)| matches!(&s.origin, Origin::View(_, ViewReq::RefreshDoc(asked)) if *asked == Bson::Int32(id))).expect("Refresh Document sent").0;
        h.pending.remove(&req);
        v.on_doc(req, Bson::Int32(id), raw_page(&reply));
    };
    // "a" comes back as "z": b c d e z.
    refresh(&mut v, 0, 1, vec![tagged(1, "z")]);
    let selected = |v: &ResultView| v.selection.selected.iter().map(|&r| row_names(v)[r].clone()).collect::<Vec<_>>();
    assert_eq!(selected(&v), ["b", "d"], "the same documents, on their new rows");
    assert_eq!(v.selection.anchor, Some(2), "d, clicked last, on row 2 now");
    // b is deleted on the server.
    refresh(&mut v, 0, 3, vec![]);
    assert_eq!(row_names(&v), ["c", "d", "e", "z"]);
    assert_eq!(selected(&v), ["d"]);
    assert_eq!(v.selection.anchor, Some(1));
}

/// A row step under a column drill (meta, then one row's deep) follows
/// its document too, and when the document is not on the next page the
/// drill steps back out to the column drill above it.
#[test]
fn a_drill_under_a_column_follows_its_document_or_steps_out() {
    let page = tagged_page();
    let mut v = find_view(raw_page(&page));
    assert!(v.step_into(4, "meta", false));
    assert!(v.step_into(4, "deep", true));
    assert_eq!(level_texts(&v), ["{ \"k\" : 4 }"]);
    let mut again = page.clone();
    again.reverse();
    v.paging.loading = Some(21);
    v.on_page(21, raw_page(&again), false, 1);
    assert_eq!(level_texts(&v), ["{ \"k\" : 4 }"], "under a column drill");
    let next: Vec<Document> = (5..8).map(|i| tagged(i, "n")).collect();
    v.paging.loading = Some(22);
    v.on_page(22, raw_page(&next), false, 1);
    assert_eq!(v.drill, [Step::Field("meta".into())], "the next page: back out to above the document's step");
}

/// A static result's drill follows its document through page turns too:
/// the same page again keeps it, another page steps back out.
#[test]
fn a_static_drill_steps_out_when_its_document_leaves_the_page() {
    let mut h = Harness::new();
    let docs: Vec<Document> = (0..6).map(|i| tagged(i, "n")).collect();
    let mut v = ResultView::new(1, "t", Source::Static { docs, truncated: false });
    v.paging.page_size = 3;
    h.frame(&mut v, vec![]);
    assert!(v.step_into(1, "tags", false));
    v.fetch(&mut h.net());
    assert_eq!(level_texts(&v), ["\"t1a\"", "\"t1b\""], "Refresh View: the same page");
    v.paging.offset = 3;
    v.fetch(&mut h.net());
    assert!(v.drill.is_empty(), "the next page: {:?} shows {:?}", v.drill, level_texts(&v));
}

/// A header sort on a static result (or a Collection History preview)
/// over several pages: each page holds the next documents in the sort's
/// order. A preview whose one page holds them all still shares its bytes.
#[test]
fn a_sorted_static_result_pages_in_order() {
    let mut h = Harness::new();
    let docs: Vec<Document> = ["d", "b", "e", "a", "c"].iter().enumerate().map(|(i, n)| doc! { "_id": i as i32, "name": *n }).collect();
    let raw = Arc::new(raw_page(&docs));
    let views = [ResultView::new(1, "t", Source::Static { docs: docs.clone(), truncated: false }), ResultView::new(1, "t", Source::Raw { docs: raw.clone(), truncated: false })];
    for (which, mut v) in ["static", "raw"].into_iter().zip(views) {
        v.paging.page_size = 2;
        h.frame(&mut v, vec![]);
        for (dir, want) in [(1, [vec!["a", "b"], vec!["c", "d"], vec!["e"]]), (-1, [vec!["e", "d"], vec!["c", "b"], vec!["a"]])] {
            v.sort_by("name".into(), Some(dir));
            let pages: Vec<Vec<String>> = [0, 2, 4]
                .into_iter()
                .map(|offset| {
                    v.paging.offset = offset;
                    v.fetch(&mut h.net());
                    row_names(&v)
                })
                .collect();
            assert_eq!(pages, want, "{which} {dir}");
        }
    }
    let mut v = ResultView::new(1, "t", Source::Raw { docs: raw.clone(), truncated: false });
    v.paging.page_size = 10;
    v.sort_by("name".into(), Some(-1));
    h.frame(&mut v, vec![]);
    assert_eq!(row_names(&v), ["e", "d", "c", "b", "a"]);
    assert!(Arc::ptr_eq(v.rows.raw(), &raw), "one page: the preview's own bytes, sorted");
}

/// In a static result over several pages, an open Tree node stays with
/// its document: a header sort that brings other documents onto the
/// page opens none of theirs, and its own is open again when it is back.
#[test]
fn static_tree_nodes_stay_with_their_documents() {
    let mut h = Harness::new();
    let docs: Vec<Document> = ["a", "b", "c", "d"]
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let mut d = doc! { "_id": i as i32, "name": *n };
            d.insert(format!("only_{i}"), 1);
            d
        })
        .collect();
    let mut v = ResultView::new(1, "t", Source::Static { docs, truncated: false });
    v.paging.page_size = 2;
    v.mode = Mode::Tree;
    let has = |texts: &Texts, t: &str| texts.iter().any(|(s, _)| s.ends_with(t));
    h.frame(&mut v, vec![]);
    let (_, texts) = h.frame(&mut v, vec![]);
    let first = texts.iter().find(|(s, _)| s.ends_with("(1) 0")).map(|(_, p)| *p).expect("row 1 drawn");
    h.click(&mut v, first);
    let (_, texts) = h.frame(&mut v, vec![]);
    assert!(has(&texts, "only_0"), "document 0 is open");
    v.sort_by("name".into(), Some(-1));
    h.frame(&mut v, vec![]);
    let (_, texts) = h.frame(&mut v, vec![]);
    assert!(has(&texts, "(1) 3") && has(&texts, "(2) 2"), "documents 3 and 2 on the page");
    assert!(!has(&texts, "only_3") && !has(&texts, "only_2"), "neither opened in its place");
    v.sort_by("name".into(), None);
    h.frame(&mut v, vec![]);
    let (_, texts) = h.frame(&mut v, vec![]);
    assert!(has(&texts, "only_0"), "document 0, back on the page, is still open");
}

/// Sort keys read for a static result's documents are not reused for
/// other documents put in its place (`source` is public).
#[test]
fn sort_keys_are_read_again_for_other_static_documents() {
    let mut h = Harness::new();
    let mut v = ResultView::new(1, "t", Source::Static { docs: vec![doc! { "name": "b" }, doc! { "name": "a" }], truncated: false });
    h.frame(&mut v, vec![]);
    v.sort_by("name".into(), Some(1));
    h.frame(&mut v, vec![]);
    v.source = Source::Static { docs: ["c", "b", "d", "a"].iter().map(|n| doc! { "name": *n }).collect(), truncated: false };
    v.sort_by("name".into(), Some(1));
    h.frame(&mut v, vec![]);
    assert_eq!(row_names(&v), ["a", "b", "c", "d"]);
}

/// The ⇧-click anchor goes wherever the selection goes: a page turn or
/// a step into a value forgets both. Hidden columns are kept per drill
/// level, as the column order is: stepping out shows the level as it was.
#[test]
fn anchor_and_hidden_columns_keep_to_their_level() {
    let mut h = Harness::new();
    let docs: Vec<Document> = (0..30).map(|i| doc! { "_id": i, "img": i, "meta": { "a": i, "b": i } }).collect();
    let mut v = ResultView::new(1, "t", Source::Static { docs, truncated: false });
    v.paging.page_size = 10;
    h.frame(&mut v, vec![]);
    v.click_row(8, Modifiers::NONE);
    v.paging.offset += 10;
    v.fetch(&mut h.net());
    v.click_row(2, Modifiers::SHIFT);
    assert_eq!(v.selection.selected, BTreeSet::from([2]), "a page turn forgets the anchor");
    v.click_row(5, Modifiers::NONE);
    assert!(v.step_into(5, "meta", false));
    v.click_row(1, Modifiers::SHIFT);
    assert_eq!(v.selection.selected, BTreeSet::from([1]), "so does stepping in");

    v.set_drill(Vec::new());
    v.hidden_mut().insert("img".into());
    assert!(v.step_into(0, "meta", false));
    assert!(v.hidden().is_empty(), "a level of its own");
    v.hidden_mut().insert("b".into());
    h.key(&mut v, Key::Escape, Modifiers::NONE);
    assert!(v.drill.is_empty());
    assert_eq!(v.hidden(), &BTreeSet::from(["img".to_string()]), "stepping out shows what it hid");
    assert!(v.step_into(0, "meta", false));
    assert_eq!(v.hidden(), &BTreeSet::from(["b".to_string()]), "and stepping back in, too");
}

/// Field shortcuts, Step Into and the menu under the rows act on the row
/// last ⌘-clicked (the anchor), not on the lowest selected one.
#[test]
fn shortcuts_act_on_the_row_last_clicked() {
    let mut h = Harness::new();
    let docs: Vec<Document> = (0..8).map(|i| doc! { "_id": i, "price": i * 10, "meta": { "k": i } }).collect();
    let mut v = find_view(raw_page(&docs));
    h.frame(&mut v, vec![]);
    v.click_row(2, Modifiers::NONE);
    v.click_row(7, Modifiers::COMMAND);
    v.selection.selected_col = Some("price".into());
    assert_eq!(v.selected_field(), Some(("price".to_string(), Bson::Int32(70))), "⌘U fills in row 7's price");

    let texts = h.right_click(&mut v, pos2(600.0, 700.0));
    h.click_text(&mut v, &texts, "Refresh Document");
    let asked: Vec<_> = h.pending.values().filter_map(|s| match &s.origin {
        Origin::View(_, ViewReq::RefreshDoc(id)) => Some(id.clone()),
        _ => None,
    }).collect();
    assert_eq!(asked, [Bson::Int32(7)], "the menu under the rows is row 7's");

    v.selection.selected_col = Some("meta".into());
    h.key(&mut v, Key::Enter, Modifiers::SHIFT);
    assert!(matches!(v.drill.as_slice(), [Step::Cell { row: 7, .. }]), "⇧↩ steps into row 7: {:?}", v.drill);
}

/// After a header sort, everything that reads rows agrees on their
/// order: the table, the tree, the JSON text, selection, copy and
/// drilling in.
#[test]
fn a_sorted_page_reads_the_same_everywhere() {
    let mut h = Harness::new();
    let docs = [
        doc! { "_id": 1, "name": "bravo", "meta": { "k": "B" } },
        doc! { "_id": 2, "name": "charlie", "meta": { "k": "C" } },
        doc! { "_id": 3, "name": "alpha", "meta": { "k": "A" } },
    ];
    let mut v = find_view(raw_page(&docs));
    v.sort_by("name".into(), Some(1));
    let top_to_bottom = |texts: &[(String, egui::Pos2)], want: &[&str]| {
        let ys: Vec<f32> = want.iter().map(|w| texts.iter().find(|(s, _)| s.ends_with(w)).unwrap_or_else(|| panic!("{w} drawn")).1.y).collect();
        assert!(ys.windows(2).all(|p| p[0] < p[1]), "{want:?} at {ys:?}");
    };
    h.frame(&mut v, vec![]);
    let (_, texts) = h.frame(&mut v, vec![]);
    top_to_bottom(&texts, &["alpha", "bravo", "charlie"]);

    v.selection.selected = BTreeSet::from([0]);
    assert_eq!(v.selected_ids(), [Bson::Int32(3)]);
    assert!(v.copy_text().contains("\"alpha\""), "{}", v.copy_text());
    assert!(v.selection_text(',', false).starts_with("3,alpha"), "{}", v.selection_text(',', false));

    v.mode = Mode::Tree;
    h.frame(&mut v, vec![]);
    let (_, texts) = h.frame(&mut v, vec![]);
    top_to_bottom(&texts, &["(1) 3", "(2) 1", "(3) 2"]);

    v.mode = Mode::Json;
    h.frame(&mut v, vec![]);
    while v.json.building.is_some() {
        std::thread::sleep(std::time::Duration::from_millis(5));
        h.frame(&mut v, vec![]);
    }
    let text = &v.json.lines.as_ref().expect("JSON text built").text;
    let at = |w: &str| text.find(w).unwrap_or_else(|| panic!("{w} in the JSON text"));
    assert!(at("alpha") < at("bravo") && at("bravo") < at("charlie"), "{text}");

    v.mode = Mode::Table;
    v.set_drill(vec![Step::Field("meta".into())]);
    let rows = v.resolved();
    let ks: Vec<_> = (0..rows.len()).filter_map(|i| rows.get(i)).map(|r| r.value.as_document().and_then(|d| d.get_str("k").ok()).unwrap_or("-").to_string()).collect();
    assert_eq!(ks, ["A", "B", "C"]);
}

/// A query running for more than a moment shows in the view's own status
/// line (not in a bar across the window): what it is, a moving bar, the
/// time, and ⊗, which cancels it and keeps the rows on screen.
#[test]
fn slow_query_shows_in_the_status_line() {
    let mut h = Harness::new();
    let mut v = big_view(5);
    v.paging.loading = Some(77);
    let started = std::time::Instant::now() - std::time::Duration::from_millis(2300);
    h.pending.insert(77, crate::net::Sent { what: Some("Running find on d.c".into()), started, ..crate::net::Sent::new(Origin::View(1, ViewReq::Page)) });
    let mut frame = |v: &mut ResultView, events: Vec<Event>| {
        let (_, out) = h.run(v, Some(egui::vec2(1400.0, 900.0)), events);
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
    // Started 2.3 s ago; a loaded machine may draw it later, so "at least 2s".
    assert!(texts.iter().any(|(t, _)| t.strip_suffix('s').and_then(|n| n.parse::<u64>().ok()).is_some_and(|n| n >= 2)), "time taken: {texts:?}");
    let x = texts.iter().filter(|(t, p)| t == icon::X_CIRCLE && (p.y - selected.y).abs() < 6.0).map(|(_, p)| *p).next().expect("⊗ on the status line");
    let press = |pressed| Event::PointerButton { pos: x, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
    frame(&mut v, vec![Event::PointerMoved(x), press(true)]);
    frame(&mut v, vec![press(false)]);
    assert_eq!((v.paging.loading, v.rows.len()), (None, 5), "cancelled, rows kept");
    let texts = frame(&mut v, vec![]);
    assert!(!texts.iter().any(|(t, _)| t.starts_with("Running find")), "gone: {texts:?}");
}

/// Right-click in the empty space under the rows opens a menu, as in
/// Studio 3T: the selected row's menu when a row is selected, otherwise
/// the one for the whole result (Insert, Refresh View, Export…).
#[test]
fn right_click_under_the_rows() {
    let mut h = Harness::new();
    let mut frame = |v: &mut ResultView, events: Vec<Event>| {
        let (_, out) = h.run(v, Some(egui::vec2(1400.0, 900.0)), events);
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

    v.selection.selected = BTreeSet::from([1]);
    v.selection.selected_col = Some("name".into());
    let texts = right_click(&mut v, &mut frame);
    assert!(has(&texts, "Field") && has(&texts, "Copy") && has(&texts, "Refresh Document"), "the selected row's menu: {texts:?}");
    assert_eq!(v.selection.selected, BTreeSet::from([1]), "the selection stays");
}

/// Dragging over rows selects them, from the row where the drag began
/// to the row under the pointer; ⌘ keeps what was already selected;
/// past the bottom the table scrolls and the selection follows. A
/// plain click still picks one row.
#[test]
fn dragging_over_rows_selects_them() {
    use std::collections::HashMap;
    let mut h = Harness::new();
    let screen = egui::vec2(1000.0, 500.0);
    // One frame; returns where each visible "n<k>" cell is drawn.
    let mut frame = |v: &mut ResultView, events: Vec<Event>| {
        let (_, out) = h.run(v, Some(screen), events);
        let mut cells = HashMap::new();
        for s in &out.shapes {
            if let egui::Shape::Text(t) = &s.shape
                && let Some(k) = t.galley.text().strip_prefix('n').and_then(|k| k.parse::<usize>().ok())
                && s.clip_rect.contains(t.pos + t.galley.rect.center().to_vec2())
            {
                cells.insert(k, t.pos + t.galley.rect.center().to_vec2());
            }
        }
        cells
    };
    let docs: Vec<Document> = (0..40).map(|i| doc! { "_id": i, "name": format!("n{i}") }).collect();
    let mut v = ResultView::new(1, "t", Source::Static { docs, truncated: false });
    for _ in 0..3 {
        frame(&mut v, vec![]);
    }
    let cells = frame(&mut v, vec![]);
    type Frame<'a> = dyn FnMut(&mut ResultView, Vec<Event>) -> HashMap<usize, egui::Pos2> + 'a;
    fn button(pos: egui::Pos2, pressed: bool, modifiers: Modifiers) -> Event {
        Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers }
    }
    fn drag(frame: &mut Frame, v: &mut ResultView, from: egui::Pos2, to: egui::Pos2, m: Modifiers) {
        frame(v, vec![Event::ModifiersChanged(m), Event::PointerMoved(from)]);
        frame(v, vec![button(from, true, m)]);
        for k in 1..=5 {
            frame(v, vec![Event::PointerMoved(from + (to - from) * (k as f32 / 5.0))]);
        }
    }
    fn release(frame: &mut Frame, v: &mut ResultView, at: egui::Pos2, m: Modifiers) {
        frame(v, vec![button(at, false, m), Event::ModifiersChanged(Modifiers::NONE)]);
        frame(v, vec![]);
    }

    // n3 down to n7.
    drag(&mut frame, &mut v, cells[&3], cells[&7], Modifiers::NONE);
    assert_eq!(v.selection.selected, (3..=7).collect(), "while dragging");
    release(&mut frame, &mut v, cells[&7], Modifiers::NONE);
    assert_eq!(v.selection.selected, (3..=7).collect(), "after letting go");
    assert_eq!(v.selection.anchor, Some(3), "⇧-click extends from where the drag began");
    assert_eq!(v.selection.selected_col.as_deref(), Some("name"), "the cell it began on is the active one");

    // Upwards works too, and ⌘ adds to what is selected.
    drag(&mut frame, &mut v, cells[&12], cells[&10], Modifiers::COMMAND);
    release(&mut frame, &mut v, cells[&10], Modifiers::COMMAND);
    assert_eq!(v.selection.selected, (3..=7).chain(10..=12).collect());

    // A click is still a click.
    frame(&mut v, vec![Event::PointerMoved(cells[&1])]);
    frame(&mut v, vec![button(cells[&1], true, Modifiers::NONE)]);
    frame(&mut v, vec![button(cells[&1], false, Modifiers::NONE)]);
    assert_eq!(v.selection.selected, BTreeSet::from([1]));

    // Held below the table, it scrolls and keeps selecting to the end.
    let last_seen = *cells.keys().max().unwrap();
    assert!(last_seen < 39, "the window shows only some rows: up to n{last_seen}");
    let below = pos2(cells[&0].x, screen.y - 2.0);
    drag(&mut frame, &mut v, cells[&0], below, Modifiers::NONE);
    for _ in 0..240 {
        frame(&mut v, vec![Event::PointerMoved(below)]);
    }
    assert_eq!(v.selection.selected, (0..40).collect(), "scrolled to the last row");
    release(&mut frame, &mut v, below, Modifiers::NONE);
    assert!(frame(&mut v, vec![]).contains_key(&39), "the last row is in view");
}

/// A drag can start in the empty space under the rows. A dashed box
/// runs from where it began to the pointer, every row it touches is
/// selected, and the box goes away when the button comes up.
#[test]
fn dragging_from_empty_space_draws_a_box() {
    use std::collections::HashMap;
    let mut h = Harness::new();
    // One frame; returns where each "n<k>" cell is and how many dashes are drawn.
    let mut frame = |v: &mut ResultView, events: Vec<Event>| {
        let (_, out) = h.run(v, Some(egui::vec2(900.0, 600.0)), events);
        let mut cells = HashMap::new();
        let mut dashes = 0;
        for s in &out.shapes {
            match &s.shape {
                egui::Shape::Text(t) => {
                    if let Some(k) = t.galley.text().strip_prefix('n').and_then(|k| k.parse::<usize>().ok()) {
                        cells.insert(k, t.pos + t.galley.rect.center().to_vec2());
                    }
                }
                egui::Shape::LineSegment { stroke, .. } if stroke.color == theme::ACCENT => dashes += 1,
                _ => {}
            }
        }
        (cells, dashes)
    };
    let docs: Vec<Document> = (0..5).map(|i| doc! { "_id": i, "name": format!("n{i}") }).collect();
    let mut v = ResultView::new(1, "t", Source::Static { docs, truncated: false });
    for _ in 0..3 {
        frame(&mut v, vec![]);
    }
    let (cells, dashes) = frame(&mut v, vec![]);
    assert_eq!(dashes, 0, "no box before a drag");
    let button = |pos, pressed| Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
    let empty = cells[&4] + vec2(0.0, 120.0);

    // From the empty space up to n2.
    frame(&mut v, vec![Event::PointerMoved(empty)]);
    frame(&mut v, vec![button(empty, true)]);
    let mut dashes = 0;
    for k in 1..=5 {
        dashes = frame(&mut v, vec![Event::PointerMoved(empty + (cells[&2] - empty) * (k as f32 / 5.0))]).1;
    }
    assert!(dashes >= 8, "a dashed box while dragging: {dashes} dashes");
    assert_eq!(v.selection.selected, BTreeSet::from([2, 3, 4]));
    frame(&mut v, vec![button(cells[&2], false)]);
    assert_eq!(frame(&mut v, vec![]).1, 0, "the box goes when the button comes up");
    assert_eq!(v.selection.selected, BTreeSet::from([2, 3, 4]), "the rows stay selected");

    // A box that touches no row selects nothing.
    frame(&mut v, vec![Event::PointerMoved(empty)]);
    frame(&mut v, vec![button(empty, true)]);
    for k in 1..=5 {
        frame(&mut v, vec![Event::PointerMoved(empty + vec2(30.0 * k as f32, 10.0 * k as f32))]);
    }
    frame(&mut v, vec![button(empty + vec2(150.0, 50.0), false)]);
    assert!(v.selection.selected.is_empty(), "{:?}", v.selection.selected);
}

/// Scrolling a table to places it hasn't been before doesn't keep
/// adding memory. (egui remembers every widget that can take keyboard
/// focus until the arrow keys move focus; table cells used to be such
/// widgets, and a cell's id changes with the first row on screen, so
/// each new scroll position added a few thousand entries.) Release only:
/// debug builds of egui also record every id they see.
#[test]
#[cfg_attr(debug_assertions, ignore = "egui records every id in debug builds; run with --release")]
fn scrolling_does_not_keep_adding_memory() {
    let mut h = Harness::new();
    let docs: Vec<Document> = (0..300)
        .map(|i| {
            let mut d = doc! { "_id": rongo_core::bson::oid::ObjectId::new(), "name": format!("player {i}") };
            for k in 0..20 {
                let v = match k % 4 {
                    0 => Bson::String(format!("value {i}.{k}")),
                    1 => Bson::Int64(i * k),
                    2 => Bson::Document(doc! { "a": k, "b": [1, 2, 3] }),
                    _ => Bson::Array(vec![Bson::Int32(1), Bson::String("x".into())]),
                };
                d.insert(format!("f{k}"), v);
            }
            d
        })
        .collect();
    let mut v = ResultView::new(1, "t", Source::Static { docs, truncated: false });
    let mut scroll = |v: &mut ResultView, dy: f32| {
        let wheel = Event::MouseWheel { unit: egui::MouseWheelUnit::Point, delta: vec2(0.0, dy), phase: egui::TouchPhase::Move, modifiers: Modifiers::NONE };
        h.run(v, Some(vec2(3000.0, 800.0)), vec![Event::PointerMoved(pos2(900.0, 400.0)), wheel]);
    };
    // Every row once (parsed and laid out), down and back up.
    for dy in [-60.0, 60.0] {
        for _ in 0..140 {
            scroll(&mut v, dy);
        }
    }
    let before = crate::heap::live();
    // The same rows again, stopping at new places.
    for dy in [-7.0, 7.0] {
        for _ in 0..900 {
            scroll(&mut v, dy);
        }
    }
    let grown = crate::heap::live() - before;
    assert!(grown < 100 * 1024, "scrolling added {} KiB", grown / 1024);
}

/// Tab never stops on a table cell: cells don't take keyboard focus,
/// which is what keeps scrolling from adding memory
/// (scrolling_does_not_keep_adding_memory).
#[test]
fn tab_skips_the_table_cells() {
    let mut h = Harness::new();
    let ctx = h.ctx.clone();
    let docs: Vec<Document> = (0..20).map(|i| doc! { "_id": i, "name": format!("player {i}") }).collect();
    let mut v = ResultView::new(1, "t", Source::Static { docs, truncated: false });
    let mut frame = |v: &mut ResultView, events: Vec<Event>| {
        let (_, out) = h.run(v, Some(vec2(900.0, 600.0)), events);
        // Where the rows are: from the first to the last "player" cell.
        out.shapes.iter().filter_map(|s| match &s.shape {
            egui::Shape::Text(t) if t.galley.text().starts_with("player ") => Some(t.galley.rect.translate(t.pos.to_vec2())),
            _ => None,
        }).reduce(|a, b| a.union(b))
    };
    frame(&mut v, vec![]);
    let rows = frame(&mut v, vec![]).expect("rows drawn");
    let tab = Event::Key { key: egui::Key::Tab, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE };
    let mut stops = 0;
    for _ in 0..40 {
        frame(&mut v, vec![tab.clone()]);
        if let Some(r) = ctx.memory(|m| m.focused()).and_then(|id| ctx.read_response(id)).map(|r| r.rect) {
            stops += 1;
            // A cell is a row high and among the rows. (A column's resize
            // handle runs the table's height; it may take focus.)
            let cell = r.height() < 30.0 && r.center().y > rows.top() && r.center().y < rows.bottom();
            assert!(!cell, "Tab stopped on a cell at {r:?} (rows {rows:?})");
        }
    }
    assert!(stops > 0, "Tab moves between the toolbar's controls");
}

/// Dragging a header moves its column: it lands where the blue line
/// shows, keeps its own width, and stays put on the next page. A plain
/// click still sorts (header_click_sorts).
#[test]
fn header_drag_moves_the_column() {
    let mut h = Harness::new();
    let names = ["_id", "name", "img", "zone"];
    // One frame; returns each header's rect (text centre and the cell's width), left to right.
    let mut frame = |v: &mut ResultView, events: Vec<Event>| {
        let (_, out) = h.run(v, Some(egui::vec2(1400.0, 900.0)), events);
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
    let id_width = v.cols.widths["_id"];
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
    assert_eq!(v.cols.dragging.as_deref(), Some("_id"), "a drag, not a click");
    frame(&mut v, vec![button(to, false)]);
    for _ in 0..3 {
        frame(&mut v, vec![]);
    }
    let heads = frame(&mut v, vec![]);
    assert_eq!(order(&heads), ["name", "img", "_id", "zone"]);
    assert!(v.cols.dragging.is_none());
    assert!((v.cols.widths["_id"] - id_width).abs() < 1.0, "_id kept its width: {} vs {id_width}", v.cols.widths["_id"]);
    assert!(v.cols.widths["name"] < 200.0, "the first column didn't take _id's width");
    let Source::Static { .. } = &v.source else { unreachable!() };
    assert!(v.local_sort.is_none(), "dragging didn't sort");

    // The order holds for the next page of the same result.
    v.refresh();
    frame(&mut v, vec![]);
    let heads = frame(&mut v, vec![]);
    assert_eq!(order(&heads), ["name", "img", "_id", "zone"]);

    // Dragging the edge between two headers still resizes, and moves nothing.
    let name_w = v.cols.widths["name"];
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
    assert!(v.cols.widths["name"] > name_w + 40.0, "name was widened: {name_w} -> {}", v.cols.widths["name"]);
}

/// The selected cell's outline covers the whole cell and is clipped like
/// the cell's text, so it can't show through below the table.
#[test]
fn cell_outline_stays_inside_the_table() {
    let mut h = Harness::new();
    let mut v = big_view(50);
    v.selection.selected = BTreeSet::from([2]);
    v.selection.selected_col = Some("name".into());
    let (_, out) = h.run(&mut v, Some(egui::vec2(1400.0, 900.0)), Vec::new());

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
    let mut h = Harness::new();
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
            let (_, out) = h.run(&mut v, Some(screen.size()), Vec::new());
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

/// The page size is one framed box holding the number and its ▾, like
/// Studio 3T's; ▾ opens the presets and picking one applies it.
#[test]
fn page_size_is_one_box() {
    use egui::{Event, Modifiers};
    let mut h = Harness::new();
    let mut v = big_view(50);
    let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1400.0, 700.0));
    let mut frame = |v: &mut ResultView, events: Vec<Event>| {
        let (_, out) = h.run(v, Some(screen.size()), events);
        out.shapes
    };
    let texts = |shapes: &[egui::epaint::ClippedShape], want: &str| -> Vec<egui::Rect> {
        shapes
            .iter()
            .filter_map(|s| match &s.shape {
                egui::Shape::Text(t) if t.galley.text() == want => Some(t.galley.rect.translate(t.pos.to_vec2())),
                _ => None,
            })
            .collect()
    };
    let mut shapes = Vec::new();
    for _ in 0..3 {
        shapes = frame(&mut v, vec![]);
    }
    let number = texts(&shapes, "50");
    let caret = texts(&shapes, icon::CARET_DOWN);
    assert_eq!((number.len(), caret.len()), (1, 1), "the number and ▾ drawn once each");
    let boxes: Vec<egui::Rect> = shapes
        .iter()
        .filter_map(|s| match &s.shape {
            egui::Shape::Rect(r) if r.rect.contains_rect(number[0]) && r.rect.contains_rect(caret[0]) && r.rect.width() < 200.0 => Some(r.rect),
            _ => None,
        })
        .collect();
    assert_eq!(boxes.len(), 1, "one frame around the number and ▾: {boxes:?}");
    let outer = boxes[0];
    assert!(caret[0].left() - number[0].right() >= 6.0, "a gap between the number and ▾: {:?} {:?}", number[0], caret[0]);
    // Like Studio 3T's "200 [▾]": ▾ in a small raised square of its own,
    // inside the box.
    let raised = theme::RAISED; // widgets.inactive.weak_bg_fill
    let squares: Vec<egui::Rect> = shapes
        .iter()
        .filter_map(|s| match &s.shape {
            egui::Shape::Rect(r) if r.fill == raised && r.rect.contains_rect(caret[0]) && !r.rect.contains_rect(number[0]) => Some(r.rect),
            _ => None,
        })
        .collect();
    assert_eq!(squares.len(), 1, "▾ sits on a raised square: {squares:?}");
    let square = squares[0];
    assert!(outer.contains_rect(square) && square.height() < outer.height(), "inside the box: {square:?} in {outer:?}");
    assert!((square.width() - square.height()).abs() <= 1.0, "a square: {square:?}");
    assert!(square.left() > number[0].right(), "right of the number: {square:?}");

    // ▾ opens the presets.
    let at = caret[0].center();
    let button = |pressed| Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
    frame(&mut v, vec![Event::PointerMoved(at)]);
    frame(&mut v, vec![button(true)]);
    frame(&mut v, vec![button(false)]);
    for _ in 0..3 {
        shapes = frame(&mut v, vec![]);
    }
    let hundred = texts(&shapes, "100");
    assert_eq!(hundred.len(), 1, "the menu lists 100");

    // Picking 100 applies it.
    let at = hundred[0].center();
    let button = |pressed| Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
    frame(&mut v, vec![Event::PointerMoved(at)]);
    frame(&mut v, vec![button(true)]);
    frame(&mut v, vec![button(false)]);
    for _ in 0..3 {
        shapes = frame(&mut v, vec![]);
    }
    assert_eq!(v.paging.page_size, 100);
    assert_eq!(texts(&shapes, "100").len(), 1, "the box now reads 100, the menu closed");
}

fn big_view(n: usize) -> ResultView {
    let docs: Vec<Document> = (0..n)
        .map(|i| doc! { "_id": rongo_core::bson::oid::ObjectId::new(), "name": format!("Position {i}"), "img": [], "position": i as i32, "bannerType": "advertisers", "meta": { "a": 1, "b": "x" } })
        .collect();
    find_view(docs.iter().map(|d| RawDocumentBuf::try_from(d).unwrap()).collect())
}

/// A query's view holding one page, which arrived the way a server
/// page does: moved in by on_page.
fn find_view(page: Vec<RawDocumentBuf>) -> ResultView {
    let n = page.len() as i64;
    let conn = ConnId(uuid::Uuid::nil());
    let mut v = ResultView::new(1, "big", Source::Find { conn, spec: FindSpec::all(Namespace::new("d", "c"), n), max: None });
    v.paging.page_size = n;
    v.paging.needs_fetch = false;
    v.paging.loading = Some(7);
    v.on_page(7, page, false, 1);
    v
}

/// Milliseconds per frame, after the first, for each view mode. Run with
/// `cargo test -p rongo --release perf -- --ignored --nocapture`.
fn frame_ms(mode: Mode, n: usize) -> f64 {
    frame_ms_selecting(mode, n, &[3])
}

/// `frame_ms` with these rows selected (every row, as after ⌘A).
fn frame_ms_selecting(mode: Mode, n: usize, rows: &[usize]) -> f64 {
    let mut h = Harness::new();
    let mut v = big_view(n);
    v.mode = mode;
    let arrived = Instant::now();
    v.selection.selected = rows.iter().copied().collect();
    let mut frame = |v: &mut ResultView| {
        h.run(v, Some(egui::vec2(1400.0, 900.0)), Vec::new());
    };
    frame(&mut v);
    let first_ms = arrived.elapsed().as_secs_f64() * 1000.0;
    frame(&mut v);
    while v.json.building.is_some() {
        std::thread::sleep(std::time::Duration::from_millis(5));
        frame(&mut v);
    }
    // The frames really had the data to draw.
    assert_eq!(v.rows.len(), n);
    if mode == Mode::Json {
        assert!(v.json.lines.as_ref().is_some_and(|l| l.len() > n), "JSON lines were built");
    }
    if mode == Mode::Tree {
        assert_eq!(v.tree_heights.len(), n);
    }
    // The fastest of several rounds: on a loaded machine another process
    // can slow any one round, but not every one.
    let frames = 10;
    let ms = (0..5)
        .map(|_| {
            let started = Instant::now();
            for _ in 0..frames {
                frame(&mut v);
            }
            started.elapsed().as_secs_f64() * 1000.0 / frames as f64
        })
        .fold(f64::INFINITY, f64::min);
    LAST_FIRST_FRAME_MS.with(|f| f.set(first_ms));
    ms
}

thread_local! {
    static LAST_FIRST_FRAME_MS: std::cell::Cell<f64> = const { std::cell::Cell::new(0.0) };
}

/// Frames until the JSON text is built (by the clock: a loaded machine
/// builds slowly; generous, it only guards a hang).
fn json_settles(h: &mut Harness, v: &mut ResultView) {
    let end = Instant::now() + std::time::Duration::from_secs(60);
    while v.json.lines.is_none() {
        assert!(Instant::now() < end, "the JSON text was not built");
        h.frame(v, vec![]);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

/// Leaving the JSON view keeps its text while the rows stay the same:
/// coming back shows it at once and builds nothing. Rows that change (a
/// sort here; another page) drop it.
#[test]
fn json_text_is_kept_until_the_rows_change() {
    let mut h = Harness::new();
    let mut v = big_view(200);
    v.mode = Mode::Json;
    json_settles(&mut h, &mut v);
    let text = v.json.lines.clone().expect("built");
    for mode in [Mode::Table, Mode::Tree] {
        v.mode = mode;
        h.frame(&mut v, vec![]);
        assert!(v.json.lines.as_ref().is_some_and(|t| Arc::ptr_eq(t, &text)), "kept while the {} shows", mode.label());
    }
    v.mode = Mode::Json;
    h.frame(&mut v, vec![]);
    assert!(v.json.lines.as_ref().is_some_and(|t| Arc::ptr_eq(t, &text)));
    assert_eq!(v.json.builds, 1, "nothing built again");
    v.sort_by("position".into(), Some(-1));
    assert!(v.json.lines.is_none(), "a sort drops it");
}

/// Turning the page in the JSON view builds the new page's text once:
/// nothing is built from the old page while the new one loads.
#[test]
fn a_page_turn_in_the_json_view_builds_once() {
    let mut h = Harness::new();
    let page = |from: i32| raw_page(&(from..from + 300).map(|i| doc! { "_id": i, "name": format!("doc {i}") }).collect::<Vec<_>>());
    let mut v = find_view(page(0));
    v.mode = Mode::Json;
    json_settles(&mut h, &mut v);
    let builds = v.json.builds;
    v.paging.offset = 300;
    v.paging.needs_fetch = true;
    // The server takes a while to answer.
    for _ in 0..30 {
        h.frame(&mut v, vec![]);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let req = v.paging.loading.expect("the next page was asked for");
    v.on_page(req, page(300), false, 1);
    json_settles(&mut h, &mut v);
    let text = &v.json.lines.as_ref().unwrap().text;
    assert!(text.starts_with("// 301\n") && text.contains("\"doc 300\""), "the new page's text");
    assert_eq!(v.json.builds - builds, 1, "built once, from the new page");
}

/// A JSON text no longer wanted (the rows changed under it) stops being
/// built, instead of formatting the old page next to the new one. The old
/// build is held after its first documents until the rows have changed (a
/// loaded machine once let it finish first), and its end is waited for,
/// not guessed from the clock.
#[test]
fn a_superseded_json_build_stops() {
    let mut h = Harness::new();
    let mut v = find_view(catalogue(3_000));
    v.mode = Mode::Json;
    super::json_text::hold_next_build();
    h.frame(&mut v, vec![]);
    let old = v.json.building.as_ref().expect("building").formatted.clone();
    v.paging.loading = Some(99);
    v.on_page(99, catalogue(10), false, 1);
    old.release();
    json_settles(&mut h, &mut v);
    let end = Instant::now() + std::time::Duration::from_secs(60);
    while !old.has_ended() {
        assert!(Instant::now() < end, "the old build never stopped");
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let done = old.count();
    assert!(done < 3_000, "the old build stopped part way, not after all {done} documents");
}
/// The JSON view's lines reach the bottom of the visible area: rows are
/// drawn at the height the scroll area assumed for them.
#[test]
fn json_lines_fill_the_view() {
    let mut h = Harness::new();
    let mut v = big_view(2_000);
    v.mode = Mode::Json;
    let mut frame = |v: &mut ResultView| {
        h.run(v, Some(egui::vec2(1400.0, 900.0)), Vec::new());
    };
    while v.json.lines.is_none() {
        frame(&mut v);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    frame(&mut v);
    frame(&mut v);
    let (drawn, visible) = v.json.fill.expect("JSON view drawn");
    assert!(drawn >= visible - 1.0, "lines end at {drawn}, the view at {visible}");
}

/// A new page's text arrives while the find bar still holds matches for
/// the old one: they are dropped, not drawn at the old byte offsets
/// (which panicked inside a Thai character).
#[test]
fn find_matches_do_not_outlive_their_text() {
    let mut h = Harness::new();
    let first = vec![doc! { "name": "abcdefgh" }];
    let mut v = ResultView::new(1, "t", Source::Static { docs: first.clone(), truncated: false });
    v.rows = Docs::from_docs(&first);
    v.paging.needs_fetch = false;
    v.mode = Mode::Json;
    let mut frame = |v: &mut ResultView| {
        h.run(v, None, Vec::new());
    };
    // By the clock, not a count of frames: a loaded machine builds the
    // lines and runs the find slowly. Generous, it only guards a hang.
    let settle = |v: &mut ResultView, frame: &mut dyn FnMut(&mut ResultView)| {
        let end = Instant::now() + std::time::Duration::from_secs(60);
        while Instant::now() < end {
            frame(v);
            if v.json.lines.is_some() && v.json.find.as_ref().is_none_or(|f| f.running.is_none()) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("timed out waiting for the JSON lines and the find");
    };
    settle(&mut v, &mut frame);
    v.json.find = Some(Find { text: "gh".into(), ..Default::default() });
    settle(&mut v, &mut frame);
    assert_eq!(v.json.find.as_ref().unwrap().matches.len(), 1);
    // The next page: same line, Thai text where the match was.
    v.rows = Docs::from_docs(&[doc! { "name": "ภาษาไทย" }]);
    v.rows_changed();
    settle(&mut v, &mut frame);
    assert!(v.json.find.as_ref().unwrap().matches.is_empty());
}

/// Rows are parsed only when used: drawing a 50,000-row Table or Tree
/// view parses the rows in view, not the page, and scrolling through it
/// keeps the parsed cache bounded.
#[test]
fn only_rows_in_use_are_parsed() {
    let mut h = Harness::new();
    for mode in [Mode::Table, Mode::Tree, Mode::Json] {
        let mut v = big_view(50_000);
        v.mode = mode;
        let mut frame = |v: &mut ResultView, events: Vec<Event>| {
            h.run(v, Some(egui::vec2(1400.0, 900.0)), events);
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
    let mut h = Harness::new();
    let mut v = big_view(500);
    let mut frame = |v: &mut ResultView, events: Vec<Event>| -> Option<String> {
        let (_, out) = h.run(v, Some(egui::vec2(1400.0, 900.0)), events);
        out.platform_output.commands.into_iter().find_map(|c| match c {
            egui::OutputCommand::CopyText(t) => Some(t),
            _ => None,
        })
    };
    let cmd_a = Event::Key { key: Key::A, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::COMMAND };

    frame(&mut v, vec![cmd_a.clone()]);
    assert_eq!(v.selection.selected.len(), 500, "Table view: ⌘A selected every document");

    v.mode = Mode::Json;
    v.selection.selected.clear();
    while v.json.lines.is_none() {
        frame(&mut v, vec![]);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    frame(&mut v, vec![cmd_a]);
    // Built off the UI thread, and copied when done.
    let mut copied = frame(&mut v, vec![Event::Copy]);
    let end = Instant::now() + std::time::Duration::from_secs(60);
    while copied.is_none() && Instant::now() < end {
        assert!(v.json.copy.is_some(), "the copy is on its way");
        std::thread::sleep(std::time::Duration::from_millis(2));
        copied = frame(&mut v, vec![]);
    }
    let copied = copied.expect("⌘C copied something");
    assert!(v.json.copy.is_none());
    let lines = v.json.lines.as_ref().unwrap();
    assert_eq!(copied, lines.text, "the whole JSON text, not just what is on screen");
    assert!(copied.contains("Position 499"));

    frame(&mut v, vec![Event::PointerMoved(pos2(600.0, 500.0))]);
    let click = |pressed| Event::PointerButton { pos: pos2(600.0, 500.0), button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
    frame(&mut v, vec![click(true)]);
    frame(&mut v, vec![click(false)]);
    assert_ne!(frame(&mut v, vec![Event::Copy]).as_deref(), Some(copied.as_str()), "a click cleared select-all");
}

/// The JSON view's ⌘A ⌘C copies text that pastes back as the same BSON
/// (whole-number doubles stay doubles), while the view itself shows
/// doubles for reading (`5.0`).
#[test]
fn json_view_copy_pastes_back() {
    let mut h = Harness::new();
    let docs = vec![doc! { "_id": 1, "price": 5.0, "big": 1e12, "f": 2.5 }, doc! { "_id": 2, "n": [1.0, { "d": -3.0 }] }];
    let mut v = find_view(raw_page(&docs));
    v.mode = Mode::Json;
    let end = Instant::now() + std::time::Duration::from_secs(60);
    while v.json.lines.is_none() {
        assert!(Instant::now() < end, "the JSON text was not built");
        h.frame(&mut v, vec![]);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(v.json.lines.as_ref().unwrap().text.contains("5.0"), "the view shows doubles for reading");
    let cmd_a = Event::Key { key: Key::A, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::COMMAND };
    h.frame(&mut v, vec![cmd_a]);
    h.frame(&mut v, vec![Event::Copy]);
    while h.copied.is_none() && Instant::now() < end {
        std::thread::sleep(std::time::Duration::from_millis(2));
        h.frame(&mut v, vec![]);
    }
    let copied = h.copied.take().expect("⌘C copied the text");
    assert_eq!(rongo_shell::parse_documents_text(&copied).unwrap(), docs, "{copied}");
}

/// ⌘F over a view opens the find bar on the JSON view; typing finds,
/// scrolls to the first match, and ↩ steps to the next.
#[test]
fn find_bar_finds_and_scrolls() {
    let mut h = Harness::new();
    let mut v = big_view(2_000);
    let mut frame = |v: &mut ResultView, events: Vec<Event>| {
        h.run(v, Some(egui::vec2(1400.0, 900.0)), events);
    };
    let key = |key: Key, modifiers: Modifiers| Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers };
    frame(&mut v, vec![Event::PointerMoved(pos2(600.0, 500.0))]);
    frame(&mut v, vec![key(Key::F, Modifiers::COMMAND)]);
    assert!(v.find_open(), "⌘F opened the find bar on the JSON view");
    while v.json.lines.is_none() {
        frame(&mut v, vec![]);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    frame(&mut v, vec![Event::Text("position 1".into())]);
    frame(&mut v, vec![]);
    while v.json.find.as_ref().is_some_and(|f| f.running.is_some()) {
        std::thread::sleep(std::time::Duration::from_millis(2));
        frame(&mut v, vec![]);
    }
    let f = v.json.find.as_ref().unwrap();
    assert_eq!(f.text, "position 1", "the field had focus");
    // "Position 1", "Position 10".."19", "Position 100".. and the
    // "position" fields' values 1, 10.. don't match ("position" : 1).
    assert_eq!(f.matches.len(), 1 + 10 + 100 + 1000);
    assert_eq!(f.current, 0);
    let first = f.matches[0].0;
    frame(&mut v, vec![key(Key::Enter, Modifiers::NONE)]);
    frame(&mut v, vec![]);
    let f = v.json.find.as_ref().unwrap();
    assert_eq!(f.current, 1, "↩ went to the next match");
    let second = f.matches[1].0;
    assert!(second > first + 5);
    // Jumping to the last match scrolls far down.
    let last = f.matches.len() - 1;
    v.json.find.as_mut().unwrap().current = last - 1;
    frame(&mut v, vec![key(Key::Enter, Modifiers::NONE)]);
    frame(&mut v, vec![]);
    let (current, line) = { let f = v.json.find.as_ref().unwrap(); (f.current, f.matches[f.current].0) };
    assert_eq!(current, last);
    assert!(v.json.top <= line && line < v.json.top + 60, "match line {line} is in view (top {})", v.json.top);
    frame(&mut v, vec![key(Key::Escape, Modifiers::NONE)]);
    assert!(v.json.find.is_none(), "Esc closed the find bar");
}

/// How long find takes over two million lines: ASCII lines, then the
/// JSON text of a page like the owner's, against the line-by-line
/// search it replaced (which must find the same). Run with
/// `cargo test -p rongo --release -- --ignored --nocapture report_find_time_for_2m_lines`.
#[test]
#[ignore = "timing; run explicitly in release mode"]
fn report_find_time_for_2m_lines() {
    let timed = |lines: &TextLines, needle: &str, case: bool| {
        let t = Instant::now();
        let found = find_matches(lines, needle, case, &std::sync::atomic::AtomicBool::new(false));
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        let t = Instant::now();
        let before = super::find::tests::find_matches_before(lines, needle, case);
        let before_ms = t.elapsed().as_secs_f64() * 1000.0;
        assert!(found == before, "{needle:?} case={case}: not what the line-by-line search finds");
        eprintln!("{needle:>12} case={case}: {} matches in {ms:.1} ms (line by line: {before_ms:.1} ms)", found.len());
    };
    let lines = TextLines::from_lines(&(0..2_000_000).map(|i| format!(r#"        "en" : "https://cdn.example/img/game/DGM_{i}.webp","#)).collect::<Vec<_>>());
    for (needle, case) in [("dgm_19999", false), ("DGM_19999", true), ("webp", false)] {
        timed(&lines, needle, case);
    }
    // The owner's kind of text: a page of nested multilingual documents
    // (Thai, Chinese, Japanese, Korean, Vietnamese, …). Mostly needles
    // with few matches, which search every line (MAX_MATCHES stops the
    // others early).
    let lines = json_lines(&Docs::new(catalogue(9_600)), 0, Style::Display);
    let ascii = (0..lines.len()).filter(|&i| lines.line(i).is_ascii()).count();
    eprintln!("catalogue: {} lines, {ascii} of them ASCII", lines.len());
    let needles = [
        ("golden", false),
        ("g01234", false),
        ("G01234", true),
        ("12345", false),
        ("แตกง่าย 4321", false),
        ("แตกง่าย 4321", true),
        ("ドラゴンスロット 4321", false),
        ("nổ hũ 4321", false),
        ("NỔ HŨ 4321", true),
    ];
    for (needle, case) in needles {
        timed(&lines, needle, case);
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

/// A Table view frame with every one of 50,000 rows selected (⌘A), as
/// against one. Run with
/// `cargo test -p rongo --release -- --ignored --nocapture report_frame_time_after_select_all`.
#[test]
#[ignore = "timing; run explicitly in release mode"]
fn report_frame_time_after_select_all() {
    let n = 50_000;
    let one = frame_ms(Mode::Table, n);
    let all = frame_ms_selecting(Mode::Table, n, &(0..n).collect::<Vec<_>>());
    eprintln!("Table, {n} rows: {one:.2} ms/frame with one selected, {all:.2} ms/frame with all selected");
}

/// What ⌘C, Delete/⌘U/F8 (the selected `_id`s) and the JSON view's
/// ⌘A ⌘C cost with all of a page like the owner's selected. Run with
/// `cargo test -p rongo --release -- --ignored --nocapture report_bulk_action_time`.
#[test]
#[ignore = "timing; run explicitly in release mode"]
fn report_bulk_action_time() {
    fn timed<T>(f: impl FnOnce() -> T) -> (f64, T) {
        let t = Instant::now();
        let out = f();
        (t.elapsed().as_secs_f64() * 1000.0, out)
    }
    let page = catalogue(9_600);
    let all: Vec<usize> = (0..page.len()).collect();
    for round in 0..2 {
        let mut v = find_view(page.clone());
        v.select_rows(&all);
        let (ids_ms, ids) = timed(|| v.selected_ids());
        let parsed_by_ids = v.rows.parsed();
        let mut v = find_view(page.clone());
        v.select_rows(&all);
        let (copy_ms, text) = timed(|| v.copy_text());
        let (json_ms, json) = timed(|| json_lines(&v.rows, 0, Style::Editable).text.len());
        // ⌘C as it was: every selected document parsed (and kept), then
        // cloned into one array, then formatted.
        let mut v = find_view(page.clone());
        v.select_rows(&all);
        let (old_ms, old) = timed(|| {
            let docs = v.selected_docs();
            let arr = Bson::Array(docs.iter().map(|d| Bson::Document((*d).clone())).collect());
            format::value_to_shell_pretty(&arr, Style::Editable).len()
        });
        eprintln!(
            "round {round}: selected _ids {ids_ms:.1} ms ({} ids, {parsed_by_ids} documents parsed), ⌘C {copy_ms:.1} ms ({:.1} MB; as it was {old_ms:.1} ms, {} kept parsed), JSON ⌘A ⌘C {json_ms:.1} ms ({:.1} MB)",
            ids.len(),
            text.len() as f64 / 1e6,
            v.rows.parsed(),
            json as f64 / 1e6
        );
        assert_eq!(old, text.len());
        assert_eq!(ids.len(), page.len());
    }
}

/// `n` documents shaped like a real game catalogue page: nested and
/// multilingual (Thai, Chinese, Japanese, Korean, …), about 200 values
/// each, with sort keys in no particular order.
fn catalogue(n: usize) -> Vec<RawDocumentBuf> {
    let langs = [
        ("en", "Golden Dragon Slots"),
        ("th", "สล็อตมังกรทองคำ แจ็คพอตแตกง่าย"),
        ("zh", "金龙老虎机 大奖"),
        ("ja", "ゴールデンドラゴンスロット"),
        ("ko", "골든 드래곤 슬롯 잭팟"),
        ("vi", "Rồng Vàng Slot Nổ Hũ"),
        ("id", "Slot Naga Emas Jackpot"),
        ("ms", "Slot Naga Emas Hebat"),
    ];
    let fields = ["name", "description", "img", "banner", "thumbnail", "alt", "title", "subtitle", "promo", "rules", "howTo", "seo"];
    let providers = ["PG", "PP", "JILI", "EVO", "SA", "SEXY", "KM"];
    (0..n)
        .map(|i| {
            // A scrambled number, so the arrival order is not the sorted one.
            let k = i * 7919 % n;
            let local = |what: &str, salt: usize| -> Document {
                langs
                    .iter()
                    .map(|(l, base)| {
                        let text = match what {
                            "img" | "banner" | "thumbnail" => format!("https://cdn.example/img/game/{l}/G{k:05}_{salt}.webp"),
                            "description" | "rules" | "howTo" => format!("{base} {k} — {base} {base} {salt}"),
                            _ => format!("{base} {k} {what}"),
                        };
                        (l.to_string(), Bson::String(text))
                    })
                    .collect()
            };
            let mut d = doc! {
                "_id": rongo_core::bson::oid::ObjectId::new(),
                "code": format!("GAME_{k:05}"),
                "provider": providers[i % providers.len()],
                "position": k as i32,
                "rtp": 90.0 + (k % 100) as f64 / 10.0,
                "active": !i.is_multiple_of(3),
                "createdAt": rongo_core::bson::DateTime::from_millis(1_700_000_000_000 + k as i64 * 60_000),
                "updatedAt": rongo_core::bson::DateTime::from_millis(1_710_000_000_000 + i as i64 * 1_000),
                "volatility": (k % 5) as i32,
                "lines": 20 + (k % 30) as i32,
            };
            for (s, f) in fields.iter().enumerate() {
                d.insert(*f, local(f, s));
            }
            d.insert("categories", (0..4).map(|c| Bson::Document(doc! { "id": (k % 13 + c) as i32, "name": local("category", c) })).collect::<Vec<_>>());
            d.insert("tags", (0..12).map(|t| Bson::String(format!("tag-{}", (k + t) % 40))).collect::<Vec<_>>());
            let limits: Document = ["THB", "USD", "CNY", "JPY", "KRW", "VND", "IDR", "MYR"]
                .iter()
                .enumerate()
                .map(|(c, cur)| (cur.to_string(), Bson::Document(doc! { "min": 0.1 * (c + 1) as f64, "max": 1000.0 * (c + 1) as f64, "default": (k % 10) as f64 })))
                .collect();
            d.insert("limits", limits);
            let features: Document = (0..20usize).map(|f| (format!("f{f}"), if f.is_multiple_of(2) { Bson::Boolean((k + f).is_multiple_of(3)) } else { Bson::Int32(((k + f) % 7) as i32) })).collect();
            d.insert("features", features);
            RawDocumentBuf::try_from(&d).unwrap()
        })
        .collect()
}

/// How long a header sort takes on a page like the owner's (~9,600
/// nested multilingual documents). Run with
/// `cargo test -p rongo --release -- --ignored --nocapture report_sort_time`.
#[test]
#[ignore = "timing; run explicitly in release mode"]
fn report_sort_time() {
    fn timed(f: impl FnOnce()) -> f64 {
        let t = Instant::now();
        f();
        t.elapsed().as_secs_f64() * 1000.0
    }
    let page = catalogue(9_600);
    let mb = page.iter().map(|d| d.as_bytes().len()).sum::<usize>() as f64 / 1e6;
    eprintln!("{} documents, {mb:.1} MB of BSON", page.len());
    for path in ["code", "name.th", "name"] {
        let mut v = find_view(page.clone());
        let up = timed(|| v.sort_by(path.into(), Some(1)));
        let down = timed(|| v.sort_by(path.into(), Some(-1)));
        let off = timed(|| v.sort_by(path.into(), None));
        v.sort_by(path.into(), Some(1));
        let next = page.clone();
        v.paging.loading = Some(99);
        let turn = timed(|| v.on_page(99, next, false, 1));
        eprintln!("{path:>8}: ascending {up:.1} ms, descending {down:.1} ms, off {off:.1} ms, next page sorted {turn:.1} ms");
        assert_eq!(v.rows.len(), page.len());
        assert!(v.rows.parsed() < 100, "a header sort parsed {} documents", v.rows.parsed());
    }
}

/// What a refreshed document costs on a page like the owner's: the
/// reply applied in place (`on_doc`), unsorted and under a header sort,
/// against copying the page as `Docs::with` did. Run with
/// `cargo test -p rongo --release -- --ignored --nocapture report_refresh_time`.
#[test]
#[ignore = "timing; run explicitly in release mode"]
fn report_refresh_time() {
    fn timed(f: impl FnOnce()) -> f64 {
        let t = Instant::now();
        f();
        t.elapsed().as_secs_f64() * 1000.0
    }
    let mut h = Harness::new();
    let page = catalogue(9_600);
    let copy = timed(|| drop(std::hint::black_box(Docs::new(page.clone()))));
    for sort in [None, Some("code")] {
        let mut v = find_view(page.clone());
        if let Some(path) = sort {
            v.sort_by(path.into(), Some(1));
        }
        v.select_rows(&[10, 20]);
        let row = 4_321;
        let mut reply = Document::try_from(v.rows.raw_doc(row).unwrap().as_ref()).unwrap();
        let id = reply.get("_id").cloned().unwrap();
        reply.insert("code", "GAME_00000a");
        v.refresh_id(id.clone());
        h.frame(&mut v, vec![]);
        let req = v.refreshing.last().unwrap().0;
        let ms = timed(|| v.on_doc(req, id, raw_page(&[reply.clone()])));
        eprintln!("on_doc, {}: {ms:.2} ms (copying the page as Docs::with did: {copy:.1} ms)", sort.map_or("unsorted".to_string(), |p| format!("sorted by {p}")));
        assert_eq!(v.rows.len(), page.len());
        assert_eq!(v.selected_rows().len(), 2);
    }
}

/// Stepping into an array again after its document was re-read in place
/// (step in, Esc, edit the document, step into the same cell) shows the
/// array as it is now, not the rows the first drill resolved: elements
/// added show, removed ones leave no blank rows. Also under a header
/// sort.
#[test]
fn a_drill_made_again_shows_its_document_as_re_read() {
    let with = |n: i64| doc! { "_id": 1, "n": 5, "items": (0..n).map(|i| Bson::Document(doc! { "i": i })).collect::<Vec<_>>() };
    for sorted in [false, true] {
        let mut h = Harness::new();
        let mut v = find_view(raw_page(&[doc! { "_id": 0, "n": 9 }, with(3), doc! { "_id": 2, "n": 1 }]));
        if sorted {
            v.sort_by("n".into(), Some(1));
        }
        let row = (0..3).find(|&r| v.rows.doc(r).and_then(|d| d.get("_id")) == Some(&Bson::Int32(1))).unwrap();
        v.mode = Mode::Tree;
        assert!(v.step_into(row, "items", false));
        assert_eq!(v.resolved().len(), 3);
        h.frame(&mut v, vec![]);
        for n in [5, 1] {
            // Esc, then the document is edited and read again.
            v.set_drill(Vec::new());
            v.refresh_id(Bson::Int32(1));
            h.frame(&mut v, vec![]);
            let req = v.refreshing.last().expect("the document is read again").0;
            v.on_doc(req, Bson::Int32(1), raw_page(&[with(n)]));
            assert!(v.step_into(row, "items", false));
            let rows = v.resolved();
            assert_eq!(rows.len(), n as usize, "sorted={sorted}: the array as it is now");
            assert!((0..rows.len()).all(|i| rows.get(i).is_some()), "sorted={sorted}: every row has its element");
            drop(rows);
            h.frame(&mut v, vec![]);
            let (_, texts) = h.frame(&mut v, vec![]);
            assert!(texts.iter().any(|(t, _)| t.ends_with(&format!("({n})"))), "sorted={sorted}: the Tree draws all {n} rows");
        }
    }
}

/// A drill left open while its document is re-read in place (a save from
/// the editor, say) shows the document as it is now, with no step out and
/// in again: an array's elements as they are, also under a header sort,
/// and a column's new values. A re-read that moves nothing else (no drill,
/// no sort) still gives the rows a new level key, so a level kept from
/// before is never taken for the new content.
#[test]
fn an_open_drill_shows_its_document_as_re_read() {
    let with = |n: i64| doc! { "_id": 1, "n": 5, "meta": { "len": n }, "items": (0..n).map(|i| Bson::Document(doc! { "i": i })).collect::<Vec<_>>() };
    let reread = |h: &mut Harness, v: &mut ResultView, doc: Document| {
        v.refresh_id(Bson::Int32(1));
        h.frame(v, vec![]);
        let req = v.refreshing.last().expect("the document is read again").0;
        v.on_doc(req, Bson::Int32(1), raw_page(&[doc]));
    };
    for sorted in [false, true] {
        let mut h = Harness::new();
        let mut v = find_view(raw_page(&[doc! { "_id": 0, "n": 9 }, with(3), doc! { "_id": 2, "n": 1 }]));
        if sorted {
            v.sort_by("n".into(), Some(1));
        }
        let row = (0..3).find(|&r| v.rows.doc(r).and_then(|d| d.get("_id")) == Some(&Bson::Int32(1))).unwrap();
        v.mode = Mode::Tree;
        assert!(v.step_into(row, "items", false));
        h.frame(&mut v, vec![]);
        for n in [5, 1] {
            reread(&mut h, &mut v, with(n));
            assert_eq!(v.drill.len(), 1, "sorted={sorted}: the drill stays open");
            let rows = v.resolved();
            let elements: Vec<Bson> = (0..rows.len()).map(|i| rows.get(i).expect("every row has its element").value.clone()).collect();
            assert_eq!(elements, (0..n).map(|i| Bson::Document(doc! { "i": i })).collect::<Vec<_>>(), "sorted={sorted}: the array as it is now");
            drop(rows);
            h.frame(&mut v, vec![]);
            let (_, texts) = h.frame(&mut v, vec![]);
            assert!(texts.iter().any(|(t, _)| t.ends_with(&format!("({n})"))), "sorted={sorted}: the Tree draws all {n} rows");
        }
    }
    // A column drill: every document's `meta`, the re-read one's new.
    let mut h = Harness::new();
    let mut v = find_view(raw_page(&[doc! { "_id": 0, "meta": { "len": 0 } }, with(3)]));
    v.set_drill(vec![Step::Field("meta".into())]);
    h.frame(&mut v, vec![]);
    reread(&mut h, &mut v, with(7));
    assert_eq!(v.drill, [Step::Field("meta".into())]);
    assert_eq!(v.resolved().get(1).map(|r| r.value.clone()), Some(Bson::Document(doc! { "len": 7_i64 })));
    // The level key: new for an in-place re-read that moves nothing else.
    let mut v = find_view(raw_page(&[with(3)]));
    h.frame(&mut v, vec![]);
    let before = v.level_key();
    reread(&mut h, &mut v, with(4));
    assert_ne!(v.level_key(), before, "the content changed in place");
}

/// Drilled into a column (where every row is in use) or into one
/// document's long array, a frame with 50,000 rows still costs about
/// what one with 500 does: the drill's rows are resolved once per
/// change, not each frame, and the selected document isn't copied
/// each frame.
#[test]
fn drilled_frame_cost_does_not_grow_with_row_count() {
    for mode in [Mode::Table, Mode::Tree] {
        let column = |n: usize| {
            let mut v = big_view(n);
            v.mode = mode;
            v.set_drill(vec![Step::Field("meta".into())]);
            v.selection.selected = BTreeSet::from([3]);
            view_frame_ms(&mut v)
        };
        let array = |n: usize| {
            let items: Vec<Bson> = (0..n).map(|i| Bson::Document(doc! { "i": i as i64 })).collect();
            let mut v = find_view(vec![RawDocumentBuf::try_from(&doc! { "_id": 1, "items": items }).unwrap()]);
            v.mode = mode;
            assert!(v.step_into(0, "items", false));
            v.selection.selected = BTreeSet::from([3]);
            view_frame_ms(&mut v)
        };
        let (small, big) = (column(500), column(50_000));
        assert!(big < small * 3.0 + 5.0, "{}, a column: {big:.1} ms with 50k rows vs {small:.1} ms with 500", mode.label());
        let (small, big) = (array(500), array(50_000));
        assert!(big < small * 3.0 + 5.0, "{}, an array: {big:.1} ms with 50k rows vs {small:.1} ms with 500", mode.label());
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

/// Milliseconds per frame for `v` as it is, after its first frames (and
/// its JSON text, in the JSON view): the fastest of five rounds of ten.
fn view_frame_ms(v: &mut ResultView) -> f64 {
    let mut h = Harness::new();
    h.frame(v, vec![]);
    h.frame(v, vec![]);
    let end = Instant::now() + std::time::Duration::from_secs(120);
    while v.json.building.is_some() && Instant::now() < end {
        std::thread::sleep(std::time::Duration::from_millis(5));
        h.frame(v, vec![]);
    }
    (0..5)
        .map(|_| {
            let started = Instant::now();
            for _ in 0..10 {
                h.frame(v, vec![]);
            }
            started.elapsed().as_secs_f64() * 100.0
        })
        .fold(f64::INFINITY, f64::min)
}

/// Documents of 40 fields, some nested documents and arrays, as the
/// Tree view shows them.
fn wide_view(n: usize) -> ResultView {
    let docs: Vec<RawDocumentBuf> = (0..n)
        .map(|i| {
            let mut d = doc! { "_id": i as i64, "name": format!("player {i}") };
            for k in 0..40i64 {
                let v = match k % 4 {
                    0 => Bson::String(format!("value {i}.{k}")),
                    1 => Bson::Int64(i as i64 * k),
                    2 => Bson::Document(doc! { "a": k, "b": [1, 2, 3], "c": { "d": "e" } }),
                    _ => Bson::Array((0..10).map(|x| Bson::Int64(x * k)).collect()),
                };
                d.insert(format!("f{k}"), v);
            }
            RawDocumentBuf::try_from(&d).unwrap()
        })
        .collect();
    find_view(docs)
}

/// Frames of drilled views and of the Tree view at 50,000 rows: the
/// rows a drill makes, the selected document the toolbar looks at, and
/// a Tree view of wide documents. Run with
/// `cargo test -p rongo --release -- --ignored --nocapture report_drilled_and_tree_frame_times`.
#[test]
#[ignore = "timing; run explicitly in release mode"]
fn report_drilled_and_tree_frame_times() {
    let n = 50_000;
    for mode in [Mode::Table, Mode::Tree] {
        let mut v = big_view(n);
        v.mode = mode;
        v.set_drill(vec![Step::Field("meta".into())]);
        v.selection.selected = BTreeSet::from([3]);
        let ms = view_frame_ms(&mut v);
        assert_eq!(v.resolved().len(), n);
        eprintln!("{:>10}, {n} rows drilled into a column, one selected: {ms:.2} ms/frame", mode.label());
    }
    for mode in [Mode::Table, Mode::Tree] {
        let items: Vec<Bson> = (0..n).map(|i| Bson::Document(doc! { "i": i as i64, "name": format!("item {i}") })).collect();
        let mut v = find_view(vec![RawDocumentBuf::try_from(&doc! { "_id": 1, "items": items }).unwrap()]);
        v.mode = mode;
        assert!(v.step_into(0, "items", false));
        v.selection.selected = BTreeSet::from([3]);
        let ms = view_frame_ms(&mut v);
        assert_eq!(v.resolved().len(), n);
        eprintln!("{:>10}, one document's {n}-element array, one selected: {ms:.2} ms/frame", mode.label());
    }
    let mut v = wide_view(n);
    v.mode = Mode::Tree;
    v.selection.selected = BTreeSet::from([3]);
    let ms = view_frame_ms(&mut v);
    eprintln!(" Tree View, {n} documents of 40 fields, one selected: {ms:.2} ms/frame");
}

/// The JSON text of a page like the owner's (~9,600 nested multilingual
/// documents, ~2.5M lines): how long it takes to build, to show again
/// after the Table view, and what ⌘A ⌘C costs the frame. Run with
/// `cargo test -p rongo --release -- --ignored --nocapture report_json_build_time`.
#[test]
#[ignore = "timing; run explicitly in release mode"]
fn report_json_build_time() {
    fn ms(t: Instant) -> f64 {
        t.elapsed().as_secs_f64() * 1000.0
    }
    let page = catalogue(9_600);
    let raw_mb = page.iter().map(|d| d.as_bytes().len()).sum::<usize>() as f64 / 1e6;
    let rows = Docs::new(page.clone());
    let t = Instant::now();
    let text = json_lines(&rows, 0, Style::Display);
    let alone = ms(t);
    eprintln!("{} lines, {:.1} MB of text from {raw_mb:.1} MB of BSON; the builder alone: {alone:.1} ms", text.len(), text.text.len() as f64 / 1e6);
    drop(text);
    let mut h = Harness::new();
    let mut v = find_view(page);
    v.mode = Mode::Json;
    let t = Instant::now();
    while v.json.lines.is_none() {
        h.frame(&mut v, vec![]);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let first = ms(t);
    v.mode = Mode::Table;
    h.frame(&mut v, vec![]);
    v.mode = Mode::Json;
    let t = Instant::now();
    h.frame(&mut v, vec![]);
    while v.json.lines.is_none() {
        std::thread::sleep(std::time::Duration::from_millis(1));
        h.frame(&mut v, vec![]);
    }
    let back = ms(t);
    let cmd_a = Event::Key { key: Key::A, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::COMMAND };
    h.frame(&mut v, vec![cmd_a]);
    h.copied = None;
    let t = Instant::now();
    h.frame(&mut v, vec![Event::Copy]);
    let copy_frame = ms(t);
    let mut slowest = copy_frame;
    while h.copied.is_none() {
        std::thread::sleep(std::time::Duration::from_millis(1));
        let f = Instant::now();
        h.frame(&mut v, vec![]);
        slowest = slowest.max(ms(f));
    }
    let copied = ms(t);
    eprintln!("JSON view: text shown {first:.1} ms after the first frame; again after the Table view {back:.1} ms; ⌘A ⌘C on the clipboard after {copied:.1} ms, the slowest frame meanwhile {slowest:.1} ms");
}

/// A page turn in the JSON view, the server answering after 300 ms: how
/// many JSON texts are built, and when the new page's text is shown.
/// Run with `cargo test -p rongo --release -- --ignored --nocapture report_json_page_turn`.
#[test]
#[ignore = "timing; run explicitly in release mode"]
fn report_json_page_turn() {
    let page = catalogue(9_600);
    let mut h = Harness::new();
    let mut v = find_view(page.clone());
    v.mode = Mode::Json;
    while v.json.lines.is_none() {
        h.frame(&mut v, vec![]);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    v.paging.offset = page.len() as u64;
    v.paging.needs_fetch = true;
    let mut builds = 0;
    let mut frame = |h: &mut Harness, v: &mut ResultView| {
        let before = v.json.building.is_some();
        h.frame(v, vec![]);
        if !before && v.json.building.is_some() {
            builds += 1;
        }
    };
    let t = Instant::now();
    while t.elapsed() < std::time::Duration::from_millis(300) {
        frame(&mut h, &mut v);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let req = v.paging.loading.expect("the next page was asked for");
    let landed = Instant::now();
    v.on_page(req, page, false, 1);
    while v.json.lines.is_none() {
        frame(&mut h, &mut v);
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let shown = landed.elapsed().as_secs_f64() * 1000.0;
    eprintln!("page turn in the JSON view: {builds} JSON text builds started; the new page's text shown {shown:.1} ms after it landed");
}
