//! The History tab: listing, previewing, restoring and removing entries.

use super::*;

/// A second conflict check doesn't start while one is on its way (or
/// its question is open): its answer used to replace the first one's
/// question, which then vanished.
#[test]
fn one_restore_question_at_a_time() {
    let (ctx, mut app, cid) = connected_mock_app();
    let mut tabs = Vec::new();
    for name in ["hist_one", "hist_two"] {
        let ns = history_coll(&ctx, &mut app, cid, name, 2);
        delete_ids(&ctx, &mut app, cid, &ns, &[1, 2]);
        open_history(&ctx, &mut app, cid, &ns);
        tabs.push((app.tabs[app.active_tab].id, history_tab(&app).selected().cloned().unwrap()));
    }
    let checks = |app: &RongoApp| app.requests.pending.values().filter(|s| matches!(s.origin, Origin::History { req: HistoryReq::Check, .. })).count();
    let [(one, first), (two, second)] = [tabs[0].clone(), tabs[1].clone()];
    app.history_action(one, crate::history_tab::HistoryAction::Restore(first));
    app.history_action(two, crate::history_tab::HistoryAction::Restore(second.clone()));
    assert_eq!(checks(&app), 1, "the second waits");
    assert!(app.history_tab_mut(two).is_some_and(|h| h.running.is_none()));
    assert!(toast_texts(&app).iter().any(|t| t.contains("Another restore")), "{:?}", toast_texts(&app));

    step_until(&ctx, &mut app, "the first check answers", |a| a.restore_ask.is_some());
    assert_eq!(app.restore_ask.as_ref().map(|a| a.tab), Some(one));
    app.history_action(two, crate::history_tab::HistoryAction::Restore(second));
    assert_eq!(checks(&app), 0, "nor while its question is open");
    assert_eq!(app.restore_ask.as_ref().map(|a| a.tab), Some(one));
}

/// One frame with `key` pressed (with `modifiers` held), and one after
/// it with the modifiers let go.
fn press_key(ctx: &egui::Context, app: &mut RongoApp, key: Key, modifiers: Modifiers) {
    let mut frame = eframe::Frame::_new_kittest();
    let pressed = vec![egui::Event::ModifiersChanged(modifiers), egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }];
    for events in [pressed, vec![egui::Event::ModifiersChanged(Modifiers::NONE)]] {
        let mut input = frame_input();
        input.events = events;
        let mut out = ctx.run_ui(input, |ui| eframe::App::ui(app, ui, &mut frame));
        out.textures_delta.clear();
    }
}

/// One frame with ↩ pressed.
fn press_enter(ctx: &egui::Context, app: &mut RongoApp) {
    let mut input = frame_input();
    input.events.push(egui::Event::Key { key: Key::Enter, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE });
    let mut frame = eframe::Frame::_new_kittest();
    let mut out = ctx.run_ui(input, |ui| eframe::App::ui(app, ui, &mut frame));
    out.textures_delta.clear();
}

/// The texts of the app's next frame with their colours.
fn drawn_colours(ctx: &egui::Context, app: &mut RongoApp) -> Vec<(String, egui::Color32)> {
    let mut frame = eframe::Frame::_new_kittest();
    let mut out = ctx.run_ui(frame_input(), |ui| eframe::App::ui(app, ui, &mut frame));
    out.textures_delta.clear();
    text_shapes(&out).map(|t| (t.galley.text().to_string(), t.galley.job.sections.first().map_or(t.fallback_color, |s| s.format.color))).collect()
}

