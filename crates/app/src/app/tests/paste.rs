//! Copying and pasting collections.

use super::*;

/// Copy a collection on one connection, paste it as a new database on
/// another: the documents arrive, the toast says where, and the target's
/// tree learns about the new database.
#[test]
fn copy_and_paste_between_connections() {
    let (ctx, mut app, a) = connected_mock_app();
    let b = connect_mock_b(&ctx, &mut app);
    let providers = Namespace::new("demo-games", "providers");
    assert_eq!(count_docs(&app, a, &providers), 8);

    app.tree_action(TreeAction::CopyCollections { conn: a, db: "demo-games".into(), colls: vec!["providers".into()] });
    let clip = app.coll_clip.as_ref().expect("copied");
    assert_eq!((clip.conn, clip.db.as_str(), clip.colls.as_slice()), (a, "demo-games", ["providers".to_string()].as_slice()));

    app.tree_action(TreeAction::PasteCollections { conn: b, db: "copy".into() });
    assert!(app.paste_dialog.is_some(), "the Paste dialog opens");
    step_until(&ctx, &mut app, "the target's collections are listed", |a| !a.requests.pending.values().any(|s| matches!(s.origin, Origin::Collections(..))));
    let cmd = app.paste_dialog.as_ref().unwrap().build();
    app.paste_action(PasteAction::Send(cmd));
    assert!(app.paste_dialog.is_none(), "the dialog closes once sent");

    step_until(&ctx, &mut app, "the copy finishes", |a| a.toasts.iter().any(|t| t.text.starts_with("Pasted")));
    assert!(toast_texts(&app).contains(&"Pasted 1 collection into Mock B · copy: 8 documents".to_string()), "{:?}", toast_texts(&app));
    // Before counting, which reads the core's replies itself.
    step_until(&ctx, &mut app, "the target lists the new database", |a| a.conns[&b].dbs.as_ref().is_some_and(|d| d.iter().any(|d| d.info.name == "copy")));
    assert_eq!(count_docs(&app, b, &Namespace::new("copy", "providers")), 8);
    assert_eq!(count_docs(&app, a, &providers), 8, "the source is untouched");
}

/// The Paste dialog asks what the target database holds and waits for
/// the answer: before it, Paste is off ("Checking…"); once it arrives, a
/// row that lands on an existing collection offers Append and Replace. A
/// listing that fails keeps Paste off and says why.
#[test]
fn paste_waits_for_the_targets_collections() {
    let (ctx, mut app, a) = connected_mock_app();
    let b = connect_mock_b(&ctx, &mut app);
    app.tree_action(TreeAction::CopyCollections { conn: a, db: "demo-games".into(), colls: vec!["providers".into()] });
    let target = TargetInfo { name: "Mock B", color: None, read_only: false };

    app.tree_action(TreeAction::PasteCollections { conn: b, db: "demo-games".into() });
    let ready = |app: &RongoApp| app.paste_dialog.as_ref().expect("the dialog is open").ready(&target);
    assert_eq!(ready(&app), Err("Checking what demo-games holds…".to_string()), "nothing known yet");
    step_until(&ctx, &mut app, "the target's collections are listed", |a| !a.requests.pending.values().any(|s| matches!(s.origin, Origin::Collections(..))));
    assert_eq!(ready(&app), Ok(()), "known: providers exists there, appended to by default");
    let shown = drawn_texts(&ctx, &mut app);
    assert!(shown.iter().any(|t| t == "Append") && shown.iter().any(|t| t == "Replace"), "the existing target offers both: {shown:?}");

    // A failed listing reaches the dialog too.
    app.net().send(Origin::Collections(b, "demo-games".into()), Some(b), Command::ListCollections { db: "demo-games".into() });
    let id = *app.requests.pending.iter().find(|(_, s)| matches!(s.origin, Origin::Collections(..))).expect("asked").0;
    app.handle(Response { id, conn: Some(b), event: Event::Failed(CoreError::Server { code: 13, msg: "not authorized on demo-games".into() }) });
    let why = ready(&app).unwrap_err();
    assert!(why.starts_with("Couldn't see what demo-games holds") && why.contains("not authorized"), "{why}");
    assert!(drawn_texts(&ctx, &mut app).iter().any(|t| t.contains("not authorized")), "the dialog says why");
}

