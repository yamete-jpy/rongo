//! The window: tabs, layout, shortcuts, and replies reaching their dialogs.

use super::*;

/// Work that takes more than a moment floats in the bottom-left corner
/// with its connection's name and how long it has run; ⊗ cancels it and
/// it goes at once. Quick requests, background work and a result view's
/// own requests (it shows those itself) never show there, and with
/// nothing running the corner is empty.
/// On the smallest window the app allows, with the sidebar dragged to
/// its widest and a long host name, every tab (shell, collection,
/// indexes) stays inside the window: nothing right-aligned is pushed
/// past its edge by a row that doesn't fit.
#[test]
fn narrow_tabs_keep_everything_on_screen() {
    use egui_kittest::Harness;
    use egui_kittest::kittest::{NodeT, Queryable};
    let w = 960.0;
    let mut h = harness(egui::vec2(w, 700.0), true);
    let cid = ConnectionProfile::mock().conn_id();
    let wait = |h: &mut Harness<RongoApp>, done: &dyn Fn(&Harness<RongoApp>) -> bool| {
        let end = Instant::now() + WAIT;
        while !done(h) && Instant::now() < end {
            h.step();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(done(h), "timed out waiting for the app");
        h.run_steps(4);
    };
    let has = |h: &Harness<RongoApp>, text: &str| h.query_by_label_contains(text).is_some();
    wait(&mut h, &|h| has(h, "Done in"));
    if let Some(Status::Connected(info)) = h.state_mut().conns.get_mut(&cid).map(|c| &mut c.status) {
        info.host = "cluster0-shard-00-01.abcde.mongodb.net:27017".into();
    }

    // Drag the sidebar's edge as far right as it goes.
    let edge = h.state().sidebar_rect.right() + 1.0;
    let press = |pos, pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
    let send = |h: &mut Harness<RongoApp>, e| {
        h.input_mut().events.push(e);
        h.step();
    };
    send(&mut h, egui::Event::PointerMoved(egui::pos2(edge, 300.0)));
    send(&mut h, press(egui::pos2(edge, 300.0), true));
    for k in 1..=10 {
        send(&mut h, egui::Event::PointerMoved(egui::pos2(edge + 40.0 * k as f32, 300.0)));
    }
    send(&mut h, press(egui::pos2(edge + 400.0, 300.0), false));
    h.run_steps(4);
    assert!(h.state().sidebar_rect.width() > 590.0, "the sidebar is at its widest: {:?}", h.state().sidebar_rect);

    let check = |h: &Harness<RongoApp>, tab: &str| {
        // Text cut by the window's own edge (not by a column or a
        // scroll area inside it).
        let cut: Vec<String> = h.output().shapes.iter().filter_map(|c| match &c.shape {
            egui::Shape::Text(t) if t.visual_bounding_rect().max.x > w + 0.5 && c.clip_rect.max.x >= w - 0.5 => Some(t.galley.text().to_string()),
            _ => None,
        }).collect();
        assert!(cut.is_empty(), "{tab}: cut by the window's edge: {cut:?}");
        let past: Vec<String> = h.root().children_recursive().filter(|n| n.accesskit_node().bounding_box().is_some_and(|b| b.x1 as f32 > w + 0.5)).filter_map(|n| n.accesskit_node().label()).filter(|l| !l.is_empty()).collect();
        assert!(past.is_empty(), "{tab}: widgets past the window's edge: {past:?}");
    };
    h.run_steps(2);
    check(&h, "shell");
    let tab_left = h.state().sidebar_rect.right();
    let db = h.query_all_by_label_contains("demo-games").map(|n| n.rect()).find(|r| r.left() > tab_left);
    assert!(db.is_some_and(|r| r.right() <= w), "the breadcrumb keeps the database in view: {db:?}");

    let ns = Namespace::new("demo-games", "providers");
    h.state_mut().tree_action(TreeAction::OpenCollectionTab(cid, ns.clone()));
    wait(&mut h, &|h| has(h, "ALPHA"));
    check(&h, "collection");
    // The status line's note gives way to its buttons instead of lying under them.
    let text_rect = |needle: &str| h.output().shapes.iter().find_map(|c| match &c.shape {
        egui::Shape::Text(t) if t.galley.text().contains(needle) && t.pos.x > h.state().sidebar_rect.right() => Some(t.visual_bounding_rect()),
        _ => None,
    });
    let (note, count) = (text_rect("documents selected"), text_rect("Count Documents"));
    assert!(note.zip(count).is_some_and(|(a, b)| a.right() <= b.left()), "the note ends before Count Documents: {note:?} {count:?}");
    h.state_mut().tree_action(TreeAction::OpenIndexes(cid, ns));
    wait(&mut h, &|h| has(h, "1 Index"));
    check(&h, "indexes");

    // The History tab, its list and preview, and an update's changes
    // side by side, with a value far wider than the tab.
    let ns = Namespace::new("demo-games", "narrow_history");
    let docs = vec![doc! { "_id": 1, "name": "short" }];
    h.state_mut().net().send(Origin::Notify { view: None, tree: None }, Some(cid), Command::InsertMany { ns: ns.clone(), docs });
    wait(&mut h, &|h| writes_done(h.state()));
    let update = doc! { "$set": { "name": "a long value ".repeat(40) } };
    h.state_mut().view_action(1, ViewAction::UpdateOne { conn: cid, ns: ns.clone(), id: rongo_core::bson::Bson::Int32(1), update });
    wait(&mut h, &|h| writes_done(h.state()));
    h.state_mut().tree_action(TreeAction::OpenHistory(cid, ns));
    wait(&mut h, &|h| has(h, "Changed 1 document"));
    assert!(has(&h, "Changed 1 document"), "the History tab shows the update");
    check(&h, "history");
    if let TabKind::History(t) = &mut h.state_mut().tabs.last_mut().unwrap().kind {
        t.show_changes = true;
    }
    let ready = |h: &Harness<RongoApp>| matches!(&h.state().tabs.last().unwrap().kind, TabKind::History(t) if t.changes_ready());
    wait(&mut h, &ready);
    assert!(ready(&h), "the changes are shown");
    check(&h, "history changes");
}

/// A failed connect's toast wraps to a few lines in the corner instead
/// of one line wider than the window that loses its start.
#[test]
fn long_toast_stays_in_the_corner() {
    let (ctx, mut app) = test_app(false);
    app.toasts.clear();
    app.toast(format!("prod: {}", crate::test_support::long_driver_error()), true);
    let size = egui::vec2(1200.0, 800.0);
    let mut found = None;
    for _ in 0..3 {
        let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)), ..Default::default() };
        let mut out = ctx.run_ui(input, |ui| app.toasts(ui.ctx()));
        out.textures_delta.clear();
        found = out.shapes.iter().find_map(|s| match &s.shape {
            egui::Shape::Text(t) if t.galley.text().starts_with("prod: ") => Some((t.galley.clone(), t.galley.rect.translate(t.pos.to_vec2()))),
            _ => None,
        });
    }
    let (galley, rect) = found.expect("the toast is drawn");
    assert!(rect.left() >= 0.0 && rect.right() <= size.x && rect.bottom() <= size.y, "on screen: {rect:?}");
    assert!(rect.width() <= 400.0, "a narrow box: {rect:?}");
    assert!(galley.rows.len() <= 4 && galley.elided, "{} rows", galley.rows.len());
}