/// The tree's "Collection History" item opens a tab for the collection:
/// its entries newest first, with the time in the Settings time zone,
/// the kind and the count; the newest is selected and its saved
/// documents previewed in a table. Opening it again shows the same tab.
#[test]
fn history_item_opens_the_tab_and_lists_entries() {
    let (ctx, mut app, cid) = connected_mock_app();
    app.settings.timezone = rongo_core::format::DisplayTz::Offset(7 * 60);
    let ns = history_coll(&ctx, &mut app, cid, "hist_tab", 3);
    delete_ids(&ctx, &mut app, cid, &ns, &[1, 2]);

    open_history(&ctx, &mut app, cid, &ns);
    let h = history_tab(&app);
    let entries = h.entries().to_vec();
    assert_eq!(entries.iter().map(|e| (e.kind, e.count, e.complete, e.restored)).collect::<Vec<_>>(), [(Kind::Delete, 2, true, false)]);
    assert_eq!(h.selected(), Some(&entries[0].id), "the newest is selected");
    assert_eq!(h.previewed(), Some(2), "its documents are previewed");
    let texts = drawn_texts(&ctx, &mut app);
    assert!(texts.iter().any(|t| t.ends_with("Collection History - hist_tab")), "the tab's title: {texts:?}");
    assert!(texts.iter().any(|t| t.ends_with("Delete")), "the kind: {texts:?}");
    assert!(texts.iter().any(|t| t.len() > 19 && t.ends_with("+07:00")), "the time, in the Settings time zone: {texts:?}");
    assert!(texts.iter().any(|t| t == "document 2"), "the saved documents in the table: {texts:?}");

    let tabs = app.tabs.len();
    app.active_tab = 0;
    app.tree_action(TreeAction::OpenHistory(cid, ns.clone()));
    assert_eq!(app.tabs.len(), tabs, "no second tab");
    assert!(matches!(app.tabs[app.active_tab].kind, TabKind::History(_)));
}

/// ↩ starts a restore of the selected entry with the conflict check;
/// then, conflicts or not, the restore asks first: it names the
/// connection and the collection, says what it will do, and writes
/// nothing until "Restore 2" is clicked (↩ doesn't answer it). Then the
/// documents are back, the toast says how many, and the entry is listed
/// as restored. Nothing was replaced, so no new entry is kept.
#[test]
fn restore_puts_documents_back_and_marks_the_entry() {
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, cid, "hist_restore", 3);
    delete_ids(&ctx, &mut app, cid, &ns, &[1, 2]);
    assert_eq!(count_docs(&app, cid, &ns), 1);

    open_history(&ctx, &mut app, cid, &ns);
    app.toasts.clear();
    press_enter(&ctx, &mut app);
    assert!(history_sent(&app, HistoryReq::Check), "↩ started the check");
    step_until(&ctx, &mut app, "the restore asks", |a| a.restore_ask.is_some());
    assert!(history_idle(&app), "nothing sent but the check");
    let texts = drawn_texts(&ctx, &mut app);
    let name = app.conns[&cid].profile.name.clone();
    for want in [name.as_str(), "· demo-games.hist_restore", "Insert 2 deleted documents back into demo-games.hist_restore.", "Inserted documents are not kept in Collection History.", "Restore 2"] {
        assert!(texts.iter().any(|t| t == want), "{want}: {texts:?}");
    }
    assert!(texts.iter().any(|t| t.starts_with("From the delete of 2 documents saved ")), "the entry's time: {texts:?}");
    assert!(!texts.iter().any(|t| t.starts_with("Skip")), "no conflicts, no conflict choices: {texts:?}");
    assert!(!texts.iter().any(|t| t.starts_with("Saved from")), "saved from the connection as it is named now: {texts:?}");
    press_enter(&ctx, &mut app);
    assert!(app.restore_ask.is_some() && history_idle(&app), "↩ doesn't answer it");
    assert_eq!(count_docs(&app, cid, &ns), 1, "nothing restored yet");

    click_text(&ctx, &mut app, "Restore 2", |t| t == "Restore 2");
    assert!(app.restore_ask.is_none() && history_sent(&app, HistoryReq::Restore), "the click restores");
    step_until(&ctx, &mut app, "the restore is done", |a| history_idle(a) && a.toasts.iter().any(|t| t.text.starts_with("Restored")));
    assert_eq!(toast_texts(&app), ["Restored 2 documents into demo-games.hist_restore"]);
    assert_eq!(count_docs(&app, cid, &ns), 3);
    step_until(&ctx, &mut app, "the entry is listed as restored", |a| history_tab(a).entries().first().is_some_and(|e| e.restored));
    assert_eq!(history_tab(&app).entries().len(), 1, "nothing replaced, nothing new kept");
    assert!(drawn_texts(&ctx, &mut app).iter().any(|t| t == "Restored"), "marked in the list");
}

