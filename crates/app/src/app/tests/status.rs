//! The corner's running work, and what a write's outcome says.

use super::*;

#[test]
fn progress_keeps_the_request_and_shows_in_the_corner() {
    let (_ctx, mut app) = test_app(false);
    let now = Instant::now();
    let sent = Sent { what: Some("Copying 2 collections".into()), started: now - Duration::from_secs(2), ..Sent::new(Origin::Notify { view: None, tree: None }) };
    app.requests.pending.insert(7, sent);
    app.handle(Response { id: 7, conn: None, event: Event::Progress { done: 12_000, total: Some(50_000), note: "sit.orders (1/2)".into() } });
    assert!(app.requests.pending.contains_key(&7), "progress is not an answer");
    let ops = app.operations(now);
    assert!(ops[0].text.contains("sit.orders (1/2)") && ops[0].text.contains("12,000 / 50,000"), "{}", ops[0].text);
    assert!(ops[0].text.starts_with("Copying 2 collections"), "{}", ops[0].text);
    app.handle(Response { id: 7, conn: None, event: Event::Done });
    assert!(!app.requests.pending.contains_key(&7), "the terminal event ends it");
}

/// A copy's line in the corner names both ends, the connection it reads
/// from and the one it writes to, before and during its progress.
#[test]
fn copy_progress_names_both_connections() {
    let (_ctx, mut app) = test_app(false);
    let [sit, uat] = [(1, "SIT"), (2, "UAT")].map(|(n, name)| ConnectionProfile { id: uuid::Uuid::from_u128(n), name: name.into(), ..ConnectionProfile::mock() });
    let (a, b) = (sit.conn_id(), uat.conn_id());
    app.conns.insert(a, OpenConn::new(sit));
    app.conns.insert(b, OpenConn::new(uat));
    let now = Instant::now();
    let sent = Sent { conn: Some(a), what: Some("Copying 2 collections".into()), started: now - Duration::from_secs(2), ..Sent::new(Origin::Copy { to: b, db: "uat".into() }) };
    app.requests.pending.insert(7, sent);
    let text = |app: &RongoApp| app.operations(now)[0].text.clone();
    assert_eq!(text(&app), "Copying 2 collections · SIT → UAT");
    app.handle(Response { id: 7, conn: Some(a), event: Event::Progress { done: 1000, total: Some(2500), note: "uat.orders (1/2)".into() } });
    assert_eq!(text(&app), "Copying 2 collections · SIT → UAT… uat.orders (1/2) 1,000 / 2,500");
}

#[test]
fn status_bar_shows_and_discards_slow_work() {
    let (ctx, mut app) = test_app(false);
    let p = ConnectionProfile { name: "alpha".into(), ..ConnectionProfile::mock() };
    let cid = p.conn_id();
    app.conns.insert(cid, OpenConn::new(p));
    // One fixed "now" for the whole test: how long work has run is then
    // exact, however slowly the machine draws.
    let now = Instant::now();
    let ago = |ms| now - Duration::from_millis(ms);
    let slow = Sent { conn: Some(cid), what: Some("Running collStats on db".into()), started: ago(2500), ..Sent::new(Origin::Info { conn: cid, title: "Statistics".into() }) };
    // A result view's own request shows in that view instead.
    let in_view = Sent { conn: Some(cid), what: Some("Running find on db.c".into()), started: ago(2500), ..Sent::new(Origin::View(1, ViewReq::Page)) };
    app.requests.pending.insert(44, in_view);
    let quick = Sent { conn: Some(cid), what: Some("Loading databases".into()), started: ago(50), ..Sent::new(Origin::Databases(cid)) };
    let unseen = Sent { conn: Some(cid), what: None, started: ago(5000), ..Sent::new(Origin::Fields(cid, Namespace::new("db", "c"))) };
    app.requests.pending.insert(41, slow);
    app.requests.pending.insert(42, quick);
    app.requests.pending.insert(43, unseen);

    let ops = app.operations(now);
    assert_eq!(ops.iter().map(|o| o.text.as_str()).collect::<Vec<_>>(), ["Running collStats on db · alpha"]);

    let screen = Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(900.0, 600.0)));
    let draw = |app: &mut RongoApp, events: Vec<egui::Event>| {
        let mut out = ctx.run_ui(egui::RawInput { screen_rect: screen, events, ..Default::default() }, |ui| app.activity_at(ui.ctx(), now));
        out.textures_delta.clear();
        let mut texts = Vec::new();
        for s in &out.shapes {
            if let egui::Shape::Text(t) = &s.shape {
                texts.push((t.galley.text().to_string(), t.visual_bounding_rect().center()));
            }
        }
        texts
    };
    // A new area is measured on its first frame and shown from the next.
    draw(&mut app, vec![]);
    let texts = draw(&mut app, vec![]);
    assert!(texts.iter().any(|(t, _)| t == "Running collStats on db · alpha…"), "{texts:?}");
    assert!(texts.iter().all(|(_, p)| p.x < 450.0 && p.y > 500.0), "in the bottom-left corner: {texts:?}");
    assert!(texts.iter().any(|(t, _)| t == "2s"), "how long it has run: {texts:?}");
    let (_, x) = texts.iter().find(|(t, _)| t == icon::X_CIRCLE).cloned().expect("a discard button");
    let press = |pressed| egui::Event::PointerButton { pos: x, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
    draw(&mut app, vec![egui::Event::PointerMoved(x), press(true)]);
    draw(&mut app, vec![press(false)]);
    assert!(app.operations(now).is_empty(), "discarded");
    assert!(app.requests.pending.contains_key(&41), "its reply (cancelled) still finds its way back");
    let texts = draw(&mut app, vec![]);
    assert!(texts.is_empty(), "nothing running, nothing drawn: {texts:?}");
}