#[test]
fn copy_summary_says_what_happened() {
    let item = |coll: &str, inserted, overwritten, skipped, error: Option<&str>| CopiedItem {
        src: Namespace::new("demo-games", coll),
        dst: Namespace::new("copy", coll),
        inserted,
        overwritten,
        skipped,
        recorded: None,
        warning: None,
        error: error.map(str::to_string),
        unconfirmed: false,
    };
    let report = |items, cancelled| CopyReport { items, cancelled };
    // An item stopped on a write the server did not confirm.
    let mut lost = item("bets", 960, 7, 0, Some("network error: reset"));
    lost.unconfirmed = true;
    assert_eq!(
        copy_summary(&report(vec![item("providers", 8, 0, 0, None), lost], false)),
        "Pasted 1 collection into copy: 968 documents, 7 overwritten; bets: the server did not confirm a write: it may have been applied. Check before retrying. (network error: reset)"
    );
    assert_eq!(copy_summary(&report(vec![item("providers", 8, 0, 0, None)], false)), "Pasted 1 collection into copy: 8 documents");
    assert_eq!(
        copy_summary(&report(vec![item("providers", 1, 0, 0, None), item("games", 1_200, 0, 0, None)], false)),
        "Pasted 2 collections into copy: 1,201 documents"
    );
    assert_eq!(
        copy_summary(&report(vec![item("providers", 8, 0, 10, None)], false)),
        "Pasted 1 collection into copy: 8 documents, 10 skipped"
    );
    assert_eq!(
        copy_summary(&report(vec![item("providers", 8, 10, 0, None)], false)),
        "Pasted 1 collection into copy: 8 documents, 10 overwritten"
    );
    assert_eq!(
        copy_summary(&report(vec![item("providers", 2, 3, 4, None)], false)),
        "Pasted 1 collection into copy: 2 documents, 4 skipped, 3 overwritten"
    );
    assert_eq!(
        copy_summary(&report(vec![item("providers", 8, 0, 0, None), item("orders", 0, 0, 0, Some("duplicate _id 3"))], false)),
        "Pasted 1 collection into copy: 8 documents; orders failed: duplicate _id 3"
    );
    assert_eq!(
        copy_summary(&report(vec![item("providers", 8, 0, 0, None), item("orders", 5, 0, 0, None)], true)),
        "Copy cancelled after 13 documents into copy"
    );
    assert!(copy_summary(&report(vec![], true)).starts_with("Copy cancelled after "));
    assert_eq!(copy_summary(&report(vec![], false)), "Pasted 0 collections");
    let mut warned = item("orders", 5, 0, 0, None);
    warned.warning = Some("Collection History may be incomplete: disk full".into());
    assert_eq!(copy_summary(&report(vec![warned.clone()], false)), "Pasted 1 collection into copy: 5 documents; orders: Collection History may be incomplete: disk full");
    assert!(!copy_failed(&report(vec![warned.clone()], false)), "a warning is not a failed copy");
    assert!(copy_troubled(&report(vec![warned], false)), "but its toast shows as an error");
    assert!(!copy_troubled(&report(vec![item("orders", 5, 0, 0, None)], false)));
    assert!(copy_failed(&report(vec![item("orders", 0, 0, 0, Some("x"))], false)));
    assert!(!copy_failed(&report(vec![item("orders", 0, 0, 0, None)], true)));
}

