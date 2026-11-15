//! Connections and the tree: connecting, locking, listing, tree actions.

use super::*;

/// Export Connection opens from the tree (an open connection) and from
/// the Connection Manager (any saved one), over the manager, which stays;
/// the demo server has nothing to export.
#[test]
fn export_connection_opens_from_the_tree_and_the_manager() {
    let (_ctx, mut app) = test_app(false);
    let a = ConnectionProfile::new("a");
    app.store.as_mut().expect("a store of the test's own").upsert(a.clone(), None, None, app.vault.as_deref().unwrap()).unwrap();
    app.conns.insert(a.conn_id(), crate::sidebar::OpenConn::new(a.clone()));
    app.tree_action(TreeAction::ExportConnection(a.conn_id()));
    assert_eq!(app.export_conn.as_ref().map(|d| d.uri()).as_deref(), Some("mongodb://localhost:27017"));
    assert!(app.modal_open());

    app.export_conn = None;
    app.connect_dialog = Some(ConnectDialog::new());
    app.connect_action(ConnectAction::Export(a.id));
    assert!(app.export_conn.is_some() && app.connect_dialog.is_some(), "over the manager, which stays");

    app.export_conn = None;
    let demo = ConnectionProfile::mock();
    app.conns.insert(demo.conn_id(), crate::sidebar::OpenConn::new(demo.clone()));
    app.tree_action(TreeAction::ExportConnection(demo.conn_id()));
    assert!(app.export_conn.is_none());
}

/// Colouring and moving a connection save it, and an open connection
/// shows the colour at once.
#[test]
fn colour_and_order_are_saved() {
    let (_ctx, mut app) = test_app(false);
    let vault = app.vault.clone().expect("tests start unlocked");
    let store = app.store.as_mut().expect("a store of the test's own");
    assert!(store.profiles().is_empty(), "not the real saved connections");
    let [a, b] = ["a", "b"].map(ConnectionProfile::new);
    store.upsert(a.clone(), None, None, &vault).unwrap();
    store.upsert(b.clone(), None, None, &vault).unwrap();
    app.conns.insert(a.conn_id(), crate::sidebar::OpenConn::new(a.clone()));

    app.tree_action(TreeAction::SetColor(a.conn_id(), Some("red".into())));
    assert_eq!(app.conns[&a.conn_id()].profile.color.as_deref(), Some("red"));
    assert_eq!(app.store.as_ref().unwrap().get(a.id).unwrap().color.as_deref(), Some("red"));

    app.tree_action(TreeAction::MoveConnection { id: b.conn_id(), before: Some(a.conn_id()) });
    let names: Vec<_> = app.store.as_ref().unwrap().profiles().iter().map(|p| p.name.clone()).collect();
    assert_eq!(names, ["b", "a"]);
}

/// Locking a connection saves its profile, marks the open connection and
/// tells the core, so even shell tabs on it are refused writes.
#[test]
fn locking_saves_the_profile_and_tells_the_core() {
    let (ctx, mut app, cid) = connected_mock_app();
    assert!(!app.requests.core.session(cid).unwrap().conn.is_read_only());
    app.tree_action(TreeAction::SetReadOnly(cid, true));
    assert!(app.store.as_ref().unwrap().get(cid.0).unwrap().read_only, "saved");
    assert!(app.conns[&cid].profile.read_only, "the open connection shows it");
    step_until(&ctx, &mut app, "the core locks", |a| a.requests.core.session(cid).unwrap().conn.is_read_only());
}