/// The corner asks for a repaint only for what it shows, at most every
/// 100 ms. A find or count that a result view shows itself used to
/// redraw the whole window at full speed while it ran (also in a closed
/// or background tab), and work in the corner redrew it 30 times a
/// second.
#[test]
fn the_corner_repaints_only_for_what_it_shows() {
    let (ctx, mut app) = test_app(false);
    // The start-up history purge asks for a repaint from its own thread
    // when it is done. A loaded machine runs it late, among the frames
    // measured below (the "nothing" frame then asked for 0 ns): wait
    // for it first.
    if let Some(purge) = app.startup_purge.take() {
        let _ = purge.join();
    }
    let now = Instant::now();
    let ago = |ms| now - Duration::from_millis(ms);
    // The repaint the last of a few frames asks for. egui's own clock
    // moves a second a frame, so its fades (the corner's, as it
    // appears, and the context's first frames) are over by then.
    let clock = std::cell::Cell::new(0.0);
    let delay = |app: &mut RongoApp| {
        let mut last = Duration::ZERO;
        for _ in 0..4 {
            clock.set(clock.get() + 1.0);
            let input = egui::RawInput { time: Some(clock.get()), ..frame_input() };
            let mut out = ctx.run_ui(input, |ui| app.activity_at(ui.ctx(), now));
            out.textures_delta.clear();
            last = out.viewport_output[&egui::ViewportId::ROOT].repaint_delay;
        }
        last
    };
    let in_view = Sent { what: Some("Running find on d.c".into()), started: ago(2000), ..Sent::new(Origin::View(1, ViewReq::Page)) };
    app.requests.pending.insert(1, in_view);
    assert_eq!(delay(&mut app), Duration::MAX, "a view's own request asks for nothing");

    let corner = Sent { what: Some("Dropping d.c".into()), started: ago(100), ..Sent::new(Origin::Notify { view: None, tree: None }) };
    app.requests.pending.insert(2, corner);
    // egui takes a frame's time (1/60 s) off what was asked for.
    let d = delay(&mut app);
    assert!(d > Duration::from_millis(150) && d <= Duration::from_millis(200), "when it is due to show: {d:?}");

    app.requests.pending.get_mut(&2).unwrap().started = ago(2000);
    let d = delay(&mut app);
    assert!(d > Duration::from_millis(80) && d <= Duration::from_millis(100), "while it shows: {d:?}");
}