/// The toolbar's Import lock reads the connection Export/Import act on
/// without copying the tab's query (`target_conn`), and `view_mut`
/// finds a view in its tab without listing every tab's views.
#[test]
fn target_conn_and_view_lookup() {
    let (_ctx, mut app) = test_app(false);
    let (a, b) = (ConnId(uuid::Uuid::from_u128(1)), ConnId(uuid::Uuid::from_u128(2)));
    let same = |app: &RongoApp| assert_eq!(app.target_conn(), app.target_collection().map(|t| t.0));
    same(&app);
    app.sidebar.selection = crate::sidebar::Selection { conn: Some(a), db: None, coll: None };
    same(&app);
    app.sidebar.selection.db = Some("d".into());
    same(&app);
    assert_eq!(app.target_conn(), Some(a));
    app.open_collection(b, Namespace::new("d", "c"));
    same(&app);
    assert_eq!(app.target_conn(), Some(b), "the collection tab on screen");

    let coll_view = match &app.tabs[app.active_tab].kind {
        TabKind::Collection(c) => c.view.id,
        _ => unreachable!(),
    };
    app.open_info(a, "Statistics".into(), doc! { "ok": 1 });
    let info_view = match &app.tabs[app.active_tab].kind {
        TabKind::Info { view, .. } => view.id,
        _ => unreachable!(),
    };
    assert_eq!(app.view_mut(coll_view).map(|v| v.id), Some(coll_view));
    assert_eq!(app.view_mut(info_view).map(|v| v.id), Some(info_view));
    assert!(app.view_mut(info_view + 100).is_none());
}