/// Unlocking asks first, naming the connection; the flag stays on until
/// that is confirmed, and then clears in the profile, the connection and
/// the core.
#[test]
fn unlocking_asks_first() {
    let (ctx, mut app, cid) = connected_mock_app();
    app.tree_action(TreeAction::SetReadOnly(cid, true));
    step_until(&ctx, &mut app, "the core locks", |a| a.requests.core.session(cid).unwrap().conn.is_read_only());

    app.tree_action(TreeAction::SetReadOnly(cid, false));
    let ask = app.confirm.as_ref().expect("a confirmation");
    assert_eq!(ask.title, "Allow Writes");
    assert!(ask.message.contains(&app.conns[&cid].profile.name), "names the connection: {}", ask.message);
    assert!(app.conns[&cid].profile.read_only && app.store.as_ref().unwrap().get(cid.0).unwrap().read_only, "still locked");
    assert!(app.requests.core.session(cid).unwrap().conn.is_read_only());

    let c = app.confirm.take().unwrap();
    app.confirmed(c);
    assert!(!app.conns[&cid].profile.read_only);
    assert!(!app.store.as_ref().unwrap().get(cid.0).unwrap().read_only, "saved unlocked");
    step_until(&ctx, &mut app, "the core unlocks", |a| !a.requests.core.session(cid).unwrap().conn.is_read_only());
}

/// A locked connection's tabs are told so before they draw: their views
/// offer no writes, and the tab bar shows a lock.
#[test]
fn locked_connections_lock_their_views() {
    let (ctx, mut app, cid) = connected_mock_app();
    app.tree_action(TreeAction::OpenCollectionTab(cid, Namespace::new("demo-games", "providers")));
    let tab = app.tabs.len() - 1;
    let view_locked = |a: &RongoApp| matches!(&a.tabs[tab].kind, TabKind::Collection(c) if c.view.read_only);
    step_until(&ctx, &mut app, "the tab draws", |a| matches!(&a.tabs[tab].kind, TabKind::Collection(c) if !c.view.read_only));
    assert!(!view_locked(&app));
    app.tree_action(TreeAction::SetReadOnly(cid, true));
    step_until(&ctx, &mut app, "the view is locked", view_locked);
}

/// Saving the connection form with the lock cleared on a locked profile
/// saves the other changes but leaves it locked until "Allow Writes" is
/// confirmed (the same prompt as the tree menu's).
#[test]
fn unlocking_in_the_form_asks_first() {
    let (ctx, mut app, cid) = connected_mock_app();
    app.tree_action(TreeAction::SetReadOnly(cid, true));
    step_until(&ctx, &mut app, "the core locks", |a| a.requests.core.session(cid).unwrap().conn.is_read_only());

    let mut edited = app.store.as_ref().unwrap().get(cid.0).unwrap().clone();
    edited.read_only = false;
    edited.name = "Renamed demo".into();
    app.connect_action(ConnectAction::Save { profile: edited, password: None, ssh_secret: None, then_connect: false });
    let saved = app.store.as_ref().unwrap().get(cid.0).unwrap();
    assert_eq!(saved.name, "Renamed demo", "the rest of the form saved");
    assert!(saved.read_only, "the lock is kept until confirmed");
    assert!(app.conns[&cid].profile.read_only);
    assert!(app.requests.core.session(cid).unwrap().conn.is_read_only());
    let ask = app.confirm.as_ref().expect("an Allow Writes confirmation");
    assert_eq!(ask.title, "Allow Writes");
    assert!(ask.message.contains("Renamed demo"), "names the connection: {}", ask.message);

    let c = app.confirm.take().unwrap();
    app.confirmed(c);
    assert!(!app.store.as_ref().unwrap().get(cid.0).unwrap().read_only);
    assert!(!app.conns[&cid].profile.read_only);
    step_until(&ctx, &mut app, "the core unlocks", |a| !a.requests.core.session(cid).unwrap().conn.is_read_only());

    // Locking through the form needs no confirmation.
    let mut edited = app.store.as_ref().unwrap().get(cid.0).unwrap().clone();
    edited.read_only = true;
    app.connect_action(ConnectAction::Save { profile: edited, password: None, ssh_secret: None, then_connect: false });
    assert!(app.confirm.is_none());
    assert!(app.store.as_ref().unwrap().get(cid.0).unwrap().read_only && app.conns[&cid].profile.read_only);
}