/// A rename shows in the corner without ⊗ and can't be cancelled: only
/// the server's answer moves the history, so a rename done on the
/// server can't leave it under the old name.
#[test]
fn a_rename_cannot_be_discarded() {
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, cid, "hist_ren_stop", 2);
    delete_ids(&ctx, &mut app, cid, &ns, &[1]);
    app.tree_action(TreeAction::Rename(cid, ns.clone()));
    let mut c = app.confirm.take().expect("asks for the name");
    c.input = Some("hist_ren_stop2".into());
    app.confirmed(c);
    let id = *app.requests.pending.iter().find(|(_, s)| matches!(s.origin, Origin::Rename { .. })).expect("sent").0;
    let op = app.operations(Instant::now() + Duration::from_secs(5)).into_iter().find(|o| o.key == OpKey::Request(id)).expect("shown in the corner");
    assert!(!op.stop, "no ⊗");
    app.cancel_op(OpKey::Request(id));
    assert!(app.requests.pending.get(&id).is_some_and(|s| s.what.is_some()), "not cancelled");
    step_until(&ctx, &mut app, "renamed", |a| !a.requests.pending.contains_key(&id));
    // The answer queued the move, which runs on the history files' thread:
    // done once they are idle (it may already be, egui running a frame twice).
    step_until(&ctx, &mut app, "the history files are done", |a| a.history_files.idle());
    let to = Namespace::new("demo-games", "hist_ren_stop2");
    assert_eq!(app.history_store().unwrap().list(cid.0, &to).unwrap().len(), 1, "the history moved");
}

/// Work the server finishes once begun (a drop, a clear) shows in the
/// corner without ⊗ and can't be cancelled, as a rename: "cancelled"
/// while the server carried on hid what it did. What stops between
/// batches (an import, a duplicate) or only reads keeps its ⊗.
#[test]
fn writes_that_run_to_completion_have_no_discard() {
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = Namespace::new("demo-games", "providers");
    let gone = Namespace::new("demo-games", "game_logs");
    let drop = app.net().send(Origin::Dropped { conn: cid, ns: gone.clone() }, Some(cid), Command::DropCollection { ns: gone });
    let clear = app.net().send(Origin::Notify { view: None, tree: None }, Some(cid), Command::ClearCollection { ns: ns.clone() });
    let import = app.net().send(Origin::Tool { view: None, tree: None }, Some(cid), Command::Import { ns: ns.clone(), path: "/nonexistent/rongo.json".into(), format: rongo_core::ImportFormat::Auto });
    let info = app.net().send(Origin::Info { conn: cid, title: "Server Status".into() }, Some(cid), Command::RunCommand { db: "admin".into(), cmd: doc! { "serverStatus": 1 } });
    let dup = app.net().send(Origin::TreeWrite { conn: cid, db: "demo-games".into() }, Some(cid), Command::DuplicateCollection { ns: Namespace::new("demo-games", "users"), to: "users_dup".into() });
    let ops = app.operations(Instant::now() + Duration::from_secs(5));
    let stop = |id| ops.iter().find(|o| o.key == OpKey::Request(id)).expect("shown in the corner").stop;
    assert!(!stop(drop) && !stop(clear), "no ⊗");
    assert!(stop(import) && stop(info) && stop(dup), "⊗");
    app.cancel_op(OpKey::Request(clear));
    assert!(app.requests.pending.get(&clear).is_some_and(|s| s.what.is_some()), "not cancelled");
    step_until(&ctx, &mut app, "answered", |a| ![drop, clear, import, info, dup].iter().any(|id| a.requests.pending.contains_key(id)));
    assert_eq!(count_docs(&app, cid, &ns), 0, "cleared");
}

/// An import's toast says how many went in, how many were skipped for
/// an `_id` already in the collection, and how many were refused.
#[test]
fn import_outcome_names_skipped_and_refused_documents() {
    let w = rongo_core::WriteOutcome { inserted: 2496, duplicates: 2, warning: Some("2 documents refused; the first: server error 121: Document failed validation".into()), ..Default::default() };
    assert_eq!(outcome_text(&Event::Written(w)), "Inserted 2,496 documents, skipped 2 with an _id already in the collection; 2 documents refused; the first: server error 121: Document failed validation");
    let w = rongo_core::WriteOutcome { duplicates: 3, ..Default::default() };
    assert_eq!(outcome_text(&Event::Written(w)), "Skipped 3 with an _id already in the collection");
    let w = rongo_core::WriteOutcome { warning: Some("1 document refused: x".into()), ..Default::default() };
    assert_eq!(outcome_text(&Event::Written(w)), "Nothing was written; 1 document refused: x");
    let w = rongo_core::WriteOutcome { inserted: 1000, duplicates: 5, cancelled: true, ..Default::default() };
    assert_eq!(outcome_text(&Event::Written(w)), "Inserted 1,000 documents, skipped 5 with an _id already in the collection, then stopped (cancelled)");
}