/// With more tabs than fit, the newly opened tab is scrolled into view
/// and ▾ lists every tab.
#[test]
fn tab_bar_shows_the_new_tab_when_crowded() {
    use egui_kittest::kittest::Queryable;
    let mut h = harness(egui::vec2(760.0, 600.0), false);
    let cid = ConnectionProfile::mock().conn_id();
    for i in 0..8 {
        let id = h.state_mut().next_tab();
        let tab = Tab { id, kind: TabKind::Collection(Box::new(CollectionTab::new(cid, Namespace::new("db", format!("collection_number_{i}")), 1000 + i))) };
        h.state_mut().tabs.push(tab);
    }
    let last = h.state().tabs.len() - 1;
    h.state_mut().active_tab = last;
    h.run_steps(4);
    let title = h.state().tabs[last].title(|_| "?".into());
    let shown = h.query_by_label_contains(&title).map(|n| n.rect());
    let r = shown.expect("the new tab is drawn");
    assert!(r.right() <= 760.0 && r.left() >= 0.0, "the new tab is on screen: {r:?}");
    assert!(h.query_by_label_contains(&h.state().tabs[0].title(|_| "?".into())).is_none_or(|n| n.rect().right() < 340.0), "earlier tabs scrolled away");
    let tab_row = r.center().y;
    assert!(h.query_all_by_label(icon::CARET_DOWN).any(|n| (n.rect().center().y - tab_row).abs() < 12.0 && n.rect().left() > 700.0), "▾ at the end of the tab bar lists every tab");
}

/// Screenshots of every screen on the demo server, for reviewing the
/// UI by eye. Writes PNGs to $RONGO_TOUR_DIR. Run with
/// `RONGO_TOUR_DIR=/tmp/tour cargo test -p rongo ui_tour -- --ignored`.
#[test]
#[ignore = "writes screenshots; run by hand"]
fn ui_tour() {
    use egui_kittest::Harness;
    let dir = std::path::PathBuf::from(std::env::var("RONGO_TOUR_DIR").expect("RONGO_TOUR_DIR"));
    std::fs::create_dir_all(&dir).unwrap();
    let sizes: Vec<(&str, egui::Vec2)> = match std::env::var("RONGO_TOUR_SIZE").as_deref() {
        Ok("narrow") => vec![("narrow", egui::vec2(760.0, 720.0))],
        _ => vec![("wide", egui::vec2(1400.0, 860.0))],
    };
    for (tag, size) in sizes {
        let mut h = Harness::builder().with_size(size).wgpu().build_eframe(|cc| RongoApp::new(cc, runtime(true), true));
        let settle = |h: &mut Harness<RongoApp>, ms: u64| {
            let end = Instant::now() + Duration::from_millis(ms);
            while Instant::now() < end {
                h.step();
                std::thread::sleep(Duration::from_millis(16));
            }
        };
        let shot = |h: &mut Harness<RongoApp>, name: &str| {
            h.step();
            let img = h.render().expect("render");
            img.save(dir.join(format!("{tag}-{name}.png"))).unwrap();
        };
        settle(&mut h, 2500);
        shot(&mut h, "01-shell");

        let cid = ConnectionProfile::mock().conn_id();
        let ns = Namespace::new("demo-games", "providers");
        h.state_mut().tree_action(TreeAction::OpenCollectionTab(cid, ns.clone()));
        settle(&mut h, 1200);
        shot(&mut h, "02-collection-table");
        let set_mode = |h: &mut Harness<RongoApp>, m: Mode| {
            let app = h.state_mut();
            let i = app.active_tab;
            if let TabKind::Collection(c) = &mut app.tabs[i].kind {
                c.view.mode = m;
            }
        };
        set_mode(&mut h, Mode::Tree);
        settle(&mut h, 400);
        shot(&mut h, "03-collection-tree");
        set_mode(&mut h, Mode::Json);
        settle(&mut h, 600);
        shot(&mut h, "04-collection-json");
        set_mode(&mut h, Mode::Table);
        settle(&mut h, 200);

        h.state_mut().tree_action(TreeAction::OpenIndexes(cid, ns.clone()));
        settle(&mut h, 800);
        shot(&mut h, "05-index-manager");

        // Dialogs, one at a time.
        let dialog = |h: &mut Harness<RongoApp>, name: &str, open: &dyn Fn(&mut RongoApp)| {
            open(h.state_mut());
            settle(h, 700);
            shot(h, name);
            let app = h.state_mut();
            app.connect_dialog = None;
            app.doc_dialog = None;
            app.tool = None;
            app.settings_dialog = None;
            app.view_doc = None;
            app.confirm = None;
            settle(h, 300);
        };
        dialog(&mut h, "06-connection-manager", &|app| {
            if let (Some(store), Some(vault)) = (app.store.as_mut(), app.vault.as_deref()) {
                for (n, (name, color)) in [("alpha-sit", Some("green")), ("alpha-prod", Some("red")), ("bravo-dev", Some("purple")), ("local", None)].into_iter().enumerate() {
                    let p = ConnectionProfile { id: uuid::Uuid::from_u128(100 + n as u128), color: color.map(str::to_string), ..ConnectionProfile::new(name) };
                    let _ = store.upsert(p, None, None, vault);
                }
            }
            app.connect_dialog = Some(ConnectDialog::new());
        });
        dialog(&mut h, "07-new-connection", &|app| app.connect_dialog = Some(ConnectDialog::new_connection()));
        let sample = doc! { "_id": 1, "code": "ALPHA", "name": "Alpha Gaming", "enabled": true, "config": { "currency": "THB", "timeoutMs": 5000 } };
        dialog(&mut h, "08-edit-document", &|app| app.doc_dialog = Some(DocDialog::edit(cid, ns.clone(), &sample, None)));
        dialog(&mut h, "09-update-dialog", &|app| app.tool = Some(ToolDialog::bulk(cid, ns.clone(), None, false, Vec::new(), doc! { "enabled": true }, Some(("name".into(), rongo_core::bson::Bson::String("Alpha".into()))))));
        dialog(&mut h, "10-delete-dialog", &|app| app.tool = Some(ToolDialog::bulk(cid, ns.clone(), None, true, Vec::new(), doc! {}, None)));
        dialog(&mut h, "11-add-index", &|app| app.tool = Some(ToolDialog::index(cid, ns.clone(), vec!["code".into(), "name".into(), "config.currency".into()])));
        dialog(&mut h, "12-export", &|app| app.tool = Some(ToolDialog::export(cid, FindSpec::all(ns.clone(), 50), true)));
        dialog(&mut h, "13-settings", &|app| app.open_settings());
        dialog(&mut h, "14-drop-database", &|app| app.tree_action(TreeAction::DropDatabase(cid, "demo-games".into())));

        // The update banner, for a stub release newer than this one.
        let running = crate::update::Version::running().expect("three numbers");
        h.state_mut().update.found = Some(crate::update::Release::remembered(crate::update::Version { patch: running.patch + 1, ..running }));
        settle(&mut h, 300);
        shot(&mut h, "15-update-banner");
        h.state_mut().update.found = None;
    }
}