/// With the pointer over the tree ⌘C copies the selected collection and
/// ⌘V opens the Paste dialog for the selected database; anywhere else
/// both are left for the result view.
#[test]
fn tree_shortcuts_copy_and_paste() {
    use crate::sidebar::{CollNode, DbNode, OpenConn, Selection};
    use rongo_core::{CollInfo, CollKind, DbInfo};
    let (ctx, mut app) = test_app(false);
    let p = ConnectionProfile::mock();
    let cid = p.conn_id();
    let mut conn = OpenConn::new(p);
    let coll = CollNode::new(CollInfo { name: "c".into(), kind: CollKind::Collection });
    conn.dbs = Some(vec![DbNode { info: DbInfo { name: "db".into(), size_on_disk: 0 }, colls: Some(vec![coll]), loading: false }]);
    app.conns.insert(cid, conn);
    app.sidebar.open = true;
    app.sidebar_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(200.0, 600.0));
    let over = egui::pos2(100.0, 100.0);
    let away = egui::pos2(500.0, 100.0);

    // Whether the event was still there for the result view afterwards.
    let press = |app: &mut RongoApp, at: egui::Pos2, event: egui::Event| {
        let mut left = false;
        let mut out = ctx.run_ui(egui::RawInput { events: vec![egui::Event::PointerMoved(at), event.clone()], ..Default::default() }, |ui| {
            app.shortcuts(ui.ctx());
            left = ui.input(|i| i.events.iter().any(|e| std::mem::discriminant(e) == std::mem::discriminant(&event)));
        });
        out.textures_delta.clear();
        left
    };
    let copy = || egui::Event::Copy;
    let paste = || egui::Event::Paste("c".into());

    app.sidebar.selection = Selection { conn: Some(cid), db: Some("db".into()), coll: Some("c".into()) };
    assert!(press(&mut app, away, copy()), "left for the results");
    assert!(app.coll_clip.is_none(), "away from the tree, nothing is copied");
    assert!(!press(&mut app, over, copy()), "the tree took ⌘C");
    let clip = app.coll_clip.as_ref().expect("the selected collection is copied");
    assert_eq!((clip.conn, clip.db.as_str(), clip.colls.as_slice()), (cid, "db", ["c".to_string()].as_slice()));

    app.sidebar.selection = Selection { conn: Some(cid), db: Some("db".into()), coll: None };
    assert!(press(&mut app, over, copy()), "a database has no ⌘C of its own");
    assert!(press(&mut app, away, paste()), "left for the results");
    assert!(app.paste_dialog.is_none());
    assert!(!press(&mut app, over, paste()), "the tree took ⌘V");
    assert!(app.paste_dialog.is_some(), "the Paste dialog opens for the selected database");

    // A connection alone is nothing to paste into.
    app.paste_dialog = None;
    app.sidebar.selection = Selection { conn: Some(cid), db: None, coll: None };
    assert!(press(&mut app, over, paste()));
    assert!(app.paste_dialog.is_none());
}

