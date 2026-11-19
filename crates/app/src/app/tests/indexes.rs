//! The Index Manager's index dialogs: View details, Edit index and Add
//! as New Index…, all the Add Index form filled from an index.

use super::*;
use crate::index_form::{IndexForm, Purpose};
use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;

/// The indexes the Index Manager on screen lists, by name.
fn index_names(app: &RongoApp) -> Vec<String> {
    match &app.tabs[app.active_tab].kind {
        TabKind::Indexes(t) => t.items_for_test().iter().map(|i| i.spec.get_str("name").unwrap_or_default().to_string()).collect(),
        _ => Vec::new(),
    }
}

/// The index form open, if any.
fn form(app: &RongoApp) -> Option<&IndexForm> {
    match app.tool.as_ref().map(|t| &t.kind) {
        Some(ToolKind::Index(f)) => Some(f),
        _ => None,
    }
}

/// What the open index form is for, and the index it was filled from.
fn opened(h: &Harness<'_, RongoApp>) -> Option<(Purpose, String)> {
    form(h.state()).map(|f| (f.purpose, f.listed().and_then(|l| l.get_str("name").ok()).unwrap_or_default().to_string()))
}

/// Press and release `button` at `at`, a frame each.
fn click_at(h: &mut Harness<'_, RongoApp>, at: egui::Pos2, button: egui::PointerButton) {
    h.event(egui::Event::PointerMoved(at));
    h.run_steps(1);
    for pressed in [true, false] {
        h.event(egui::Event::PointerButton { pos: at, button, pressed, modifiers: Modifiers::NONE });
        h.run_steps(1);
    }
    h.run_steps(2);
}

