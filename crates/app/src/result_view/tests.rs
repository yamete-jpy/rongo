use super::*;
use super::toolbar::{elapsed_text, parse_page_size};
use egui::Event;

fn view(docs: Vec<Document>) -> ResultView {
    let mut v = ResultView::new(1, "t", Source::Static { docs: docs.clone(), truncated: false });
    v.rows = Docs::from_docs(&docs);
    v
}

fn paths(v: &ResultView) -> Vec<(usize, String)> {
    let rows = v.resolved();
    (0..rows.len()).filter_map(|i| rows.get(i).map(|r| (r.origin, rows.path(i)))).collect()
}

/// The Tree view's running sums follow a changed height from where it
/// changed, and ignore rounding.
#[test]
fn tree_heights_keep_their_sums() {
    let mut t = TreeHeights::default();
    assert_eq!(t.tops(), [0.0]);
    t.reset(4, 24.0);
    assert_eq!(t.tops(), [0.0, 24.0, 48.0, 72.0, 96.0]);
    t.set(2, 100.0);
    assert_eq!(t.tops(), [0.0, 24.0, 48.0, 148.0, 172.0]);
    t.set(2, 100.004);
    t.set(0, 30.0);
    assert_eq!(t.tops(), [0.0, 30.0, 54.0, 154.0, 178.0]);
    t.clear();
    assert_eq!((t.len(), t.tops()), (0, &[0.0][..]));
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
    v.drill = vec![Step::Elements { row: 0, doc: DocKey { place: 0, id: Some(Bson::Int32(1)) }, field: "tags".into() }];
    assert_eq!(paths(&v), [(0, "tags.0".to_string()), (0, "tags.1".to_string())]);
    assert_eq!(v.shared_prefix(), None);
    v.selection.selected = BTreeSet::from([0, 1]);
    assert_eq!(v.selected_docs().len(), 1);
    v.drill = vec![Step::Field("cfg".into()), Step::Cell { row: 0, doc: DocKey { place: 0, id: Some(Bson::Int32(1)) }, field: "deep".into() }];
    assert_eq!(paths(&v), [(0, "cfg.deep".to_string())]);
}

/// Step Into Column is the same field of every row, an object's or an
/// array's (Studio 3T's); Step Into Cell is one row's value: an array's
/// elements as rows, or an object as one row.
#[test]
fn step_into_picks_the_right_step() {
    let mut v = view(vec![doc! { "o": { "a": 1 }, "arr": [1, 2], "s": "x" }]);
    assert!(!v.step_into(0, "s", false));
    assert!(v.step_into(0, "arr", false));
    assert_eq!(v.drill, [Step::Field("arr".into())]);
    v.set_drill(Vec::new());
    assert!(v.step_into(0, "arr", true));
    assert_eq!(v.drill, [Step::Elements { row: 0, doc: DocKey { place: 0, id: None }, field: "arr".into() }]);
    v.set_drill(Vec::new());
    assert!(v.step_into(0, "o", false));
    assert_eq!(v.drill, [Step::Field("o".into())]);
    v.set_drill(Vec::new());
    assert!(v.step_into(0, "o", true));
    assert_eq!(v.drill, [Step::Cell { row: 0, doc: DocKey { place: 0, id: None }, field: "o".into() }]);
}

/// The cells of a drill level's row `i`, column by column (`None`: empty).
fn level_cells(v: &ResultView, i: usize) -> Vec<(String, Option<Bson>)> {
    let rows = v.resolved();
    let row = rows.get(i).expect("the row");
    compute_columns(&rows).into_iter().map(|c| {
        let cell = cell_in(row.value, &c).cloned();
        (c, cell)
    }).collect()
}