/// A read-only target can't be pasted into: its menus' Paste items are
/// off, and the action itself opens nothing.
#[test]
fn paste_is_disabled_on_read_only_connections() {
    use egui_kittest::{Harness, kittest::{NodeT, Queryable}};
    let mut h = harness(egui::vec2(1200.0, 800.0), true);
    let cid = ConnectionProfile::mock().conn_id();
    let settle = |h: &mut Harness<RongoApp>, what: &str, done: &dyn Fn(&mut Harness<RongoApp>) -> bool| {
        let end = Instant::now() + WAIT;
        while Instant::now() < end {
            h.step();
            if done(h) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for {what}");
    };
    settle(&mut h, "the tree lists demo-games", &|h| h.query_all_by_label_contains("demo-games").any(|n| n.rect().right() < 340.0));
    h.state_mut().coll_clip = Some(CollClip { conn: cid, db: "demo-games".into(), colls: vec!["providers".into()] });

    // Whether a database menu item is off.
    let menu_item_disabled = |h: &mut Harness<RongoApp>, label: &str, click: bool| {
        let row = h.get_all_by_label_contains("demo-games").find(|n| n.rect().right() < 340.0).expect("the database row");
        row.click_secondary();
        h.run_steps(3);
        let item = h.get_by_label_contains(label);
        let disabled = item.accesskit_node().is_disabled();
        if click {
            item.click();
        } else {
            h.key_press(egui::Key::Escape);
        }
        h.run_steps(3);
        disabled
    };
    let paste_item_disabled = |h: &mut Harness<RongoApp>| menu_item_disabled(h, "Paste Collection", false);

    // Copy All Collections waits for the collections, and leaves out system.*.
    assert!(menu_item_disabled(&mut h, "Copy All Collections", false), "not loaded yet");
    let colls = ["providers", "system.views", "games"].map(|n| crate::sidebar::CollNode::new(rongo_core::CollInfo { name: n.into(), kind: rongo_core::CollKind::Collection }));
    h.state_mut().conns.get_mut(&cid).unwrap().db_mut("demo-games").unwrap().colls = Some(colls.into());
    assert!(!menu_item_disabled(&mut h, "Copy All Collections", true), "loaded");
    let clip = h.state().coll_clip.as_ref().expect("copied");
    assert_eq!((clip.conn, clip.db.as_str(), clip.colls.as_slice()), (cid, "demo-games", ["providers".to_string(), "games".to_string()].as_slice()));
    assert!(!paste_item_disabled(&mut h), "unlocked, with something copied: Paste is on");
    h.state_mut().coll_clip = None;
    assert!(paste_item_disabled(&mut h), "nothing copied: Paste is off");

    h.state_mut().coll_clip = Some(CollClip { conn: cid, db: "demo-games".into(), colls: vec!["providers".into()] });
    h.state_mut().tree_action(TreeAction::SetReadOnly(cid, true));
    settle(&mut h, "the lock", &|h| h.state().conns[&cid].profile.read_only);
    assert!(paste_item_disabled(&mut h), "locked: Paste is off");
    h.state_mut().tree_action(TreeAction::PasteCollections { conn: cid, db: "demo-games".into() });
    assert!(h.state().paste_dialog.is_none(), "locked: nothing opens");
    h.state_mut().tree_action(TreeAction::PasteAsDatabase(cid));
    assert!(h.state().confirm.is_none(), "locked: no name is asked");
}

#[test]
fn new_database_names_follow_mongodb_rules() {
    assert!(database_name_error("copy").is_none());
    assert!(database_name_error("demo-games_2").is_none());
    assert!(database_name_error("").is_some());
    for bad in ["a b", "a.b", "a/b", "a\\b", "a\"b", "a$b", "a\0b"] {
        assert!(database_name_error(bad).is_some(), "{bad:?}");
    }
    assert!(database_name_error(&"x".repeat(63)).is_none());
    assert!(database_name_error(&"x".repeat(64)).is_some());
}

/// Paste as New Database asks for a name, then opens the dialog for it.
#[test]
fn paste_as_new_database_asks_a_name() {
    let (_ctx, mut app, a) = connected_mock_app();
    app.tree_action(TreeAction::PasteAsDatabase(a));
    assert!(app.confirm.is_none(), "nothing copied: nothing to ask");
    app.coll_clip = Some(CollClip { conn: a, db: "demo-games".into(), colls: vec!["providers".into()] });
    app.tree_action(TreeAction::PasteAsDatabase(a));
    let c = app.confirm.as_mut().expect("asks a name");
    assert!(c.input.is_some());
    c.input = Some("bad name".into());
    let c = app.confirm.take().unwrap();
    app.confirmed(c);
    assert!(app.paste_dialog.is_none(), "an invalid name opens nothing");
    app.tree_action(TreeAction::PasteAsDatabase(a));
    app.confirm.as_mut().unwrap().input = Some(" fresh ".into());
    let c = app.confirm.take().unwrap();
    app.confirmed(c);
    assert!(app.paste_dialog.is_some(), "the dialog opens for the new database");
}