/// Connect with several rows picked in the Connection Manager opens them
/// all, though the first one closes the dialog.
#[test]
fn connect_opens_every_picked_profile() {
    let (_ctx, mut app) = test_app(false);
    // In-memory demo servers: nothing goes over the network or to the vault.
    let [a, b] = [1, 2].map(|n| ConnectionProfile { id: uuid::Uuid::from_u128(n), ..ConnectionProfile::mock() });
    let vault = app.vault.clone().expect("tests start unlocked");
    let store = app.store.as_mut().expect("a store of the test's own");
    assert!(store.profiles().is_empty(), "not the real saved connections");
    store.upsert(a.clone(), None, None, &vault).unwrap();
    store.upsert(b.clone(), None, None, &vault).unwrap();
    app.connect_dialog = Some(ConnectDialog::new());
    for action in [ConnectAction::Connect(a.id), ConnectAction::Connect(b.id)] {
        app.connect_action(action);
    }
    assert!(app.connect_dialog.is_none());
    assert!(app.conns.contains_key(&a.conn_id()) && app.conns.contains_key(&b.conn_id()), "both opened");
}

/// Drop Database's button waits for the database's exact name.
#[test]
fn drop_database_needs_the_exact_name() {
    let (_ctx, mut app) = test_app(false);
    let cid = ConnectionProfile::mock().conn_id();
    app.tree_action(TreeAction::DropDatabase(cid, "shop".into()));
    let typed = |app: &mut RongoApp, t: &str| {
        let c = app.confirm.as_mut().expect("asks first");
        c.input = Some(t.into());
        c.ok_enabled()
    };
    assert!(!typed(&mut app, ""));
    assert!(!typed(&mut app, "sh"));
    assert!(!typed(&mut app, "shopping"));
    assert!(typed(&mut app, "shop"));
    assert!(typed(&mut app, " shop "), "spaces around it are fine");
}

/// ⌘R and ⇧⌘R do what the tree's menus say when the pointer is over
/// the tree: ⇧⌘R reloads the selected database's collections, ⌘R
/// reloads every connected connection. Away from the tree they are left
/// for the results.
#[test]
fn tree_refresh_shortcuts() {
    use crate::sidebar::{CollNode, DbNode, OpenConn};
    use rongo_core::{CollInfo, CollKind, DbInfo};
    let (ctx, mut app) = test_app(false);
    let p = ConnectionProfile::mock();
    let cid = p.conn_id();
    let mut conn = OpenConn::new(p);
    conn.status = Status::Connected(server_info());
    let coll = CollNode::new(CollInfo { name: "c".into(), kind: CollKind::Collection });
    conn.dbs = Some(vec![DbNode { info: DbInfo { name: "db".into(), size_on_disk: 0 }, colls: Some(vec![coll]), loading: false }]);
    app.conns.insert(cid, conn);
    app.sidebar.open = true;
    app.sidebar.selection = crate::sidebar::Selection { conn: Some(cid), db: Some("db".into()), coll: None };
    app.sidebar_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(200.0, 600.0));

    let press = |app: &mut RongoApp, at: egui::Pos2, m: Modifiers| {
        let key = egui::Event::Key { key: Key::R, physical_key: None, pressed: true, repeat: false, modifiers: m };
        let events = vec![egui::Event::PointerMoved(at), egui::Event::ModifiersChanged(m), key];
        let mut left = false;
        let mut out = ctx.run_ui(egui::RawInput { events, ..Default::default() }, |ui| {
            app.shortcuts(ui.ctx());
            left = ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Key { key: Key::R, .. })));
        });
        out.textures_delta.clear();
        left
    };
    let colls = |app: &RongoApp| app.conns[&cid].dbs.as_ref().unwrap()[0].colls.is_some();
    let over = egui::pos2(100.0, 100.0);
    let away = egui::pos2(500.0, 100.0);

    assert!(press(&mut app, away, Modifiers::COMMAND | Modifiers::SHIFT), "left for the results");
    assert!(colls(&app), "away from the tree, the tree is untouched");
    assert!(!press(&mut app, over, Modifiers::COMMAND | Modifiers::SHIFT), "the tree took ⇧⌘R");
    assert!(!colls(&app), "the database's collections will reload");
    assert!(!app.conns[&cid].loading_dbs, "only the selected database");

    assert!(!press(&mut app, over, Modifiers::COMMAND), "the tree took ⌘R");
    assert!(app.conns[&cid].loading_dbs, "every connection reloads");
}