/// The restore question names the connection it writes to as it is
/// named now; an entry saved while it had another name says that too.
#[test]
fn the_restore_question_says_what_the_entry_was_saved_from() {
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, cid, "hist_saved_from", 2);
    delete_ids(&ctx, &mut app, cid, &ns, &[1]);
    let then = app.conns[&cid].profile.name.clone();
    app.conns.get_mut(&cid).unwrap().profile.name = "Renamed since".into();
    open_history(&ctx, &mut app, cid, &ns);
    let tab = app.tabs[app.active_tab].id;
    let entry = history_tab(&app).selected().cloned().unwrap();
    app.history_action(tab, crate::history_tab::HistoryAction::Restore(entry));
    step_until(&ctx, &mut app, "the restore asks", |a| a.restore_ask.is_some());
    let texts = drawn_texts(&ctx, &mut app);
    assert!(texts.iter().any(|t| t == "Renamed since"), "{texts:?}");
    assert!(texts.iter().any(|t| *t == format!("Saved from {then}")), "{texts:?}");
}

/// A deleted document whose `_id` is back conflicts: the question shows
/// how many and which, and offers Skip, Overwrite and Insert as New.
/// Skip restores the others and leaves that one as it is.
#[test]
fn conflicts_ask_which_way() {
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, cid, "hist_conflict", 3);
    delete_ids(&ctx, &mut app, cid, &ns, &[1, 2, 3]);
    app.net().send(Origin::Notify { view: None, tree: None }, Some(cid), Command::Insert { ns: ns.clone(), doc: doc! { "_id": 2, "name": "back again" } });
    step_until(&ctx, &mut app, "the document is back", writes_done);

    open_history(&ctx, &mut app, cid, &ns);
    let tab = app.tabs[app.active_tab].id;
    let entry = history_tab(&app).selected().cloned().unwrap();
    app.history_action(tab, crate::history_tab::HistoryAction::Restore(entry));
    step_until(&ctx, &mut app, "the conflict check answers", |a| a.restore_ask.is_some());
    let ask = app.restore_ask.as_ref().unwrap();
    assert_eq!((ask.total, ask.conflicts, ask.sample.as_slice()), (3, 1, ["2".to_string()].as_slice()));
    assert_eq!((ask.on_conflict, ask.count()), (rongo_core::OnConflict::Skip, 2), "Skip unless chosen otherwise");
    let texts = drawn_texts(&ctx, &mut app);
    for want in ["1 of 3", "Skip", "Overwrite", "Insert as New", "Restore 2"] {
        assert!(texts.iter().any(|t| t.contains(want)), "{want}: {texts:?}");
    }
    assert_eq!(count_docs(&app, cid, &ns), 1, "nothing restored before the answer");

    app.toasts.clear();
    app.restore_answer(true);
    step_until(&ctx, &mut app, "the restore is done", |a| history_idle(a) && a.toasts.iter().any(|t| t.text.starts_with("Restored")));
    assert_eq!(toast_texts(&app), ["Restored 2 documents into demo-games.hist_conflict, 1 skipped"]);
    assert_eq!(count_docs(&app, cid, &ns), 3);
    let two = find_docs(&app, cid, &ns, doc! { "_id": 2 });
    assert_eq!(two[0].get_str("name").unwrap(), "back again", "the conflicting document was left alone");
}

/// For an update, "Show changes" puts each saved version beside the
/// document as it is now (read with one query): a changed field's old
/// value in the removed colour and its new one in the added colour, a
/// removed field in the removed colour, an added one in the added colour.
#[test]
fn update_preview_marks_changed_fields() {
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, cid, "hist_changes", 3);
    let update = doc! { "$set": { "n": 20, "tag": "new" }, "$unset": { "name": "" } };
    app.view_action(1, ViewAction::UpdateOne { conn: cid, ns: ns.clone(), id: Bson::Int32(2), update });
    step_until(&ctx, &mut app, "the update is done", writes_done);

    open_history(&ctx, &mut app, cid, &ns);
    assert_eq!(history_tab(&app).entries()[0].kind, Kind::Update);
    assert!(drawn_texts(&ctx, &mut app).iter().any(|t| t.contains("Show changes")), "offered for an update");
    if let TabKind::History(h) = &mut app.tabs[app.active_tab].kind {
        h.show_changes = true;
    }
    step_until(&ctx, &mut app, "the changes are shown", |a| history_tab(a).changes_ready());
    let shown = drawn_colours(&ctx, &mut app);
    let has = |text: &str, colour| shown.iter().any(|(t, c)| t == text && *c == colour);
    assert!(has("n: 2", theme::ERROR) && has("n: 20", theme::OK), "a changed field: {shown:?}");
    assert!(has("name: \"document 2\"", theme::ERROR), "a removed field: {shown:?}");
    assert!(has("tag: \"new\"", theme::OK), "an added field: {shown:?}");
}