/// Stepped into as a column, an array is a row per document, its
/// elements in columns 0, 1, … up to the longest array; a shorter
/// array's missing index is an empty cell. The rows' path is the
/// array's, so a cell's is `gameType.1`.
#[test]
fn an_array_column_steps_in_as_index_columns() {
    let s = |t: &str| Some(Bson::String(t.into()));
    let mut v = view(vec![
        doc! { "_id": 1, "gameType": ["POKER"] },
        doc! { "_id": 2, "gameType": ["LUCKY", "WHEEL"] },
        doc! { "_id": 3, "gameType": [] },
    ]);
    assert!(v.step_into(0, "gameType", false));
    assert_eq!(v.resolved().len(), 3, "a row per document");
    assert_eq!(compute_columns(&v.resolved()), ["0", "1"]);
    assert_eq!(level_cells(&v, 0), [("0".to_string(), s("POKER")), ("1".to_string(), None)]);
    assert_eq!(level_cells(&v, 1), [("0".to_string(), s("LUCKY")), ("1".to_string(), s("WHEEL"))]);
    assert_eq!(level_cells(&v, 2), [("0".to_string(), None), ("1".to_string(), None)]);
    assert_eq!(paths(&v), [(0, "gameType".to_string()), (1, "gameType".to_string()), (2, "gameType".to_string())]);
    v.selection.selected = BTreeSet::from([1]);
    v.selection.anchor = Some(1);
    v.selection.selected_col = Some("1".into());
    assert_eq!(v.selected_field(), Some(("gameType.1".to_string(), Bson::String("WHEEL".into()))));
}

/// Values of several kinds at one level: `(value)` (the scalars) first,
/// then the objects' keys in the order first seen, then the arrays'
/// indices in order. A key that reads as an index is one column.
#[test]
fn mixed_values_give_value_keys_then_indices() {
    let mut v = view(vec![doc! { "x": [1, 2, 3] }, doc! { "x": 5 }, doc! { "x": { "b": 1, "a": 2 } }, doc! { "x": { "c": 1, "0": "zero" } }]);
    v.drill = vec![Step::Field("x".into())];
    assert_eq!(compute_columns(&v.resolved()), ["(value)", "b", "a", "c", "0", "1", "2"]);
    assert_eq!(level_cells(&v, 0)[0], ("(value)".to_string(), None), "an array is not a value of its own");
    assert_eq!(level_cells(&v, 1)[0], ("(value)".to_string(), Some(Bson::Int32(5))));
    assert_eq!(level_cells(&v, 3)[4], ("0".to_string(), Some(Bson::String("zero".into()))), "the object's own \"0\"");
    assert_eq!(level_cells(&v, 0)[4], ("0".to_string(), Some(Bson::Int32(1))), "the array's element 0");
}

/// An element stepped into as a column (`items`, then `0`) shows that
/// element's fields for every document: its rows' path is `items.0`, a
/// cell's `items.0.name`. A document whose array has no such element has
/// a null row, as a missing field does. Step Into Cell works inside one
/// too.
#[test]
fn an_element_column_steps_in_to_its_fields() {
    let mut v = view(vec![
        doc! { "_id": 1, "items": [{ "name": "a", "qty": 1, "meta": { "k": 1 } }, { "name": "b" }] },
        doc! { "_id": 2, "items": [{ "name": "c", "price": 3 }] },
        doc! { "_id": 3, "items": [] },
    ]);
    assert!(v.step_into(0, "items", false));
    assert_eq!(compute_columns(&v.resolved()), ["0", "1"]);
    assert!(v.step_into(0, "0", false), "an element that is an object steps in");
    assert_eq!(v.drill, [Step::Field("items".into()), Step::Field("0".into())]);
    assert_eq!(compute_columns(&v.resolved()), ["(value)", "name", "qty", "meta", "price"]);
    assert_eq!(paths(&v), [(0, "items.0".to_string()), (1, "items.0".to_string()), (2, "items.0".to_string())]);
    assert_eq!(v.resolved().get(2).map(|r| r.value.clone()), Some(Bson::Null), "no element 0");
    v.selection.selected = BTreeSet::from([1]);
    v.selection.anchor = Some(1);
    v.selection.selected_col = Some("name".into());
    assert_eq!(v.selected_field(), Some(("items.0.name".to_string(), Bson::String("c".into()))));
    assert_eq!(v.shared_prefix().as_deref(), Some("items.0"), "the header sort's paths");

    // Step Into Cell on a field of an element stepped into as a column.
    assert!(v.step_into(0, "meta", true));
    assert_eq!(paths(&v), [(0, "items.0.meta".to_string())]);
    assert_eq!(v.resolved().get(0).map(|r| r.value.clone()), Some(Bson::Document(doc! { "k": 1 })));

    // And on an element (an object) from the array's level.
    v.set_drill(vec![Step::Field("items".into())]);
    assert!(v.step_into(0, "1", true));
    assert!(matches!(v.drill.as_slice(), [Step::Field(_), Step::Cell { row: 0, field, .. }] if field == "1"), "{:?}", v.drill);
    assert_eq!(paths(&v), [(0, "items.1".to_string())]);
    assert_eq!(v.resolved().get(0).map(|r| r.value.clone()), Some(Bson::Document(doc! { "name": "b" })));
}