/// The middle of the Index Manager's row for the index `name`.
fn row(h: &Harness<'_, RongoApp>, name: &str) -> egui::Pos2 {
    let end = format!("  {name}");
    h.output()
        .shapes
        .iter()
        .find_map(|s| match &s.shape {
            egui::Shape::Text(t) if t.galley.text().ends_with(&end) => Some(t.galley.rect.translate(t.pos.to_vec2()).center()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("the row of {name}"))
}

/// Clicks the widget labelled `label`, or else whose label holds it (the
/// lowest one on screen: a menu's item rather than the toolbar's button),
/// then lets the dialog it opens settle.
fn click(h: &mut Harness<'_, RongoApp>, label: &str) {
    let lowest = |a: &egui_kittest::Node<'_>, b: &egui_kittest::Node<'_>| a.rect().top().total_cmp(&b.rect().top());
    let node = h.query_all_by_label(label).max_by(lowest).or_else(|| h.query_all_by_label_contains(label).max_by(lowest)).unwrap_or_else(|| panic!("{label} on screen"));
    node.click();
    h.run_steps(4);
}

/// The demo app with an Index Manager open on a collection of its own
/// holding a TTL index, `at_ttl`, whose row is selected.
fn index_manager(coll: &str) -> (Harness<'static, RongoApp>, ConnId, Namespace) {
    let (h, cid, ns, _) = index_manager_with(coll, doc! { "at": 1 }, doc! { "name": "at_ttl", "expireAfterSeconds": 3600 });
    (h, cid, ns)
}

/// The demo app with an Index Manager open on a collection of its own
/// holding one index of these keys and options, whose row is selected;
/// and that index's name.
fn index_manager_with(coll: &str, keys: Document, options: Document) -> (Harness<'static, RongoApp>, ConnId, Namespace, String) {
    let mut h = harness(egui::vec2(1400.0, 900.0), true);
    let cid = ConnectionProfile::mock().conn_id();
    harness_until(&mut h, "the demo connects", |a| matches!(a.conns.get(&cid).map(|c| &c.status), Some(Status::Connected(_))) && a.requests.pending.is_empty());
    let ns = Namespace::new("demo-games", coll);
    let name = options.get_str("name").map(str::to_string).unwrap_or_else(|_| rongo_core::conn::default_index_name(&keys));
    let index = Command::CreateIndex { ns: ns.clone(), keys, options: Some(options) };
    h.state_mut().net().send(Origin::Notify { view: None, tree: None }, Some(cid), index);
    harness_until(&mut h, "the index is created", writes_done);
    h.state_mut().open_indexes(cid, ns.clone());
    harness_until(&mut h, "the indexes are listed", |a| index_names(a).len() == 2);
    let at = row(&h, &name);
    click_at(&mut h, at, egui::PointerButton::Primary);
    (h, cid, ns, name)
}

/// The Index Manager's View details (its toolbar's and the row's menu's)
/// and Edit index open the Add Index form filled from the selected index.
#[test]
fn index_manager_opens_view_and_edit_as_the_index_form() {
    let (mut h, _, _) = index_manager("idx_open");
    let want = |p| Some((p, "at_ttl".to_string()));
    click(&mut h, "View details");
    assert_eq!(opened(&h), want(Purpose::View), "the toolbar's View details");
    assert!(h.state().view_doc.is_none(), "not the document viewer");
    click(&mut h, "Cancel");
    assert!(h.state().tool.is_none());

    click(&mut h, "Edit index");
    assert_eq!(opened(&h), want(Purpose::Edit), "the toolbar's Edit index");
    click(&mut h, "Cancel");

    let at = row(&h, "at_ttl");
    click_at(&mut h, at, egui::PointerButton::Secondary);
    click(&mut h, "View details");
    assert_eq!(opened(&h), want(Purpose::View), "the row's menu's View details");
}

/// Edit index's OK changes the index on the server, only what changed;
/// Add as New Index… opens Add Index filled from it with no name, and
/// creating that (with its key now descending: MongoDB keeps one index per
/// key pattern) leaves the old index where it was.
#[test]
fn edit_saves_and_add_as_new_index_adds_one_beside_it() {
    let (mut h, cid, ns) = index_manager("idx_edit");
    let listed = |h: &Harness<'_, RongoApp>, name: &str| match &h.state().tabs[h.state().active_tab].kind {
        TabKind::Indexes(t) => t.items_for_test().iter().find(|i| i.spec.get_str("name").ok() == Some(name)).map(|i| i.spec.clone()),
        _ => None,
    };
    click(&mut h, "Edit index");
    click(&mut h, "Options");
    click(&mut h, "Hidden");
    assert!(form(h.state()).is_some_and(|f| f.hidden && f.ready()), "Hidden ticked, OK on");
    click(&mut h, "OK");
    harness_until(&mut h, "the change is saved and listed", |a| a.tool.is_none() && writes_done(a) && index_names(a).len() == 2);
    harness_until(&mut h, "the Index Manager shows it", |a| matches!(&a.tabs[a.active_tab].kind, TabKind::Indexes(t) if t.items_for_test().iter().any(|i| i.spec.get_bool("hidden").is_ok_and(|h| h))));
    assert_eq!(listed(&h, "at_ttl"), Some(doc! { "v": 2, "key": { "at": 1 }, "name": "at_ttl", "expireAfterSeconds": 3600, "hidden": true }), "only Hidden changed");

    click(&mut h, "Edit index");
    click(&mut h, "Add as New Index…");
    let f = form(h.state()).expect("Add Index is open");
    assert_eq!(f.purpose, Purpose::Add);
    let (keys, options) = f.definition().expect("filled");
    assert_eq!(keys, doc! { "at": 1 });
    assert_eq!(options, doc! { "expireAfterSeconds": 3600_i64, "hidden": true }, "no name: MongoDB's default");
    if let Some(ToolKind::Index(f)) = h.state_mut().tool.as_mut().map(|t| &mut t.kind) {
        f.hidden = false;
        f.fields[0].1 = crate::index_form::KeyType::Desc;
    }
    h.run_steps(2);
    click(&mut h, "Create index");
    harness_until(&mut h, "the new index is created and listed", |a| a.tool.is_none() && writes_done(a) && index_names(a).len() == 3);
    assert_eq!(index_names(h.state()), ["_id_", "at_ttl", "at_-1"], "the old index stays");
    assert!(toast_texts(h.state()).iter().any(|t| t == "Index created: Created index at_-1"), "{:?}", toast_texts(h.state()));
    let indexes = match ask_core(h.state(), cid, Command::IndexDetails { ns: ns.clone() }) {
        Event::IndexDetails { items, .. } => items,
        other => panic!("listed {other:?}"),
    };
    assert_eq!(indexes.len(), 3, "nothing was dropped");
}

/// Add as New Index… of an index with MongoDB's default name: with no
/// name typed, the new index would take that very name, so Create index
/// waits for one and says why. Named, but with the same key pattern and
/// only Unique changed, it is refused by the server (MongoDB keeps one
/// index per key pattern, as the form notes): the error shows in the
/// dialog and nothing is created.
#[test]
fn add_as_new_index_of_a_default_named_index() {
    use egui_kittest::kittest::NodeT;
    let (mut h, _, _, name) = index_manager_with("idx_default", doc! { "email": 1 }, doc! {});
    assert_eq!(name, "email_1");
    click(&mut h, "View details");
    click(&mut h, "Add as New Index…");
    assert_eq!(form(h.state()).map(|f| f.purpose), Some(Purpose::Add));
    assert!(crate::test_support::disabled(&h, "Create index"), "with no name it would be email_1");
    assert!(h.query_by_label_contains("type a name").is_some(), "says why");
    assert!(h.query_by_label_contains("one index per key pattern").is_some(), "notes the key pattern");
    click(&mut h, "Options");
    click(&mut h, "Unique");
    assert!(form(h.state()).is_some_and(|f| f.unique), "Unique ticked");
    assert!(crate::test_support::disabled(&h, "Create index"), "still no name");

    let field = h.query_all_by_role(egui::accesskit::Role::TextInput).find(|n| n.accesskit_node().placeholder().is_some_and(|p| p.starts_with("leave empty"))).expect("the name field");
    field.focus();
    h.run_steps(1);
    crate::test_support::type_in(&mut h, "email_unique");
    assert!(!crate::test_support::disabled(&h, "Create index"), "named");
    click(&mut h, "Create index");
    harness_until(&mut h, "the server answers", |a| writes_done(a) && a.tool.as_ref().is_some_and(|t| t.error.is_some()));
    let error = h.state().tool.as_ref().and_then(|t| t.error.clone()).unwrap_or_default();
    assert!(error.contains("equivalent index"), "the server's refusal: {error}");
    assert_eq!(index_names(h.state()), ["_id_", "email_1"], "nothing was created");
}

/// Add Index of an index that is already there, identical: the server
/// builds nothing, and rongo says so (as a problem, in the toast and the
/// Operations panel), never "Created index".
#[test]
fn adding_an_identical_index_says_nothing_was_created() {
    let (mut h, cid, ns, _) = index_manager_with("idx_same", doc! { "email": 1 }, doc! {});
    h.state_mut().toasts.clear();
    h.state_mut().add_index_dialog(cid, ns);
    h.run_steps(3);
    if let Some(ToolKind::Index(f)) = h.state_mut().tool.as_mut().map(|t| &mut t.kind) {
        f.fields = vec![("email".into(), crate::index_form::KeyType::Asc)];
    }
    h.run_steps(2);
    click(&mut h, "Create index");
    harness_until(&mut h, "the server answers", |a| a.tool.is_none() && writes_done(a));
    let toasts = toast_texts(h.state());
    assert!(toasts.iter().any(|t| t == "Index email_1 already exists; nothing was created"), "{toasts:?}");
    assert!(!toasts.iter().any(|t| t.contains("reated index") || t.starts_with("Index created")), "{toasts:?}");
    assert!(h.state().toasts.iter().any(|t| t.error), "shown as a problem");
    assert_eq!(index_names(h.state()), ["_id_", "email_1"]);
}

/// On a read-only connection View details still opens, read-only as
/// ever; its Add as New Index… is greyed out, and Edit index is too.
#[test]
fn view_details_works_on_a_read_only_connection() {
    let (mut h, cid, _) = index_manager("idx_locked");
    h.state_mut().tree_action(TreeAction::SetReadOnly(cid, true));
    harness_until(&mut h, "the lock", |a| a.conns[&cid].profile.read_only);
    h.run_steps(2);
    assert!(crate::test_support::disabled(&h, &format!("{}  Edit index", icon::PENCIL_SIMPLE)), "Edit index is off");
    click(&mut h, "View details");
    assert_eq!(opened(&h), Some((Purpose::View, "at_ttl".to_string())));
    assert!(crate::test_support::disabled(&h, "Add as New Index…"), "locked: no new index from it");
    assert!(crate::test_support::disabled(&h, "OK"));
}