/// Run the whole app (demo server, theme, sidebar, shell results, a
/// collection tab) and check that, with no input, nothing on screen moves
/// from one frame to the next. Two splitters used to creep a pixel or so
/// per frame because their size came from their own content.
#[test]
fn nothing_moves_on_its_own() {
    let (ctx, mut app) = test_app(true);
    let mut frame = eframe::Frame::_new_kittest();
    let sidebar_width = |ctx: &egui::Context| egui::containers::panel::PanelState::load(ctx, egui::Id::new("sidebar")).map(|s| s.size().x);
    let mut widths = Vec::new();
    let mut step = |app: &mut RongoApp| {
        let mut out = ctx.run_ui(frame_input(), |ui| eframe::App::ui(app, ui, &mut frame));
        out.textures_delta.clear();
        widths.extend(sidebar_width(&ctx));
        format!("{:?}", out.shapes)
    };
    let cid = ConnectionProfile::mock().conn_id();
    // Draws until `done` holds, by the clock (a loaded machine draws slowly).
    let mut settle = |app: &mut RongoApp, what: &str, done: &dyn Fn(&RongoApp) -> bool| {
        let end = Instant::now() + WAIT;
        while Instant::now() < end {
            step(app);
            if done(app) {
                return;
            }
            std::thread::sleep(Duration::from_millis(16));
        }
        panic!("timed out waiting for: {what}");
    };
    // Let the demo connect, the tree load and the sample script run.
    settle(&mut app, "the demo connects and the sample script ends", &|a| {
        matches!(a.conns.get(&cid).map(|c| &c.status), Some(Status::Connected(_)))
            && a.requests.pending.is_empty()
            && a.tabs.iter().all(|t| !matches!(&t.kind, TabKind::Shell(s) if s.run_when_ready || s.is_running()))
    });
    app.tree_action(TreeAction::OpenCollectionTab(cid, Namespace::new("demo-games", "providers")));
    settle(&mut app, "the collection's first page", &|a| a.requests.pending.is_empty());
    for tab in [app.tabs.len() - 1, 1] {
        app.active_tab = tab;
        for _ in 0..10 {
            step(&mut app);
        }
        let first = step(&mut app);
        for i in 0..20 {
            assert!(step(&mut app) == first, "tab {tab}: the screen changed {i} frames later without input");
        }
    }
    // The search box sits under the toolbar and the demo connection's tree
    // starts right below it, on screen.
    let search = ctx.read_response(egui::Id::new("tree-search")).expect("search box").rect;
    let tree_top = ctx.read_response(egui::Id::new("tree-first-row")).expect("tree").rect;
    assert!(search.top() < 120.0, "search box at {search:?}");
    assert!(tree_top.top() < search.bottom() + 40.0, "tree starts at {tree_top:?}");
    // Nobody dragged the sidebar, so it must have kept its width the
    // whole time (a creeping panel stops moving once it hits its maximum).
    assert!(widths.len() > 60);
    assert!(widths.windows(2).all(|w| (w[0] - w[1]).abs() < 0.5), "sidebar width moved: first {:?}, last {:?}", widths.first(), widths.last());
}