/// An array has only its elements: a field step by a name that is not an
/// index leaves out the rows where the value is an array, as it does a
/// scalar (no null row whose path the server would refuse to write). An
/// index step gives each array's element, null where it is missing, and
/// null for an object without that key.
#[test]
fn only_an_index_steps_into_an_array() {
    let mut v = view(vec![doc! { "x": { "name": { "first": "a" } } }, doc! { "x": [1, 2] }, doc! { "x": 5 }, doc! { "x": [] }]);
    v.drill = vec![Step::Field("x".into()), Step::Field("name".into())];
    assert_eq!(paths(&v), [(0, "x.name".to_string())]);
    v.drill = vec![Step::Field("x".into()), Step::Field("0".into())];
    assert_eq!(paths(&v), [(0, "x.0".to_string()), (1, "x.0".to_string()), (3, "x.0".to_string())]);
    let values: Vec<Bson> = (0..3).filter_map(|i| v.resolved().get(i).map(|r| r.value.clone())).collect();
    assert_eq!(values, [Bson::Null, Bson::Int32(1), Bson::Null]);
}

/// Step Into Cell on an array is still that row's elements as rows,
/// each at its own index.
#[test]
fn step_into_cell_on_an_array_lists_its_elements() {
    let mut v = view(vec![doc! { "_id": 1, "gameType": ["POKER"] }, doc! { "_id": 2, "gameType": ["LUCKY", "WHEEL"] }]);
    assert!(v.step_into(1, "gameType", true));
    assert!(matches!(v.drill.as_slice(), [Step::Elements { row: 1, .. }]));
    assert_eq!(paths(&v), [(1, "gameType.0".to_string()), (1, "gameType.1".to_string())]);
    assert_eq!(compute_columns(&v.resolved()), ["(value)"]);
}

#[test]
fn copy_selection_as_csv_and_for_excel() {
    let docs: Vec<Document> = (0..3)
        .map(|i| if i == 2 { doc! { "_id": i, "name": format!("n{i}"), "f": "=1+1", "=1+1": 5, "nested": { "a": 1 } } } else { doc! { "_id": i, "name": format!("n{i}"), "nested": { "a": 1 } } })
        .collect();
    let mut v = view(docs);
    let rows = v.rows.clone();
    v.columns(&Rows::new(&rows, v.level()));
    v.hidden_mut().insert("nested".into());
    v.move_column("name", Some("_id"));
    v.selection.selected = BTreeSet::from([0, 2]);
    assert_eq!(v.selection_text(',', true), "name,_id,f,'=1+1\nn0,0,,\nn2,2,'=1+1,5");
    assert_eq!(v.selection_text('\t', false), "n0\t0\t\t\nn2\t2\t'=1+1\t5");
    assert_eq!(v.selection_text('\t', true).lines().next(), Some("name\t_id\tf\t'=1+1"), "a formula-like column name is defused in the header");
    v.selection.selected.clear();
    assert_eq!(v.selection_text(',', true), "name,_id,f,'=1+1", "no rows: just the header");
}