/// Disconnect drops the connection's shell tabs: a script running in
/// one is stopped first, as its Stop button does (it used to go on
/// writing with no tab to stop it from), and one toast says so.
#[test]
fn disconnect_stops_running_scripts() {
    let (ctx, mut app, cid) = connected_mock_app();
    // The script's own connection, which outlives the core's.
    let session = app.requests.core.session(cid).expect("connected");
    let ns = Namespace::new("demo-games", "endless");
    let count = || session.rt.block_on(session.conn.count(&ns, doc! {})).unwrap();
    // The demo tab's own script first runs to its end.
    step_until(&ctx, &mut app, "the demo script finishes", |a| !a.tabs.iter().any(|t| matches!(&t.kind, TabKind::Shell(s) if s.is_running() || s.run_when_ready)));
    app.open_shell(cid, Some("demo-games".into()), Some("for (let i = 0; ; i++) { db.endless.insertOne({ i: i }); sleep(5) }".into()));
    let shell_tab = app.tabs.len() - 1;
    if let TabKind::Shell(s) = &mut app.tabs[shell_tab].kind {
        s.run_when_ready = true;
    }
    step_until(&ctx, &mut app, "the script runs", |a| matches!(&a.tabs[shell_tab].kind, TabKind::Shell(s) if s.is_running()));
    let end = Instant::now() + WAIT;
    while count() == 0 {
        assert!(Instant::now() < end, "the script never wrote");
        std::thread::sleep(Duration::from_millis(10));
    }
    app.tree_action(TreeAction::Disconnect(cid));
    assert!(app.tabs.iter().all(|t| t.conn() != Some(cid)), "its tabs are gone");
    assert_eq!(toast_texts(&app), ["Stopped 1 running script"]);
    // Running, it writes about one document every 5 ms.
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        let before = count();
        std::thread::sleep(Duration::from_millis(300));
        if count() == before {
            break;
        }
        assert!(Instant::now() < end, "the script goes on writing after Disconnect");
    }
}

/// Only the latest Connect speaks for a connection. An earlier attempt,
/// given up on by Disconnect (then Connect again, as Save & Connect
/// does), used to mark the working connection failed when it timed out
/// at last; and one that failed after Disconnect toasted ": error".
#[test]
fn a_stale_connect_answer_changes_nothing() {
    let (_ctx, mut app) = test_app(false);
    let p = ConnectionProfile::mock();
    let cid = p.conn_id();
    let latest = |app: &RongoApp| app.requests.pending.iter().filter(|(_, s)| matches!(s.origin, Origin::Connect(c) if c == cid)).map(|(id, _)| *id).max().expect("a Connect");
    let timed_out = || Event::Failed(CoreError::Other("server selection timed out".into()));
    app.connect(p.clone());
    let first = latest(&app);
    app.tree_action(TreeAction::Disconnect(cid));
    app.connect(p.clone());
    let second = latest(&app);
    app.handle(Response { id: second, conn: None, event: Event::Connected(server_info()) });
    app.handle(Response { id: first, conn: None, event: timed_out() });
    assert!(matches!(app.conns[&cid].status, Status::Connected(_)), "the working connection stays connected");
    assert!(app.toasts.is_empty(), "{:?}", toast_texts(&app));

    // Given up on by Disconnect: its failure says nothing.
    app.tree_action(TreeAction::Disconnect(cid));
    app.connect(p.clone());
    let third = latest(&app);
    app.tree_action(TreeAction::Disconnect(cid));
    app.handle(Response { id: third, conn: None, event: timed_out() });
    assert!(app.toasts.is_empty(), "{:?}", toast_texts(&app));
    assert!(!app.conns.contains_key(&cid));
}