/// On a read-only connection Restore is off and says why on hover; ↩
/// sends nothing.
#[test]
fn restore_is_disabled_on_read_only() {
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, cid, "hist_locked", 2);
    delete_ids(&ctx, &mut app, cid, &ns, &[1]);
    app.tree_action(TreeAction::SetReadOnly(cid, true));

    open_history(&ctx, &mut app, cid, &ns);
    assert_eq!(history_tab(&app).restore_blocked(app.conns.get(&cid)), Some(crate::result_view::LOCKED));
    press_enter(&ctx, &mut app);
    assert!(history_idle(&app), "↩ sent nothing");

    // Hovering the button shows the reason.
    let label = format!("{}  Restore", icon::ARROW_U_UP_LEFT);
    let mut frame = eframe::Frame::_new_kittest();
    let mut out = ctx.run_ui(frame_input(), |ui| eframe::App::ui(&mut app, ui, &mut frame));
    out.textures_delta.clear();
    let at = text_shapes(&out).find(|t| t.galley.text() == label).map(|t| t.galley.rect.translate(t.pos.to_vec2()).center()).expect("the Restore button is drawn");
    let mut texts = Vec::new();
    // Until the tooltip shows (it has a hover delay), by the clock.
    let end = Instant::now() + WAIT;
    for i in 0.. {
        if Instant::now() > end {
            break;
        }
        let mut input = frame_input();
        if i == 0 {
            input.events.push(egui::Event::PointerMoved(at));
        }
        let mut out = ctx.run_ui(input, |ui| eframe::App::ui(&mut app, ui, &mut frame));
        out.textures_delta.clear();
        texts = texts_of(&out);
        if texts.iter().any(|t| t == crate::result_view::LOCKED) {
            break;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    assert!(texts.iter().any(|t| t == crate::result_view::LOCKED), "{texts:?}");
}

/// A time series collection keeps no history: no tab opens for one the
/// tree lists as time series. One the tree doesn't list (dropped) opens.
#[test]
fn no_history_tab_for_time_series() {
    let (_ctx, mut app, cid) = connected_mock_app();
    let c = app.conns.get_mut(&cid).unwrap();
    c.set_databases(vec![rongo_core::DbInfo { name: "tsdb".into(), size_on_disk: 0 }]);
    c.db_mut("tsdb").unwrap().colls = Some(vec![CollNode::new(rongo_core::CollInfo { name: "ticks".into(), kind: rongo_core::CollKind::Timeseries })]);
    let tabs = app.tabs.len();
    app.tree_action(TreeAction::OpenHistory(cid, Namespace::new("tsdb", "ticks")));
    assert_eq!(app.tabs.len(), tabs);
    app.tree_action(TreeAction::OpenHistory(cid, Namespace::new("tsdb", "dropped")));
    assert_eq!(app.tabs.len(), tabs + 1, "a collection the tree doesn't list (dropped) still opens");
}

/// The tab's menu removes one entry, or every entry of the collection,
/// each after asking.
#[test]
fn remove_and_empty_ask_first() {
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, cid, "hist_remove", 3);
    for id in 1..=3 {
        delete_ids(&ctx, &mut app, cid, &ns, &[id]);
    }
    open_history(&ctx, &mut app, cid, &ns);
    assert_eq!(history_tab(&app).entries().len(), 3);
    let tab = app.tabs[app.active_tab].id;
    let newest = history_tab(&app).entries()[0].id.clone();

    app.history_action(tab, crate::history_tab::HistoryAction::Remove(newest.clone()));
    let ask = app.confirm.take().expect("asks first");
    assert_eq!(ask.title, "Remove from History");
    app.confirmed(ask);
    step_until(&ctx, &mut app, "listed again", |a| history_tab(a).listed() && history_tab(a).entries().len() == 2);
    assert!(!history_tab(&app).entries().iter().any(|e| e.id == newest));

    app.history_action(tab, crate::history_tab::HistoryAction::Empty);
    let ask = app.confirm.take().expect("asks first");
    assert_eq!(ask.title, "Empty Collection History");
    assert!(ask.message.contains("2 entries"), "{}", ask.message);
    app.confirmed(ask);
    // Emptied off the UI thread, as Settings deletes.
    assert_eq!(app.history_files.doing(), Some("Emptying a collection's history…"));
    step_until(&ctx, &mut app, "listed again", |a| history_tab(a).listed() && history_tab(a).entries().is_empty());
    assert!(app.history_store().unwrap().list(cid.0, &ns).unwrap().is_empty());
}