/// At a drilled level, Copy › Selected as CSV and for Excel start with
/// the `{Document id}` column: each row's document's `_id`, on a column
/// step's level and on one row's elements alike. The top level has none.
#[test]
fn copied_drilled_rows_start_with_the_document_id() {
    let mut v = view(vec![
        doc! { "_id": 1, "gameName": { "en": "Dragon", "th": "มังกร" }, "tags": ["x"] },
        doc! { "_id": "two", "gameName": { "en": "Wheel" }, "tags": ["a", "b"] },
    ]);
    v.selection.selected = BTreeSet::from([0, 1]);
    assert!(v.selection_text(',', true).starts_with("_id,gameName,tags\n"), "{}", v.selection_text(',', true));
    v.set_drill(vec![Step::Field("gameName".into())]);
    v.selection.selected = BTreeSet::from([0, 1]);
    assert_eq!(v.selection_text(',', true), "{Document id},en,th\n1,Dragon,มังกร\ntwo,Wheel,");
    assert_eq!(v.selection_text('\t', false), "1\tDragon\tมังกร\ntwo\tWheel\t");
    // Hidden and moved columns leave it first.
    v.columns(&Rows::new(&v.rows.clone(), v.level()));
    v.move_column("th", Some("en"));
    v.hidden_mut().insert("en".into());
    assert_eq!(v.selection_text(',', true), "{Document id},th\n1,มังกร\ntwo,");
    v.set_drill(vec![Step::Elements { row: 1, doc: DocKey { place: 1, id: Some(Bson::String("two".into())) }, field: "tags".into() }]);
    v.selection.selected = BTreeSet::from([0, 1]);
    assert_eq!(v.selection_text(',', true), "{Document id},(value)\ntwo,a\ntwo,b");
}

/// The columns a table lays out are those in view and one more each
/// side; a spacer stands for those before and one for those after, as
/// wide as they are with the gaps between them, so each column laid out
/// starts where it would with them all.
#[test]
fn the_table_lays_out_the_columns_in_view() {
    // Column k spans 110k..110k + 100.
    let w = [100.0; 10];
    let at = |from: f32, to: f32| {
        let v = Window::of(&w, 10.0, egui::Rangef::new(from, to));
        (v.lo, v.hi, v.left, v.right)
    };
    assert_eq!(at(0.0, 250.0), (0, 4, 0.0, 650.0), "0 to 2 in view, and 3");
    assert_eq!(at(335.0, 500.0), (2, 6, 210.0, 430.0), "3 and 4 in view, and 2 and 5");
    assert_eq!(at(5_000.0, 5_500.0), (9, 10, 980.0, 0.0), "past the end: the last column");
    assert_eq!(at(0.0, 2_000.0), (0, 10, 0.0, 0.0), "all in view: no spacer");
    let none = Window::of(&[], 10.0, egui::Rangef::new(0.0, 500.0));
    assert_eq!((none.lo, none.hi, none.left, none.right), (0, 0, 0.0, 0.0));

    // Columns of all widths, the view anywhere: the first column laid out
    // starts where it would with them all, the spacer after the last one
    // ends where the table does, and the columns laid out cover the view.
    let (w, gap): (Vec<f32>, f32) = ((0..30).map(|k| 40.0 + (k * 37 % 200) as f32).collect(), 7.0);
    let starts: Vec<f32> = w.iter().scan(0.0, |x, w| {
        let start = *x;
        *x += w + gap;
        Some(start)
    }).collect();
    let table_end = starts[29] + w[29];
    for from in (0..80).map(|i| i as f32 * 97.0 - 300.0) {
        let view = egui::Rangef::new(from, from + 500.0);
        let v = Window::of(&w, gap, view);
        assert!(v.lo < v.hi, "{from}: something laid out");
        let first = if v.lo == 0 { 0.0 } else { v.left + gap };
        assert_eq!(first, starts[v.lo], "{from}: column {} starts after the spacer", v.lo);
        let last_end = starts[v.hi - 1] + w[v.hi - 1];
        let end = if v.hi == w.len() { last_end } else { last_end + gap + v.right };
        assert_eq!(end, table_end, "{from}: the table ends where it would");
        assert!(starts[v.lo] <= view.min.max(0.0) && last_end >= view.max.min(table_end), "{from}: {}..{} covers the view", v.lo, v.hi);
    }
}