/// A script that finishes while its tab is in the background is seen
/// as finished, so its tab can be closed.
#[test]
fn background_scripts_finish() {
    let (ctx, mut app) = test_app(true);
    let cid = ConnectionProfile::mock().conn_id();
    step_until(&ctx, &mut app, "the demo connects", |a| matches!(a.conns.get(&cid).map(|c| &c.status), Some(Status::Connected(_))) && a.requests.pending.is_empty());
    // Long enough to still be running when the tab is switched away from
    // (checked after every frame), short enough not to slow the suite.
    app.open_shell(cid, Some("demo-games".into()), Some("sleep(1000); print('done')".into()));
    let shell_tab = app.tabs.len() - 1;
    if let TabKind::Shell(s) = &mut app.tabs[shell_tab].kind {
        s.run_when_ready = true;
    }
    let running = |app: &RongoApp| matches!(&app.tabs[shell_tab].kind, TabKind::Shell(s) if s.is_running());
    step_until(&ctx, &mut app, "the script started", running);
    app.active_tab = 0;
    step_until(&ctx, &mut app, "it finishes in the background", |a| !running(a));
}

/// Closing the tabs a rule matches tries each of them once: a shell tab
/// running a script stays (its close is refused, with a toast), and so
/// does the Welcome tab that closing the last tab opens. The loop used to
/// meet a refused tab again and again, hanging the window.
#[test]
fn closing_tabs_by_a_rule_always_ends() {
    let (ctx, mut app, cid) = connected_mock_app();
    step_until(&ctx, &mut app, "the demo script finishes", |a| !a.tabs.iter().any(|t| matches!(&t.kind, TabKind::Shell(s) if s.is_running() || s.run_when_ready)));
    app.push_tab(|_| TabKind::Welcome);
    app.open_shell(cid, Some("demo-games".into()), Some("sleep(60000)".into()));
    let shell = app.tabs.last().unwrap().id;
    if let TabKind::Shell(s) = &mut app.tabs.last_mut().unwrap().kind {
        s.run_when_ready = true;
    }
    let running = |a: &RongoApp| a.tabs.iter().any(|t| t.id == shell && matches!(&t.kind, TabKind::Shell(s) if s.is_running()));
    step_until(&ctx, &mut app, "the script runs", running);
    assert!(app.tabs.len() > 2, "{} tabs", app.tabs.len());
    app.close_tabs_where(|_| true);
    assert_eq!(app.tabs.iter().map(|t| t.id).collect::<Vec<_>>(), [shell], "only the running script's tab stays");
    assert!(toast_texts(&app).iter().any(|t| t == "Stop the running script before closing its tab"), "{:?}", toast_texts(&app));
    if let TabKind::Shell(s) = &app.tabs[0].kind {
        s.stop();
    }
    step_until(&ctx, &mut app, "the script stops", |a| !running(a));
    app.close_tabs_where(|_| true);
    assert!(matches!(&app.tabs[..], [Tab { kind: TabKind::Welcome, .. }]), "a Welcome tab opens in its place");
}