/// Empty History removes only the entries recorded before the question:
/// a delete made after it (while Empty waited behind other work, say)
/// keeps its before-images.
#[test]
fn empty_keeps_what_was_recorded_after_the_question() {
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, cid, "hist_empty_later", 3);
    delete_ids(&ctx, &mut app, cid, &ns, &[1]);
    open_history(&ctx, &mut app, cid, &ns);
    let tab = app.tabs[app.active_tab].id;
    app.history_action(tab, crate::history_tab::HistoryAction::Empty);
    let ask = app.confirm.take().expect("asks first");
    let Pending::EmptyHistory { before, .. } = ask.pending else { panic!("the Empty question") };
    // Recorded once the clock has passed the question.
    while rongo_core::bson::DateTime::now().timestamp_millis() <= before {
        std::thread::sleep(Duration::from_millis(1));
    }
    delete_ids(&ctx, &mut app, cid, &ns, &[2]);
    app.confirmed(ask);
    step_until(&ctx, &mut app, "emptied and listed again", |a| a.history_files.idle() && history_tab(a).listed());
    let left = app.history_store().unwrap().list(cid.0, &ns).unwrap();
    assert_eq!(left.len(), 1, "the delete made after the question keeps its entry");
    let saved: Vec<Bson> = app.history_store().unwrap().read(&left[0].id).unwrap().map(|r| r.unwrap().before.get("_id").unwrap().unwrap().try_into().unwrap()).collect();
    assert_eq!(saved, [Bson::Int32(2)]);
}

/// A dropped collection's history still opens (from the tree's action,
/// as Settings will), and restoring its delete recreates the collection.
#[test]
fn restoring_a_delete_recreates_a_dropped_collection() {
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, cid, "hist_dropped", 2);
    delete_ids(&ctx, &mut app, cid, &ns, &[1, 2]);
    app.net().send(Origin::Notify { view: None, tree: None }, Some(cid), Command::DropCollection { ns: ns.clone() });
    step_until(&ctx, &mut app, "dropped", writes_done);

    open_history(&ctx, &mut app, cid, &ns);
    let tab = app.tabs[app.active_tab].id;
    let entry = history_tab(&app).selected().cloned().unwrap();
    app.history_action(tab, crate::history_tab::HistoryAction::Restore(entry));
    step_until(&ctx, &mut app, "the restore asks", |a| a.restore_ask.is_some());
    app.restore_answer(true);
    step_until(&ctx, &mut app, "the restore is done", |a| history_idle(a) && a.toasts.iter().any(|t| t.text.starts_with("Restored")));
    assert_eq!(count_docs(&app, cid, &ns), 2);
}

/// Cancel and Esc close the restore question without sending anything.
#[test]
fn restore_cancel_and_escape_send_nothing() {
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, cid, "hist_cancel", 2);
    delete_ids(&ctx, &mut app, cid, &ns, &[1, 2]);
    open_history(&ctx, &mut app, cid, &ns);
    let tab = app.tabs[app.active_tab].id;
    let entry = history_tab(&app).selected().cloned().unwrap();
    let ask = |ctx: &egui::Context, app: &mut RongoApp| {
        app.history_action(tab, crate::history_tab::HistoryAction::Restore(entry.clone()));
        step_until(ctx, app, "the restore asks", |a| a.restore_ask.is_some());
    };

    ask(&ctx, &mut app);
    drawn_texts(&ctx, &mut app);
    press_key(&ctx, &mut app, Key::Escape, Modifiers::NONE);
    assert!(app.restore_ask.is_none(), "Esc closes it");
    assert!(history_idle(&app), "and sends nothing");

    ask(&ctx, &mut app);
    click_text(&ctx, &mut app, "Cancel", |t| t == "Cancel");
    assert!(app.restore_ask.is_none(), "Cancel closes it");
    assert!(history_idle(&app), "and sends nothing");
    assert_eq!(count_docs(&app, cid, &ns), 0);
}