/// A document without an `_id` has an empty `{Document id}` cell.
#[test]
fn a_document_without_an_id_has_an_empty_document_id() {
    let mut v = view(vec![doc! { "o": { "x": "y" } }, doc! { "_id": 2, "o": { "x": "z" } }]);
    v.set_drill(vec![Step::Field("o".into())]);
    let rows = v.resolved();
    assert_eq!(rows.get(0).and_then(|r| rows.cell(r, "{Document id}")), None);
    assert_eq!(rows.get(1).and_then(|r| rows.cell(r, "{Document id}")), Some(&Bson::Int32(2)));
    drop(rows);
    v.selection.selected = BTreeSet::from([0, 1]);
    assert_eq!(v.selection_text(',', true), "{Document id},x\n,y\n2,z");
}

/// A real field named `{Document id}` is a column like any other at the
/// top level; at a drilled level the table shows one `{Document id}`, the
/// document's `_id` (README, Limitations).
#[test]
fn a_field_named_document_id_gives_way_when_drilled() {
    let mut v = view(vec![doc! { "_id": 1, "{Document id}": "TOP", "o": { "{Document id}": "REAL", "x": "y" } }]);
    v.selection.selected = BTreeSet::from([0]);
    assert_eq!(v.selection_text(',', true).lines().collect::<Vec<_>>(), ["_id,{Document id},o", "1,TOP,\"{\"\"{Document id}\"\":\"\"REAL\"\",\"\"x\"\":\"\"y\"\"}\""]);
    v.set_drill(vec![Step::Field("o".into())]);
    v.selection.selected = BTreeSet::from([0]);
    assert_eq!(v.selection_text(',', true), "{Document id},x\n1,y");
}