/// Closing a tab cancels what it was waiting for: its views' queries,
/// an Index Manager's details, a History tab's comparison. Nothing
/// showed them or could stop them once the tab was gone. A restore goes
/// on (it writes, and its toast names the collection).
#[test]
fn closing_a_tab_cancels_its_requests() {
    let (_ctx, mut app, cid) = connected_mock_app();
    let ns = Namespace::new("demo-games", "advertisers");
    let view_of = |t: &Tab| match &t.kind {
        TabKind::Collection(c) => c.view.id,
        _ => panic!("not a collection tab"),
    };
    app.open_collection(cid, ns.clone());
    let vid = view_of(app.tabs.last().unwrap());
    // On its way: the demo server answers after 20 ms or more.
    let find = app.net().send(Origin::View(vid, ViewReq::Page), Some(cid), Command::Find(FindSpec::all(ns.clone(), 50)));
    app.close_tab(app.tabs.len() - 1);
    assert!(!app.requests.pending.contains_key(&find), "forgotten");
    let end = Instant::now() + WAIT;
    loop {
        match app.requests.core.try_recv() {
            Some(r) if r.id == find => {
                assert!(matches!(r.event, Event::Failed(CoreError::Cancelled)), "cancelled, not answered: {:?}", r.event);
                break;
            }
            Some(_) => {}
            None => {
                assert!(Instant::now() < end, "the find never answered");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }

    app.open_indexes(cid, ns.clone());
    let it = app.tabs.last().unwrap().id;
    app.requests.pending.insert(901, Sent::new(Origin::IndexTab(it)));
    app.close_tab(app.tabs.len() - 1);
    assert!(!app.requests.pending.contains_key(&901), "the Index Manager's request");

    app.open_history(cid, ns.clone());
    let h = app.tabs.last().unwrap().id;
    app.requests.pending.insert(902, Sent::new(Origin::History { tab: h, ns: ns.clone(), req: HistoryReq::Compare }));
    app.requests.pending.insert(903, Sent::new(Origin::History { tab: h, ns: ns.clone(), req: HistoryReq::Restore }));
    app.close_tab(app.tabs.len() - 1);
    assert!(!app.requests.pending.contains_key(&902), "the History tab's comparison");
    assert!(app.requests.pending.contains_key(&903), "a restore goes on");

    // Disconnect closes its tabs the same way.
    app.open_collection(cid, ns.clone());
    let vid = view_of(app.tabs.last().unwrap());
    app.requests.pending.insert(904, Sent::new(Origin::View(vid, ViewReq::Count)));
    app.tree_action(TreeAction::Disconnect(cid));
    assert!(!app.requests.pending.contains_key(&904), "a view's count, on Disconnect");
}

/// A reply from a dialog that was cancelled while busy doesn't close or
/// take over the dialog opened after it.
#[test]
fn late_replies_leave_the_next_dialog_alone() {
    let (_ctx, mut app) = test_app(true);
    let cid = ConnectionProfile::mock().conn_id();
    let ns = Namespace::new("d", "c");
    // An export was started (request 900) and its dialog closed; now an
    // Update dialog is open and busy with request 901.
    let mut update = ToolDialog::bulk(cid, ns.clone(), None, false, Vec::new(), doc! {}, None);
    update.running = Some(901);
    app.tool = Some(update);
    app.requests.pending.insert(900, crate::net::Sent::new(Origin::Tool { view: None, tree: None }));
    app.handle(Response { id: 900, conn: Some(cid), event: Event::Exported { count: 3, path: "/tmp/x.json".into() } });
    assert!(app.tool.as_ref().is_some_and(|t| t.running == Some(901)), "the Update dialog stayed open and busy");
    app.requests.pending.insert(902, crate::net::Sent::new(Origin::Tool { view: None, tree: None }));
    app.handle(Response { id: 902, conn: Some(cid), event: Event::Failed(CoreError::Other("old".into())) });
    assert!(app.tool.as_ref().is_some_and(|t| t.error.is_none()), "an old failure isn't shown in it");

    // Same for the document editor.
    let mut d = DocDialog::edit(cid, ns.clone(), &doc! { "_id": 1 }, None);
    d.saving = Some(911);
    app.doc_dialog = Some(d);
    app.requests.pending.insert(910, crate::net::Sent::new(Origin::Save { view: None }));
    app.handle(Response { id: 910, conn: Some(cid), event: Event::Written(Default::default()) });
    assert!(app.doc_dialog.is_some(), "another document's save didn't close this editor");
    app.requests.pending.insert(911, crate::net::Sent::new(Origin::Save { view: None }));
    app.handle(Response { id: 911, conn: Some(cid), event: Event::Written(Default::default()) });
    assert!(app.doc_dialog.is_none(), "its own save did");

    // A save the server did not confirm keeps the editor open, with the
    // edits and why: it may or may not have been applied.
    let mut d = DocDialog::edit(cid, ns.clone(), &doc! { "_id": 1 }, None);
    d.saving = Some(912);
    app.doc_dialog = Some(d);
    app.requests.pending.insert(912, crate::net::Sent::new(Origin::Save { view: None }));
    let warning = Some("network error: reset; the documents as they were are kept in Collection History".to_string());
    let toasts = app.toasts.len();
    app.handle(Response { id: 912, conn: Some(cid), event: Event::Written(rongo_core::WriteOutcome { unconfirmed: true, warning, ..Default::default() }) });
    let d = app.doc_dialog.as_ref().expect("the editor stays open");
    assert_eq!(d.saving, None, "ready again");
    assert_eq!(d.error.as_deref(), Some("The server did not confirm this write: it may have been applied. Check before retrying. (network error: reset; the documents as they were are kept in Collection History)"));
    assert_eq!(app.toasts.len(), toasts, "said in the editor, not in a toast");
}

/// The validator dialog takes only its own reply. A dialog cancelled
/// before it loaded used to fill in the next one (another collection's
/// validator, even after that one's own had arrived), and Save then
/// wrote it onto the wrong collection. A failure shows only in its own
/// dialog.
#[test]
fn validator_replies_go_to_their_own_dialog() {
    let (_ctx, mut app) = test_app(false);
    let cid = ConnectionProfile::mock().conn_id();
    let ns = |c: &str| Namespace::new("d", c);
    let asked = |app: &RongoApp| app.requests.pending.iter().filter(|(_, s)| matches!(s.origin, Origin::Validator)).map(|(id, _)| *id).max().expect("asked");
    let reply = |level: &str| Event::Document(doc! { "validator": { "x": { "$exists": true } }, "validationLevel": level, "validationAction": "warn" });
    let shown = |app: &RongoApp| match app.tool.as_ref().map(|t| &t.kind) {
        Some(ToolKind::Validator { ns, text, level, .. }) => (ns.coll.clone(), text.clone(), level.clone()),
        _ => panic!("no validator dialog"),
    };
    // A and C are opened and cancelled before they load; B stays.
    app.tree_action(TreeAction::Validator(cid, ns("a")));
    let a = asked(&app);
    app.tool = None;
    app.tree_action(TreeAction::Validator(cid, ns("c")));
    let c = asked(&app);
    app.tool = None;
    app.tree_action(TreeAction::Validator(cid, ns("b")));
    let b = asked(&app);
    app.handle(Response { id: a, conn: Some(cid), event: reply("moderate") });
    assert_eq!(shown(&app), ("b".into(), "{ }".into(), "strict".into()), "A's validator stays out of B's dialog");
    app.handle(Response { id: b, conn: Some(cid), event: Event::Document(doc! { "validator": {}, "validationLevel": "off" }) });
    assert_eq!(shown(&app).2, "off", "its own reply fills it in");
    app.handle(Response { id: c, conn: Some(cid), event: reply("moderate") });
    assert_eq!(shown(&app).2, "off", "a late reply after its own changes nothing");

    // A failure for a dialog that is gone isn't shown in the next one.
    app.tool = None;
    app.tree_action(TreeAction::Validator(cid, ns("d")));
    let d = asked(&app);
    app.tool = Some(ToolDialog::export(cid, FindSpec::all(ns("e"), 50), false));
    app.handle(Response { id: d, conn: Some(cid), event: Event::Failed(CoreError::Other("no".into())) });
    assert!(app.tool.as_ref().is_some_and(|t| t.error.is_none()), "the Export dialog says nothing of it");
}

/// An Edit/Clone reply opens the editor only when no dialog is open: a
/// second double-click's reply used to replace the editor open on the
/// first, and the edits typed there were lost.
#[test]
fn a_document_reply_leaves_open_dialogs_alone() {
    let (_ctx, mut app) = test_app(false);
    let cid = ConnectionProfile::mock().conn_id();
    let ns = Namespace::new("d", "c");
    let reply = |app: &mut RongoApp, id: RequestId, n: i32| {
        app.requests.pending.insert(id, Sent::new(Origin::OpenDoc { conn: cid, ns: ns.clone(), view: 1, clone: false }));
        let docs = vec![rongo_core::bson::RawDocumentBuf::try_from(&doc! { "_id": n, "from": "the server" }).unwrap()];
        app.handle(Response { id, conn: Some(cid), event: Event::Page { docs, has_more: false, elapsed_ms: 1 } });
    };
    app.doc_dialog = Some(DocDialog::edit(cid, Namespace::new("d", "c"), &doc! { "_id": 1, "typed": "edits" }, None));
    reply(&mut app, 920, 2);
    assert!(app.doc_dialog.as_ref().is_some_and(|d| d.text().contains("typed")), "the open editor stays");

    app.doc_dialog = None;
    app.view_doc = Some(ViewDocDialog::new(&doc! { "_id": 9 }));
    reply(&mut app, 921, 3);
    assert!(app.doc_dialog.is_none(), "not over another dialog");

    app.view_doc = None;
    reply(&mut app, 922, 4);
    assert!(app.doc_dialog.as_ref().is_some_and(|d| d.text().contains("the server")), "with nothing open, it opens");
}

/// Editing or cloning a row from a projected result edits the whole
/// document: saving the projected row would drop the other fields.
#[test]
fn edit_and_clone_use_the_whole_document() {
    let (ctx, mut app) = test_app(true);
    let cid = ConnectionProfile::mock().conn_id();
    // Waits for each answer rather than a number of frames: under the
    // load of the whole suite the demo server can take longer.
    step_until(&ctx, &mut app, "the demo connects", |a| matches!(a.conns[&cid].status, Status::Connected(_)));
    let ns = Namespace::new("demo-games", "edit_me");
    let full = doc! { "_id": "e1", "name": "shown", "secret": "not in the projection", "n": 5_i64 };
    app.net().send(Origin::Notify { view: None, tree: None }, Some(cid), Command::Insert { ns: ns.clone(), doc: full.clone() });
    step_until(&ctx, &mut app, "the document is inserted", writes_done);

    let projected = doc! { "_id": "e1", "name": "shown" };
    app.view_action(1, ViewAction::Edit { conn: cid, ns: ns.clone(), doc: projected.clone() });
    step_until(&ctx, &mut app, "the editor opens", |a| a.doc_dialog.is_some());
    let text = app.doc_dialog.as_ref().expect("editor opened").text().to_string();
    assert!(text.contains("not in the projection") && text.contains("NumberLong"), "{text}");

    app.doc_dialog = None;
    app.view_action(1, ViewAction::Clone { conn: cid, ns: ns.clone(), doc: projected });
    step_until(&ctx, &mut app, "the clone opens", |a| a.doc_dialog.is_some());
    let d = app.doc_dialog.as_ref().expect("clone opened");
    assert!(matches!(d.mode, crate::dialogs::DocMode::Insert));
    assert!(d.text().contains("not in the projection") && !d.text().contains("\"e1\""), "{}", d.text());
}

/// ⌘F goes to the result view under the pointer (its find bar), and to
/// the tree's search box when the pointer is over the sidebar.
#[test]
fn command_f_finds_where_the_pointer_is() {
    let (ctx, mut app) = test_app(true);
    let mut frame = eframe::Frame::_new_kittest();
    let mut run = |app: &mut RongoApp, events: Vec<egui::Event>| {
        let input = egui::RawInput { events, ..frame_input() };
        let mut out = ctx.run_ui(input, |ui| eframe::App::ui(app, ui, &mut frame));
        out.textures_delta.clear();
        std::thread::sleep(Duration::from_millis(10));
    };
    let cid = ConnectionProfile::mock().conn_id();
    step_until(&ctx, &mut app, "the demo connects", |a| matches!(a.conns.get(&cid).map(|c| &c.status), Some(Status::Connected(_))) && a.requests.pending.is_empty());
    app.tree_action(TreeAction::OpenCollectionTab(cid, Namespace::new("demo-games", "providers")));
    step_until(&ctx, &mut app, "the collection's first page", |a| a.requests.pending.is_empty());
    run(&mut app, vec![]);
    let find_open = |app: &RongoApp| match app.tabs.get(app.active_tab).map(|t| &t.kind) {
        Some(TabKind::Collection(c)) => c.view.find_open(),
        _ => panic!("collection tab not active"),
    };
    let cmd_f = egui::Event::Key { key: Key::F, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::COMMAND };
    let tree_search_focused = |ctx: &egui::Context| ctx.memory(|m| m.has_focus(egui::Id::new("tree-search")));

    run(&mut app, vec![egui::Event::PointerMoved(egui::pos2(900.0, 500.0))]);
    run(&mut app, vec![cmd_f.clone()]);
    run(&mut app, vec![]);
    assert!(find_open(&app), "⌘F over the results opened their find bar");
    assert!(!tree_search_focused(&ctx));

    run(&mut app, vec![egui::Event::PointerMoved(egui::pos2(150.0, 400.0))]);
    run(&mut app, vec![]);
    run(&mut app, vec![cmd_f]);
    run(&mut app, vec![]);
    assert!(tree_search_focused(&ctx), "⌘F over the sidebar went to the tree search");
}

/// ⇧⌘[ and ⇧⌘] (previous and next tab) go by the keys' place. With ⇧
/// held, Windows names a key by its shifted character: `{` and `}` on a US
/// layout, `ฐ` (no egui key: the place is sent) and `,` on the Thai one,
/// where that `,` must not open Settings (⌘,).
#[test]
fn previous_and_next_tab_go_by_the_bracket_keys() {
    let (ctx, mut app) = test_app(false);
    app.tabs.extend([2, 3].map(|id| Tab { id, kind: TabKind::Welcome }));
    let mut frame = eframe::Frame::_new_kittest();
    let mut press = |app: &mut RongoApp, key, place| {
        let event = egui::Event::Key { key, physical_key: Some(place), pressed: true, repeat: false, modifiers: Modifiers::COMMAND | Modifiers::SHIFT };
        let input = egui::RawInput { events: vec![event], ..frame_input() };
        ctx.run_ui(input, |ui| eframe::App::ui(app, ui, &mut frame)).textures_delta.clear();
        app.active_tab
    };
    assert_eq!(press(&mut app, Key::CloseBracket, Key::CloseBracket), 1, "macOS ⇧⌘]");
    assert_eq!(press(&mut app, Key::OpenBracket, Key::OpenBracket), 0, "macOS ⇧⌘[");
    assert_eq!(press(&mut app, Key::OpenCurlyBracket, Key::OpenBracket), 2, "Windows, US layout: Ctrl+Shift+[");
    assert_eq!(press(&mut app, Key::CloseCurlyBracket, Key::CloseBracket), 0, "Windows, US layout: Ctrl+Shift+]");
    assert_eq!(press(&mut app, Key::Comma, Key::CloseBracket), 1, "Thai layout: Ctrl+Shift+]");
    assert!(app.settings_dialog.is_none(), "the Thai Ctrl+Shift+] opened Settings");
}