/// ↩ belongs to the preview's table once it is clicked: it doesn't start
/// a restore. ⇧↩ never does. A click on the entry list gives ↩ back, and
/// even then it only starts the check: the restore asks before writing.
#[test]
fn return_in_the_preview_does_not_restore() {
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, cid, "hist_keys", 2);
    delete_ids(&ctx, &mut app, cid, &ns, &[1, 2]);
    open_history(&ctx, &mut app, cid, &ns);

    click_text(&ctx, &mut app, "a preview row", |t| t == "document 1");
    press_key(&ctx, &mut app, Key::Enter, Modifiers::NONE);
    press_key(&ctx, &mut app, Key::Enter, Modifiers::SHIFT);
    assert!(history_idle(&app) && app.restore_ask.is_none(), "↩ and ⇧↩ in the preview send nothing");

    click_text(&ctx, &mut app, "the entry's row", |t| t.ends_with("Delete"));
    press_key(&ctx, &mut app, Key::Enter, Modifiers::SHIFT);
    press_key(&ctx, &mut app, Key::Enter, Modifiers::ALT);
    assert!(history_idle(&app), "⇧↩ and ⌥↩ send nothing");
    press_key(&ctx, &mut app, Key::Enter, Modifiers::NONE);
    assert!(history_sent(&app, HistoryReq::Check) && !history_sent(&app, HistoryReq::Restore), "↩ on the list checks, and only that");
    step_until(&ctx, &mut app, "the restore asks", |a| a.restore_ask.is_some());
    assert_eq!(count_docs(&app, cid, &ns), 0, "nothing restored");
}

/// ⊗ in the corner on a running check also tells its tab: a check answer
/// already on its way neither restores nor asks.
#[test]
fn cancel_from_the_corner_stops_the_check() {
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, cid, "hist_corner", 2);
    delete_ids(&ctx, &mut app, cid, &ns, &[1, 2]);
    open_history(&ctx, &mut app, cid, &ns);
    let tab = app.tabs[app.active_tab].id;
    let entry = history_tab(&app).selected().cloned().unwrap();
    app.history_action(tab, crate::history_tab::HistoryAction::Restore(entry));
    let req = history_tab(&app).running.as_ref().expect("checking").req;

    app.cancel_op(OpKey::Request(req));
    assert!(history_tab(&app).running.as_ref().is_some_and(|r| r.cancelling), "the tab knows");
    app.handle(Response { id: req, conn: Some(cid), event: Event::HistoryConflicts { total: 2, conflicts: 0, sample: Vec::new() } });
    assert!(app.restore_ask.is_none() && history_idle(&app), "no question, no restore");
    step_until(&ctx, &mut app, "settled", |a| history_tab(a).running.is_none());
    assert_eq!(count_docs(&app, cid, &ns), 0);
}

/// Remove from History and Empty History wait while a recorded write on
/// the collection is still writing its entry, also when it began after
/// the question was asked.
#[test]
fn remove_waits_for_writes_on_the_collection() {
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, cid, "hist_busy", 2);
    delete_ids(&ctx, &mut app, cid, &ns, &[1]);
    open_history(&ctx, &mut app, cid, &ns);
    let tab = app.tabs[app.active_tab].id;
    let entry = history_tab(&app).selected().cloned().unwrap();
    // Before the tab has drawn again (or while it is not on screen).
    writing(&mut app, cid, &ns);
    for a in [crate::history_tab::HistoryAction::Remove(entry.clone()), crate::history_tab::HistoryAction::Empty] {
        app.history_action(tab, a);
        assert!(app.confirm.is_none(), "not even asked");
    }
    assert_eq!(app.toasts.last().map(|t| t.text.as_str()), Some(crate::history_tab::WRITING));
    drawn_texts(&ctx, &mut app);
    assert_eq!(history_tab(&app).removal_blocked(), Some(crate::history_tab::WRITING), "the menu waits");

    written(&mut app);
    drawn_texts(&ctx, &mut app);
    assert_eq!(history_tab(&app).removal_blocked(), None);
    app.history_action(tab, crate::history_tab::HistoryAction::Remove(entry.clone()));
    let ask = app.confirm.take().expect("asks now");
    writing(&mut app, cid, &ns);
    app.confirmed(ask);
    assert_eq!(app.history_store().unwrap().list(cid.0, &ns).unwrap().len(), 1, "a write began meanwhile: kept");
    written(&mut app);
}