/// Waits until the core holds (or no longer holds) a connection for `cid`.
fn core_holds(app: &RongoApp, cid: ConnId, held: bool, what: &str) {
    let end = Instant::now() + Duration::from_secs(10);
    while app.requests.core.session(cid).is_some() != held {
        assert!(Instant::now() < end, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The current Connect failing (here its answer replaced by a failure,
/// though the core had connected) also tells the core to drop the
/// connection, so the core never keeps one the tree shows as Failed.
#[test]
fn a_failed_connect_is_dropped_in_the_core_too() {
    let (_ctx, mut app) = test_app(false);
    let p = ConnectionProfile::mock();
    let cid = p.conn_id();
    app.connect(p);
    let id = app.connecting[&cid];
    core_holds(&app, cid, true, "the core connects");
    app.handle(Response { id, conn: None, event: Event::Failed(CoreError::Timeout("server selection timed out".into())) });
    assert!(matches!(app.conns[&cid].status, Status::Failed(_)));
    core_holds(&app, cid, false, "the core drops it");
}

/// A Connect answered after its connection was given up on: when the
/// connection is no longer open, the core is told to drop what it just
/// registered; when it was opened again (a newer Connect), that answer
/// changes nothing, and the newer one's speaks for it.
#[test]
fn a_stale_connected_answer_is_dropped_unless_reopened() {
    let (_ctx, mut app) = test_app(false);
    let p = ConnectionProfile::mock();
    let cid = p.conn_id();

    // Given up on, and the core connected all the same (its Disconnect got there first).
    app.connect(p.clone());
    let first = app.connecting[&cid];
    core_holds(&app, cid, true, "the core connects");
    app.conns.remove(&cid);
    app.connecting.remove(&cid);
    app.handle(Response { id: first, conn: None, event: Event::Connected(server_info()) });
    assert!(!app.conns.contains_key(&cid), "not brought back");
    core_holds(&app, cid, false, "the core drops it");

    // Opened again: the stale answer leaves the newer connection alone.
    app.connect(p.clone());
    let old = app.connecting[&cid];
    core_holds(&app, cid, true, "the core connects");
    app.tree_action(TreeAction::Disconnect(cid));
    core_holds(&app, cid, false, "Disconnect drops it");
    app.connect(p);
    let new = app.connecting[&cid];
    core_holds(&app, cid, true, "the core connects again");
    app.handle(Response { id: old, conn: None, event: Event::Connected(server_info()) });
    assert!(matches!(app.conns[&cid].status, Status::Connecting), "the stale answer changes nothing");
    std::thread::sleep(Duration::from_millis(200));
    assert!(app.requests.core.session(cid).is_some(), "nor is the newer connection dropped");
    app.handle(Response { id: new, conn: None, event: Event::Connected(server_info()) });
    assert!(matches!(app.conns[&cid].status, Status::Connected(_)));
    assert!(app.requests.core.session(cid).is_some());
}

/// Reconnect leaves a failed connection in the tree when it can't start
/// (the vault changed in another window, so the master password is
/// asked first). It used to vanish, its tabs left titled "?".
#[test]
fn reconnect_keeps_the_connection_when_it_cannot_start() {
    use crate::test_support::TEST_PASSWORD;
    let (_ctx, mut app, other) = two_windows();
    let p = ConnectionProfile::new("p");
    let cid = p.conn_id();
    let mut c = OpenConn::new(p);
    c.status = Status::Failed("timed out".into());
    app.conns.insert(cid, c);
    other.change_password(TEST_PASSWORD, "another password").unwrap();
    app.tree_action(TreeAction::Reconnect(cid));
    assert!(app.vault.is_none(), "asks for the master password again");
    assert!(app.conns.contains_key(&cid), "still in the tree");
}

/// A failed database listing names its connection, and Refresh All
/// reloads only connected ones: a failed or connecting one used to
/// toast an unnamed "not connected" each and list no databases.
#[test]
fn database_listing_failures_name_the_connection() {
    let (_ctx, mut app) = test_app(false);
    let open = |n: u128, name: &str, status: Status| {
        let p = ConnectionProfile { id: uuid::Uuid::from_u128(n), name: name.into(), ..ConnectionProfile::mock() };
        let mut c = OpenConn::new(p);
        c.status = status;
        c
    };
    let (a, b, c) = (ConnId(uuid::Uuid::from_u128(1)), ConnId(uuid::Uuid::from_u128(2)), ConnId(uuid::Uuid::from_u128(3)));
    app.conns.insert(a, open(1, "alpha", Status::Connected(server_info())));
    app.conns.insert(b, open(2, "beta", Status::Failed("timed out".into())));
    app.conns.insert(c, open(3, "gamma", Status::Connecting));
    app.tree_action(TreeAction::RefreshAll);
    let listing: Vec<ConnId> = app.requests.pending.values().filter_map(|s| match s.origin {
        Origin::Databases(c) => Some(c),
        _ => None,
    }).collect();
    assert_eq!(listing, [a], "only the connected one reloads");
    let id = *app.requests.pending.keys().next().unwrap();
    app.handle(Response { id, conn: Some(a), event: Event::Failed(CoreError::Other("not authorized".into())) });
    assert_eq!(toast_texts(&app), ["alpha: not authorized"]);
}

/// A failed field sample isn't kept as "no fields" (autocomplete and
/// the Add Index picker stayed empty until reconnect), and a second
/// isn't sent while one is on its way.
#[test]
fn a_failed_field_sample_is_asked_again() {
    let (_ctx, mut app) = test_app(false);
    let p = ConnectionProfile::mock();
    let cid = p.conn_id();
    let mut c = OpenConn::new(p);
    c.status = Status::Connected(server_info());
    app.conns.insert(cid, c);
    let ns = Namespace::new("d", "c");
    let samples = |app: &RongoApp| app.requests.pending.iter().filter(|(_, s)| matches!(&s.origin, Origin::Fields(c, n) if *c == cid && *n == ns)).map(|(id, _)| *id).collect::<Vec<_>>();
    app.add_index_dialog(cid, ns.clone());
    app.tool = None;
    app.add_index_dialog(cid, ns.clone());
    let sent = samples(&app);
    assert_eq!(sent.len(), 1, "one on its way is enough");
    app.handle(Response { id: sent[0], conn: Some(cid), event: Event::Failed(CoreError::Other("timed out".into())) });
    assert!(!app.conns[&cid].fields.contains_key(&ns), "a failure isn't kept");
    app.tool = None;
    app.add_index_dialog(cid, ns.clone());
    assert_eq!(samples(&app).len(), 1, "asked again");
}

/// Rename and Drop close the collection's tabs (its collection tab and
/// Index Manager) once the server has done it, not before: a refused
/// rename used to lose the open tab with its query, sort and page.
#[test]
fn rename_and_drop_close_tabs_once_done() {
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, cid, "tabs_ren", 1);
    let taken = history_coll(&ctx, &mut app, cid, "tabs_taken", 1);
    let open = |app: &mut RongoApp, ns: &Namespace| {
        app.open_collection(cid, ns.clone());
        app.open_indexes(cid, ns.clone());
    };
    let tabs_on = |app: &RongoApp, ns: &Namespace| {
        app.tabs.iter().filter(|t| matches!(&t.kind, TabKind::Collection(c) if c.ns == *ns) || matches!(&t.kind, TabKind::Indexes(i) if i.ns == *ns)).count()
    };
    let answered = |a: &RongoApp| !a.requests.pending.values().any(|s| matches!(s.origin, Origin::Rename { .. }) || s.what.as_deref().is_some_and(|w| w.starts_with("Dropping")));
    let rename = |app: &mut RongoApp, to: &str| {
        app.tree_action(TreeAction::Rename(cid, ns.clone()));
        let mut c = app.confirm.take().expect("renaming asks for the name");
        c.input = Some(to.into());
        app.confirmed(c);
    };
    open(&mut app, &ns);
    rename(&mut app, "tabs_taken");
    assert_eq!(tabs_on(&app, &ns), 2, "open until the server answers");
    step_until(&ctx, &mut app, "the rename is refused", answered);
    assert_eq!(tabs_on(&app, &ns), 2, "a refused rename keeps them");

    rename(&mut app, "tabs_ren2");
    assert_eq!(tabs_on(&app, &ns), 2);
    step_until(&ctx, &mut app, "the rename is done", answered);
    assert_eq!(tabs_on(&app, &ns), 0, "closed once renamed");

    open(&mut app, &taken);
    app.tree_action(TreeAction::DropCollection(cid, taken.clone()));
    let c = app.confirm.take().expect("dropping asks first");
    app.confirmed(c);
    assert_eq!(tabs_on(&app, &taken), 2, "open until the server answers");
    step_until(&ctx, &mut app, "the drop is done", answered);
    assert_eq!(tabs_on(&app, &taken), 0, "closed once dropped");
}

/// Add Collection, Duplicate and Rename take no blank name: their OK
/// (and ↩) waits for one, so an empty name never reaches the server.
#[test]
fn collection_names_are_never_blank() {
    let (_ctx, mut app) = test_app(false);
    let cid = ConnectionProfile::mock().conn_id();
    let ns = Namespace::new("d", "c");
    for a in [TreeAction::CreateCollection(cid, "d".into()), TreeAction::Duplicate(cid, ns.clone()), TreeAction::Rename(cid, ns.clone())] {
        app.tree_action(a);
        let mut c = app.confirm.take().expect("asks for a name");
        c.input = Some("   ".into());
        assert!(!c.ok_enabled(), "{}", c.title);
    }
}

/// Testing a profile that is already open leaves its connection alone
/// (the test used to replace it in the core, then disconnect it).
#[test]
fn testing_an_open_profile_keeps_its_connection() {
    let (ctx, mut app) = test_app(true);
    let profile = ConnectionProfile::mock();
    let cid = profile.conn_id();
    step_until(&ctx, &mut app, "the demo connects", |a| matches!(a.conns.get(&cid).map(|c| &c.status), Some(Status::Connected(_))) && a.requests.pending.is_empty());
    app.connect_action(ConnectAction::Test(Box::new(ConnectRequest { profile, password: None, ssh_secret: None })));
    step_until(&ctx, &mut app, "the test is answered", |a| !a.requests.pending.values().any(|s| matches!(s.origin, Origin::Test(_))));
    app.conns.get_mut(&cid).unwrap().dbs = None;
    app.refresh_tree(cid);
    step_until(&ctx, &mut app, "the open connection lists its databases", |a| a.conns[&cid].dbs.as_ref().is_some_and(|d| !d.is_empty()));
    let c = &app.conns[&cid];
    assert!(c.dbs.as_ref().is_some_and(|d| !d.is_empty()), "the open connection still answers");
}

/// The index manager loads indexes with sizes, hiding one refreshes it,
/// and the hover card gets collection statistics.
#[test]
fn index_manager_and_hover_stats_on_the_demo_server() {
    let (ctx, mut app) = test_app(true);
    let cid = ConnectionProfile::mock().conn_id();
    step_until(&ctx, &mut app, "the demo connects", |a| matches!(a.conns.get(&cid).map(|c| &c.status), Some(Status::Connected(_))) && a.requests.pending.is_empty());
    let ns = Namespace::new("demo-games", "providers");
    let index = Command::CreateIndex { ns: ns.clone(), keys: doc! { "code": 1 }, options: Some(doc! { "name": "code_1", "unique": true }) };
    // A tracked origin: `Origin::None` isn't kept in `pending`, so waiting
    // on it returned at once and the listing could run before the create.
    app.net().send(Origin::Notify { view: None, tree: None }, Some(cid), index);
    step_until(&ctx, &mut app, "the index is created", |a| a.requests.pending.is_empty());

    app.tree_action(TreeAction::OpenIndexes(cid, ns.clone()));
    let names = |app: &RongoApp| -> Vec<(String, bool)> {
        let Some(Tab { kind: TabKind::Indexes(it), .. }) = app.tabs.get(app.active_tab) else { panic!("index tab not active") };
        it.items_for_test().iter().map(|i| (i.spec.get_str("name").unwrap().to_string(), i.spec.get_bool("hidden").unwrap_or(false))).collect()
    };
    // Both indexes listed (the tab is active as soon as it is opened, its list arrives later).
    step_until(&ctx, &mut app, "the indexes are listed", |a| a.requests.pending.is_empty() && names(a).len() == 2);
    assert_eq!(names(&app), [("_id_".to_string(), false), ("code_1".to_string(), false)]);

    app.index_action(IndexAction::SetHidden(cid, ns.clone(), "code_1".into(), true));
    step_until(&ctx, &mut app, "the tab refreshes after hiding", |a| a.requests.pending.is_empty() && names(a).get(1).is_some_and(|i| i.1));
    assert_eq!(names(&app)[1], ("code_1".to_string(), true), "the tab refreshed after hiding");

    // Hover statistics land on the collection's tree node.
    if let Some(c) = app.conns.get_mut(&cid) {
        if c.dbs.is_none() {
            c.set_databases(vec![rongo_core::DbInfo { name: ns.db.clone(), size_on_disk: 0 }]);
        }
        if let Some(db) = c.db_mut(&ns.db) {
            db.colls = Some(vec![CollNode::new(rongo_core::CollInfo { name: ns.coll.clone(), kind: rongo_core::CollKind::Collection })]);
            db.loading = true;
        }
    }
    app.net().send(Origin::HoverStats(cid, ns.clone()), Some(cid), Command::CollStats { ns: ns.clone() });
    step_until(&ctx, &mut app, "the statistics arrive", |a| {
        a.conns.get(&cid).and_then(|c| c.dbs.as_ref()).and_then(|d| d.iter().find(|d| d.info.name == ns.db)).and_then(|d| d.colls.as_ref()).is_some_and(|cs| cs.iter().any(|c| c.info.name == ns.coll && c.stats.is_some()))
    });
    let stats = app.coll_node(cid, &ns).and_then(|n| n.stats.clone()).expect("stats arrived").expect("stats ok");
    assert_eq!((stats.0.count, stats.0.indexes), (8, 2));
    assert!(stats.0.total_index_size > 0);
}

/// A Duplicate that fails and can't drop its partial copy says so, and
/// the tree lists its collections again, so the copy shows without a
/// manual Refresh (as after one that finished).
#[test]
fn a_duplicate_that_leaves_its_partial_copy_reloads_the_tree() {
    let (_ctx, mut app, cid) = connected_mock_app();
    let listed = |name: &str| CollNode::new(rongo_core::CollInfo { name: name.into(), kind: CollKind::Collection });
    let c = app.conns.get_mut(&cid).unwrap();
    c.set_databases(vec![rongo_core::DbInfo { name: "demo-games".into(), size_on_disk: 0 }, rongo_core::DbInfo { name: "other".into(), size_on_disk: 0 }]);
    c.db_mut("demo-games").unwrap().colls = Some(vec![listed("users")]);
    c.db_mut("other").unwrap().colls = Some(vec![listed("kept")]);
    c.loading_dbs = false;
    // Sent as the app sends it, answered with the failure before the
    // core's own answer (which then finds nobody waiting).
    app.tree_action(TreeAction::Duplicate(cid, Namespace::new("demo-games", "users")));
    let mut ask = app.confirm.take().expect("asks for the name");
    ask.input = Some("users_dup".into());
    app.confirmed(ask);
    let id = *app.requests.pending.iter().find(|(_, s)| s.what.as_deref() == Some("Duplicating demo-games.users")).expect("sent").0;
    let why = "Duplicating stopped: read-only connection; the partial copy demo-games.users_dup remains, with 1,000 documents (dropping it failed: read-only connection)";
    app.handle(Response { id, conn: Some(cid), event: Event::Failed(CoreError::Other(why.into())) });
    assert!(toast_texts(&app).last().is_some_and(|t| t.contains("users_dup remains")), "{:?}", toast_texts(&app));
    let c = &app.conns[&cid];
    let dbs = c.dbs.as_ref().unwrap();
    assert!(dbs[0].colls.is_none(), "demo-games' collections are listed again");
    assert!(dbs[1].colls.is_some() && !c.loading_dbs, "the other database and the database list are left as they were");
}
