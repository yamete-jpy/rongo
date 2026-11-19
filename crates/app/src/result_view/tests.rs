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

#[test]
fn step_into_picks_the_right_step() {
    let mut v = view(vec![doc! { "o": { "a": 1 }, "arr": [1, 2], "s": "x" }]);
    assert!(!v.step_into(0, "s", false));
    assert!(v.step_into(0, "arr", false));
    assert_eq!(v.drill, [Step::Elements { row: 0, doc: DocKey { place: 0, id: None }, field: "arr".into() }]);
    v.set_drill(Vec::new());
    assert!(v.step_into(0, "o", true));
    assert_eq!(v.drill, [Step::Cell { row: 0, doc: DocKey { place: 0, id: None }, field: "o".into() }]);
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