/// Copied documents paste back as the same BSON: one document or many,
/// whole-number doubles staying doubles (not Int32 or NumberLong).
#[test]
fn copying_many_documents_pastes_back() {
    let docs = vec![doc! { "_id": 1, "a": "x", "price": 5.0, "zero": -0.0, "big": 1e12, "f": 2.5 }, doc! { "_id": 2, "n": [1.0, { "d": 3.0 }] }];
    let mut v = view(docs.clone());
    for rows in [vec![0, 1], vec![0], vec![1]] {
        v.selection.selected = rows.iter().copied().collect();
        let text = v.copy_text();
        let back = rongo_shell::parse_documents_text(&text).unwrap();
        let want: Vec<Document> = rows.iter().map(|&i| docs[i].clone()).collect();
        assert_eq!(back, want, "{text}");
        // Bson's == tells 5.0 from 5, but not -0.0 from 0.0.
        if rows[0] == 0 {
            assert!(matches!(back[0].get("zero"), Some(Bson::Double(_))), "-0.0 stays a double: {text}");
        }
    }
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

/// A view on a read-only connection offers no writes: no document
/// buttons act, and ⇧⌫ deletes nothing. The same view unlocked does.
#[test]
fn read_only_views_offer_no_writes() {
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;
    use std::collections::HashMap;
    use std::sync::Arc;
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
    let core = rongo_core::CoreHandle::start(rt.handle().clone(), Arc::new(|| {}));
    let conn = ConnId(uuid::Uuid::nil());
    let ns = Namespace::new("db", "c");
    let mut v = ResultView::new(1, "c", Source::Find { conn, spec: FindSpec::all(ns.clone(), 50), max: None });
    v.paging.needs_fetch = false;
    let docs = vec![doc! { "_id": 1, "a": 1 }, doc! { "_id": 2, "a": 2 }];
    v.rows = Docs::from_docs(&docs);
    v.selection.selected = BTreeSet::from([0]);
    assert!(v.editable().is_some(), "a find view is editable until it is locked");
    v.read_only = true;
    assert!(v.editable().is_none());

    struct State {
        view: ResultView,
        actions: Vec<ViewAction>,
        pending: HashMap<RequestId, crate::net::Sent>,
        next: u64,
    }
    let mut h = Harness::builder().with_size(vec2(900.0, 500.0)).build_ui_state(
        |ui, st: &mut State| {
            let mut net = Net { core: &core, pending: &mut st.pending, next_view_id: &mut st.next, keys_enabled: true };
            let acts = st.view.ui(ui, &mut net);
            st.actions.extend(acts.into_iter().map(|(_, a)| a));
        },
        State { view: v, actions: Vec::new(), pending: HashMap::new(), next: 0 },
    );
    h.run_steps(3);
    let is_write = |a: &ViewAction| matches!(a, ViewAction::Insert { .. } | ViewAction::Edit { .. } | ViewAction::Delete { .. } | ViewAction::Clone { .. } | ViewAction::Bulk { .. } | ViewAction::Paste { .. });
    for glyph in [icon::FILE_PLUS, icon::PENCIL_SIMPLE, icon::TRASH] {
        h.get_by_label(glyph).click();
        h.run_steps(3);
    }
    h.key_press_modifiers(egui::Modifiers::SHIFT, Key::Backspace);
    h.run_steps(3);
    assert!(!h.state().actions.iter().any(is_write), "a locked view acted: {}", h.state().actions.len());

    // Unlocked, the same clicks do act.
    h.state_mut().view.read_only = false;
    h.run_steps(3);
    for glyph in [icon::FILE_PLUS, icon::PENCIL_SIMPLE, icon::TRASH] {
        h.get_by_label(glyph).click();
        h.run_steps(3);
    }
    let acts = &h.state().actions;
    assert!(acts.iter().any(|a| matches!(a, ViewAction::Insert { .. })), "Insert");
    assert!(acts.iter().any(|a| matches!(a, ViewAction::Edit { .. })), "Edit");
    assert!(acts.iter().any(|a| matches!(a, ViewAction::Delete { .. })), "Delete");
}

/// A page on its way shows in the status bar (once it takes a moment), not
/// as a spinner beside the view picker as well. (Only an empty view, still
/// waiting for its first page, has one: in the middle, where rows go.)
#[test]
fn a_loading_page_has_no_spinner_by_the_view_picker() {
    use egui_kittest::Harness;
    use egui::accesskit::Role;
    use egui_kittest::kittest::Queryable;
    use std::collections::HashMap;
    use std::sync::Arc;
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
    let core = rongo_core::CoreHandle::start(rt.handle().clone(), Arc::new(|| {}));
    let mut v = ResultView::new(1, "c", Source::Find { conn: ConnId(uuid::Uuid::nil()), spec: FindSpec::all(Namespace::new("db", "c"), 50), max: None });
    // Rows on screen, the page asked for again (a refresh).
    v.rows = Docs::from_docs(&[doc! { "_id": 1, "a": 1 }]);
    v.paging.needs_fetch = true;
    struct State {
        view: ResultView,
        pending: HashMap<RequestId, crate::net::Sent>,
        next: u64,
    }
    let mut h = Harness::builder().with_size(vec2(900.0, 500.0)).build_ui_state(
        |ui, st: &mut State| {
            let mut net = Net { core: &core, pending: &mut st.pending, next_view_id: &mut st.next, keys_enabled: true };
            st.view.ui(ui, &mut net);
        },
        State { view: v, pending: HashMap::new(), next: 0 },
    );
    h.run_steps(2);
    assert!(h.state().view.paging.loading.is_some(), "the page was asked for (nothing answers it here)");
    assert_eq!(h.state().view.rows.len(), 1, "the rows stay while it loads");
    assert_eq!(h.query_all_by_role(Role::ProgressIndicator).count(), 0);
}

/// A page request discarded from the status bar stops the spinner and
/// keeps the rows already on screen.
#[test]
fn cancelled_page_keeps_the_rows() {
    let raw = |n: i32| (0..n).map(|i| RawDocumentBuf::try_from(&doc! { "_id": i }).unwrap()).collect::<Vec<_>>();
    let mut v = ResultView::new(1, "c", Source::Find { conn: ConnId(uuid::Uuid::nil()), spec: FindSpec::all(Namespace::new("db", "c"), 50), max: None });
    v.paging.needs_fetch = false;
    v.paging.loading = Some(1);
    v.on_page(1, raw(5), false, 1);
    v.paging.loading = Some(2);
    v.paging.counting = Some(3);
    v.on_cancelled(2);
    assert_eq!((v.paging.loading, v.rows.len(), v.error.as_deref()), (None, 5, None));
    assert_eq!(v.paging.counting, Some(3), "only the cancelled request");
    v.on_cancelled(3);
    assert_eq!(v.paging.counting, None);
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
    v.paging.page_size = 50;
    v.paging.needs_fetch = false;
    v.paging.loading = Some(100);
    v.on_page(100, raw(50), true, 1);
    assert_eq!(v.paging.total, None);
    assert!(v.can_go_last(), "enabled before counting");

    let mut net = Net { core: &core, pending: &mut pending, next_view_id: &mut next, keys_enabled: true };
    v.go_last(&mut net);
    let req = v.paging.counting.expect("pressing Last starts a count");
    assert!(!v.can_go_last(), "disabled while it counts");
    assert_eq!(v.paging.offset, 0);
    v.on_count(req, 1234);
    assert_eq!((v.paging.offset, v.paging.needs_fetch), (1200, true), "jumped to the page holding document 1234");

    // Counted already: Last jumps straight there.
    v.paging.offset = 0;
    v.paging.needs_fetch = false;
    v.paging.loading = None;
    v.go_last(&mut net);
    assert_eq!(v.paging.offset, 1200);
    assert!(v.paging.counting.is_none(), "no second count");

    // Next page pressed while the count runs: the count does not move it.
    v.set_spec(FindSpec::all(Namespace::new("db", "c"), 50));
    v.paging.loading = None;
    v.paging.has_more = true;
    v.go_last(&mut net);
    let req = v.paging.counting.unwrap();
    v.paging.offset = 50;
    v.fetch(&mut net);
    v.paging.loading = None;
    v.on_count(req, 1234);
    assert_eq!(v.paging.offset, 50, "the later Next page wins");
    assert_eq!(v.paging.total, Some(1234));
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
    v.paging.needs_fetch = false;
    v.paging.loading = Some(100);
    v.on_page(100, raw(5), false, 1);

    // Next page requested; a row of the old page is clicked meanwhile.
    v.paging.loading = Some(101);
    v.click_row(2, Modifiers::NONE);
    v.on_page(101, raw(5), false, 1);
    assert!(v.selection.selected.is_empty(), "the click was on the old page");

    // Count Documents, then a new query before the count returns.
    v.paging.counting = Some(102);
    v.set_spec(FindSpec { filter: doc! { "x": 1 }, ..FindSpec::all(ns.clone(), 50) });
    v.on_count(102, 999);
    assert_eq!(v.paging.total, None, "the old query's count");

    // The new query starts even though a page is still loading.
    v.paging.loading = Some(103);
    let mut frame = |v: &mut ResultView| {
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            let mut net = Net { core: &core, pending: &mut pending, next_view_id: &mut next, keys_enabled: true };
            v.ui(ui, &mut net);
        });
        out.textures_delta.clear();
    };
    frame(&mut v);
    assert!(v.paging.loading.is_some_and(|l| l != 103), "a new request replaced the slow one");

    // Refresh Document on this page, then the page changes, then the
    // reply (document gone) arrives: it must not delete row 0 of the new page.
    let id = v.paging.loading.unwrap();
    v.on_page(id, raw(5), false, 1);
    v.refreshing = vec![(200, Bson::Int32(0))];
    v.refresh();
    frame(&mut v);
    let id = v.paging.loading.unwrap();
    v.on_page(id, raw(5), false, 1);
    v.on_doc(200, Bson::Int32(0), Vec::new());
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