/// A "Show changes" read that fails after its tab closed says nothing; a
/// restore that fails still says so.
#[test]
fn a_closed_tabs_failed_comparison_is_quiet() {
    let (_ctx, mut app) = test_app(false);
    let ns = Namespace::new("d", "c");
    let fail = |app: &mut RongoApp, id: RequestId, req: HistoryReq| {
        app.requests.pending.insert(id, crate::net::Sent::new(Origin::History { tab: 12_345, ns: ns.clone(), req }));
        app.handle(Response { id, conn: None, event: Event::Failed(CoreError::Other("boom".into())) });
    };
    fail(&mut app, 1, HistoryReq::Compare);
    assert!(app.toasts.is_empty(), "{:?}", toast_texts(&app));
    fail(&mut app, 2, HistoryReq::Restore);
    assert_eq!(toast_texts(&app), ["Restoring into d.c failed: boom"]);
}

/// What a restore did, for its toast: counts, a stop, and a warning
/// (shown as an error).
#[test]
fn restore_toast_says_what_happened() {
    let ns = Namespace::new("d", "c");
    let r = |restored, skipped, overwritten, inserted_new, cancelled, warning: Option<&str>| {
        let ev = Event::Restored { restored, skipped, overwritten, inserted_new, recorded: None, cancelled, warning: warning.map(str::to_string), unconfirmed: false, unconfirmed_docs: 0 };
        restored_toast(&ns, &ev)
    };
    assert_eq!(r(1, 0, 0, 0, false, None), ("Restored 1 document into d.c".to_string(), false));
    assert_eq!(r(3, 1, 2, 4, false, None), ("Restored 9 documents into d.c (2 overwritten, 4 with new _ids), 1 skipped".to_string(), false));
    assert_eq!(r(1000, 0, 0, 0, true, None), ("Restored 1,000 documents into d.c, then stopped (cancelled)".to_string(), false));
    assert_eq!(r(0, 0, 0, 0, true, None), ("The restore stopped before anything was restored".to_string(), false));
    assert_eq!(r(0, 2, 0, 0, false, None), ("Nothing restored into d.c, 2 skipped".to_string(), false));
    assert_eq!(r(5, 0, 0, 0, false, Some("disk full")), ("Restored 5 documents into d.c; disk full".to_string(), true));
    // A write the server did not confirm.
    let unconfirmed = |restored, warning: &str| {
        let ev = Event::Restored { restored, skipped: 0, overwritten: 0, inserted_new: 0, recorded: None, cancelled: false, warning: Some(warning.into()), unconfirmed: true, unconfirmed_docs: 0 };
        restored_toast(&ns, &ev)
    };
    assert_eq!(unconfirmed(0, "network error: reset"), ("The server did not confirm this write: it may have been applied. Check before retrying. (network error: reset)".to_string(), true));
    assert_eq!(unconfirmed(1000, "network error: reset"), ("The server did not confirm this write: it may have been applied. Check before retrying. Confirmed before it: restored 1,000 documents into d.c. (network error: reset)".to_string(), true));
    // Documents put back under a write-concern error are counted, but never called confirmed.
    let ev = Event::Restored { restored: 5, skipped: 1, overwritten: 0, inserted_new: 2, recorded: None, cancelled: false, warning: Some("write concern error 64: waiting for replication timed out".into()), unconfirmed: true, unconfirmed_docs: 4 };
    assert_eq!(restored_toast(&ns, &ev), ("The server did not confirm this write: it may have been applied. Check before retrying. Confirmed: restored 3 documents into d.c, 1 skipped. Not confirmed: 4 documents restored. (write concern error 64: waiting for replication timed out)".to_string(), true));
}
