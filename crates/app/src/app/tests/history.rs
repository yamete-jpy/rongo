//! Recorded writes: what Collection History keeps, and asking before going on without it.

use super::*;

/// Collection History lives in the local data folder, which a Windows
/// roaming profile doesn't sync (`%LOCALAPPDATA%\rongo\rongo\data\history`).
/// On macOS that is the folder it always was. Only paths are worked out:
/// nothing is read or written.
#[test]
fn history_lives_in_the_local_data_folder() {
    let dirs = directories::ProjectDirs::from("dev", "rongo", "rongo").expect("a home folder");
    let root = crate::app::history_root(&dirs);
    assert_eq!(root, dirs.data_local_dir().join("history"));
    if cfg!(target_os = "macos") {
        assert_eq!(root, dirs.data_dir().join("history"), "unchanged on macOS");
    }
}

/// The start-up purge's failure is said: history would otherwise stay
/// over its limits with no word of it.
#[test]
fn a_failed_startup_purge_is_said() {
    let (ctx, mut app) = test_app(false);
    // A file where the history folder should be.
    let root = app._temp.0.join("not-a-folder");
    std::fs::write(&root, "x").unwrap();
    app.startup_purge = purge_history_at_start(root, &HistorySettings::default(), Arc::new(|| {}));
    let end = Instant::now() + WAIT;
    while app.startup_purge.as_ref().is_some_and(|h| !h.is_finished()) {
        assert!(Instant::now() < end, "the purge never finished");
        std::thread::sleep(Duration::from_millis(10));
    }
    step_until(&ctx, &mut app, "a frame takes it in", |_| true);
    assert!(app.startup_purge.is_none(), "taken in");
    let texts = toast_texts(&app);
    assert!(texts.iter().any(|t| t.starts_with("Collection History could not be trimmed at start-up: could not read ") && t.contains("not-a-folder")), "{texts:?}");
}

const KEEP_BOX: &str = "Keep the documents in Collection History";

/// A write known to touch one document (Set to null, Remove Field, Edit
/// Value, the editor's Save) re-reads only that document, with the
/// query's projection: the page is not fetched again and the selection
/// stays. An insert still fetches the page.
#[test]
fn a_one_document_write_rereads_only_that_document() {
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, cid, "reread", 5);
    app.open_collection(cid, ns.clone());
    let view = match &app.tabs[app.active_tab].kind {
        TabKind::Collection(c) => c.view.id,
        _ => unreachable!(),
    };
    app.view_mut(view).unwrap().set_spec(FindSpec { projection: Some(doc! { "n": 1 }), ..FindSpec::all(ns.clone(), 50) });
    let shown = |a: &RongoApp| a.tabs.iter().find_map(|t| match &t.kind {
        TabKind::Collection(c) if c.view.id == view => c.view.shown(),
        _ => None,
    });
    step_until(&ctx, &mut app, "the page is shown", |a| shown(a).is_some_and(|d| d.len() == 5));
    app.view_mut(view).unwrap().select_rows(&[1, 3]);
    let n_of = |a: &RongoApp, id: i32| shown(a).and_then(|d| d.into_iter().find(|d| d.get_i32("_id").ok() == Some(id))).and_then(|d| d.get("n").cloned());
    // Every page and single-document read the view sends, as they go out.
    let sent = std::cell::RefCell::new(std::collections::BTreeMap::<RequestId, bool>::new());
    let watch = |a: &RongoApp| {
        for (id, s) in &a.requests.pending {
            match &s.origin {
                Origin::View(v, ViewReq::Page) if *v == view => sent.borrow_mut().insert(*id, true),
                Origin::View(v, ViewReq::RefreshDoc(_)) if *v == view => sent.borrow_mut().insert(*id, false),
                _ => None,
            };
        }
    };
    let settled = |a: &RongoApp| writes_done(a) && a.tool.is_none() && a.doc_dialog.is_none() && !a.requests.pending.values().any(|s| matches!(s.origin, Origin::View(..) | Origin::OpenDoc { .. }));
    let check = |app: &mut RongoApp, what: &str, done: &dyn Fn(&RongoApp) -> bool| {
        sent.borrow_mut().clear();
        step_until(&ctx, app, what, |a| {
            watch(a);
            settled(a) && done(a)
        });
        let pages = sent.borrow().values().filter(|p| **p).count();
        let reads = sent.borrow().values().filter(|p| !**p).count();
        assert_eq!((pages, reads), (0, 1), "{what}: page fetches, single-document reads");
        assert_eq!(app.view_mut(view).unwrap().selected_rows(), [1, 3], "{what}: the selection stays");
        let doc = shown(app).unwrap();
        assert!(doc.iter().all(|d| !d.contains_key("name")), "{what}: read with the query's projection: {doc:?}");
    };

    app.view_action(view, ViewAction::UpdateOne { conn: cid, ns: ns.clone(), id: Bson::Int32(2), update: doc! { "$set": { "n": Bson::Null } } });
    check(&mut app, "Set to null", &|a| n_of(a, 2) == Some(Bson::Null));
    app.view_action(view, ViewAction::UpdateOne { conn: cid, ns: ns.clone(), id: Bson::Int32(4), update: doc! { "$unset": { "n": "" } } });
    check(&mut app, "Remove Field", &|a| n_of(a, 4).is_none() && shown(a).is_some_and(|d| d.len() == 5));
    app.view_action(view, ViewAction::EditValue { conn: cid, ns: ns.clone(), id: Bson::Int32(3), path: "n".into(), value: Bson::Int32(3) });
    let tool = app.tool.as_mut().expect("Edit Value opens");
    if let ToolKind::Value { text, .. } = &mut tool.kind {
        *text = "30".into();
    }
    let cmd = tool.build().unwrap();
    app.tool_send(cmd);
    check(&mut app, "Edit Value", &|a| n_of(a, 3) == Some(Bson::Int32(30)));
    app.doc_dialog = Some(DocDialog::edit(cid, ns.clone(), &doc! { "_id": 5, "n": 5, "name": "document 5" }, Some(view)));
    app.save_doc(doc! { "_id": 5, "n": 50, "name": "document 5" });
    check(&mut app, "Save", &|a| n_of(a, 5) == Some(Bson::Int32(50)));

    sent.borrow_mut().clear();
    app.doc_dialog = Some(DocDialog::insert(cid, ns.clone(), None, Some(view)));
    app.save_doc(doc! { "_id": 6, "n": 6 });
    step_until(&ctx, &mut app, "the insert shows", |a| {
        watch(a);
        settled(a) && shown(a).is_some_and(|d| d.len() == 6)
    });
    assert!(sent.borrow().values().any(|p| *p), "an insert fetches the page");
}

/// With the default settings, deleting a selected row, Edit Value and
/// Set to null each keep what they change in Collection History, under
/// the connection's profile and the collection.
#[test]
fn deletes_and_edits_are_recorded_by_default() {
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, cid, "hist_default", 5);

    delete_ids(&ctx, &mut app, cid, &ns, &[1]);

    app.view_action(1, ViewAction::EditValue { conn: cid, ns: ns.clone(), id: Bson::Int32(2), path: "n".into(), value: Bson::Int32(20) });
    let cmd = app.tool.as_mut().expect("Edit Value opens").build().unwrap();
    app.tool_send(cmd);
    step_until(&ctx, &mut app, "the value is saved", |a| a.tool.is_none());

    app.view_action(1, ViewAction::UpdateOne { conn: cid, ns: ns.clone(), id: Bson::Int32(3), update: doc! { "$set": { "n": Bson::Null } } });
    step_until(&ctx, &mut app, "set to null", writes_done);

    let entries = app.history_store().expect("the test key").list(cid.0, &ns).unwrap();
    let count = |k: Kind| entries.iter().filter(|e| e.kind == k).count();
    assert_eq!((entries.len(), count(Kind::Delete), count(Kind::Update)), (3, 1, 2), "{entries:?}");
    let name = app.conns[&cid].profile.name.clone();
    assert!(entries.iter().all(|e| e.complete && e.count == 1 && e.conn_name == name), "{entries:?}");
    assert_eq!(count_docs(&app, cid, &ns), 4, "and the delete happened");
}

/// History off in Settings sends the plain writes, keeping nothing. A
/// connection's own override wins over Settings either way, for that
/// connection only; turning deletes off keeps no deletes.
#[test]
fn history_off_sends_plain_commands() {
    let (ctx, mut app, a) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, a, "hist_off", 4);
    let store = app.history_store().unwrap();

    app.settings.history.enabled = false;
    delete_ids(&ctx, &mut app, a, &ns, &[1]);
    assert!(store.list(a.0, &ns).unwrap().is_empty(), "history off: nothing kept");
    assert_eq!(count_docs(&app, a, &ns), 3, "but deleted");

    // On in Settings, off for B.
    app.settings.history.enabled = true;
    let b = connect_mock_b(&ctx, &mut app);
    let nsb = history_coll(&ctx, &mut app, b, "hist_off", 4);
    app.conns.get_mut(&b).unwrap().profile.history = Some(false);
    delete_ids(&ctx, &mut app, b, &nsb, &[1]);
    assert!(store.list(b.0, &nsb).unwrap().is_empty(), "off for B");
    assert_eq!(count_docs(&app, b, &nsb), 3);
    delete_ids(&ctx, &mut app, a, &ns, &[2]);
    assert_eq!(store.list(a.0, &ns).unwrap().len(), 1, "A follows Settings");

    // Off in Settings, on for B.
    app.settings.history.enabled = false;
    app.conns.get_mut(&b).unwrap().profile.history = Some(true);
    delete_ids(&ctx, &mut app, b, &nsb, &[2]);
    assert_eq!(store.list(b.0, &nsb).unwrap().len(), 1, "B's own On wins");

    // Deletes not recorded, updates still are.
    app.settings.history.enabled = true;
    app.settings.history.deletes = false;
    delete_ids(&ctx, &mut app, a, &ns, &[3]);
    assert_eq!(store.list(a.0, &ns).unwrap().len(), 1, "no delete kept");
    app.view_action(1, ViewAction::UpdateOne { conn: a, ns: ns.clone(), id: Bson::Int32(4), update: doc! { "$unset": { "n": "" } } });
    step_until(&ctx, &mut app, "the field is removed", writes_done);
    assert_eq!(store.list(a.0, &ns).unwrap().len(), 2, "the update kept");
}

/// The Delete Documents dialog offers "Keep the documents in Collection
/// History", ticked; unticked, nothing is kept. With history off for the
/// connection the box isn't there.
#[test]
fn dialog_checkbox_follows_settings() {
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, cid, "hist_dialog", 4);
    let open = |app: &mut RongoApp, ids: &[i32]| {
        let selected = ids.iter().map(|i| Bson::Int32(*i)).collect();
        app.view_action(1, ViewAction::Bulk { conn: cid, ns: ns.clone(), delete: true, selected, query: doc! {}, field: None });
    };
    fn bulk(app: &mut RongoApp) -> &mut crate::tools::BulkState {
        match &mut app.tool.as_mut().expect("the dialog is open").kind {
            ToolKind::Bulk(b) => b,
            _ => panic!("not the Delete dialog"),
        }
    }
    let send = |app: &mut RongoApp| {
        let cmd = app.tool.as_mut().unwrap().build().unwrap();
        app.tool_send(cmd);
        step_until(&ctx, app, "the delete is done", |a| a.tool.is_none());
    };

    open(&mut app, &[1, 2]);
    assert!(drawn_texts(&ctx, &mut app).iter().any(|t| t == KEEP_BOX), "the box is offered");
    assert!(bulk(&mut app).keep_history, "ticked by default");
    bulk(&mut app).keep_history = false;
    send(&mut app);
    assert!(app.history_store().unwrap().list(cid.0, &ns).unwrap().is_empty(), "unticked: nothing kept");
    assert_eq!(count_docs(&app, cid, &ns), 2);

    open(&mut app, &[3]);
    send(&mut app);
    assert_eq!(app.history_store().unwrap().list(cid.0, &ns).unwrap().len(), 1, "ticked: kept");

    app.settings.history.enabled = false;
    open(&mut app, &[4]);
    assert!(!drawn_texts(&ctx, &mut app).iter().any(|t| t == KEEP_BOX), "history off: no box");
}

/// A size limit the history would pass asks before going on without it,
/// with the sizes; OK waits for the collection's name. Nothing is
/// deleted before that; then the plain delete runs and nothing is kept.
#[test]
fn too_large_asks_before_deleting_without_history() {
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, cid, "hist_big", 3);
    app.settings.history.max_mb = 0;
    app.view_action(1, ViewAction::Delete { conn: cid, ns: ns.clone(), ids: vec![Bson::Int32(1), Bson::Int32(2)] });
    let c = app.confirm.take().unwrap();
    app.confirmed(c);
    step_until(&ctx, &mut app, "the size check answers", |a| a.confirm.is_some());
    let ask = app.confirm.as_ref().unwrap();
    assert_eq!(ask.title, WITHOUT_HISTORY);
    assert!(ask.message.contains("would need about") && ask.message.contains("the size limit is 0 MB") && ask.message.contains("free"), "{}", ask.message);
    assert!(drawn_texts(&ctx, &mut app).iter().any(|t| t == WITHOUT_HISTORY), "shown");
    assert_eq!(count_docs(&app, cid, &ns), 3, "nothing deleted yet");

    let ask = app.confirm.as_mut().unwrap();
    assert!(!ask.ok_enabled(), "the name first");
    ask.input = Some("hist".into());
    assert!(!ask.ok_enabled());
    ask.input = Some(ns.coll.clone());
    assert!(ask.ok_enabled());
    let c = app.confirm.take().unwrap();
    app.confirmed(c);
    step_until(&ctx, &mut app, "the plain delete is done", writes_done);
    assert_eq!(count_docs(&app, cid, &ns), 1);
    assert!(app.history_store().unwrap().list(cid.0, &ns).unwrap().is_empty(), "nothing kept");
}

/// "Keep no history for this operation?" comes when the size check
/// answers, by when another question may be open (another write's, say):
/// that question is never replaced unanswered. The write that would have
/// asked is not sent, and a toast says so and why, to try again.
#[test]
fn a_late_history_question_never_replaces_an_open_one() {
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, cid, "hist_asking", 3);
    app.settings.history.max_mb = 0;
    let delete = |app: &mut RongoApp, id: i32| {
        app.view_action(1, ViewAction::Delete { conn: cid, ns: ns.clone(), ids: vec![Bson::Int32(id)] });
        let c = app.confirm.take().unwrap();
        app.confirmed(c);
    };
    let not_sent = |app: &RongoApp| toast_texts(app).iter().filter(|t| t.contains("would need about") && t.contains("was not sent")).count();

    // Another question is open when the size check answers.
    delete(&mut app, 1);
    app.tree_action(TreeAction::DropCollection(cid, ns.clone()));
    step_until(&ctx, &mut app, "the size check answers", writes_done);
    assert_eq!(app.confirm.as_ref().map(|c| c.title.as_str()), Some("Drop Collection"), "still open");
    assert_eq!(not_sent(&app), 1, "{:?}", toast_texts(&app));
    app.confirm = None;

    // Two writes refused for their size: one asks, the other is not sent.
    delete(&mut app, 1);
    delete(&mut app, 2);
    step_until(&ctx, &mut app, "both size checks answer", writes_done);
    assert_eq!(app.confirm.as_ref().map(|c| c.title.as_str()), Some(WITHOUT_HISTORY));
    assert_eq!(not_sent(&app), 2, "{:?}", toast_texts(&app));
    let mut c = app.confirm.take().unwrap();
    c.input = Some(ns.coll.clone());
    app.confirmed(c);
    step_until(&ctx, &mut app, "the one asked about is deleted", writes_done);
    assert_eq!(count_docs(&app, cid, &ns), 2, "only the write that asked");
}

/// Without the history key (only while the vault is locked, when the
/// window shows nothing else), history is unavailable: the Update/Delete
/// dialog would say so instead of offering the box, and a delete asks
/// "Keep no history for this operation?" with the reason before going on.
#[test]
fn unavailable_history_asks_before_deleting() {
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, cid, "hist_nokey", 2);
    assert_eq!(app.history_offer(cid, &ns, Kind::Delete), HistoryOffer::On);
    app.vault = None;
    assert!(app.history_store().is_none());
    assert!(app.record_for(cid, &ns, Kind::Delete).is_none());
    assert_eq!(app.history_offer(cid, &ns, Kind::Delete), HistoryOffer::Unavailable("the vault is locked".into()));

    app.view_action(1, ViewAction::Delete { conn: cid, ns: ns.clone(), ids: vec![Bson::Int32(1)] });
    let c = app.confirm.take().unwrap();
    app.confirmed(c);
    let ask = app.confirm.as_mut().expect("asks before going on without history");
    assert_eq!(ask.title, WITHOUT_HISTORY);
    assert!(ask.message.contains("the vault is locked"), "{}", ask.message);
    assert!(!ask.ok_enabled());
    ask.input = Some(ns.coll.clone());
    let c = app.confirm.take().unwrap();
    app.confirmed(c);
    step_until(&ctx, &mut app, "the plain delete is done", writes_done);
    assert_eq!(count_docs(&app, cid, &ns), 1);
}

/// A recorded write whose history key can't be checked first (the vault
/// changed in another window) doesn't use up what the user did: the
/// confirmed Delete keeps its question and the Paste dialog stays open,
/// to go on once unlocked again. A cell's Set to null carries nothing
/// typed: it sends nothing and the toast says to try again.
#[test]
fn a_write_that_cannot_check_the_key_keeps_what_was_asked() {
    use crate::test_support::TEST_PASSWORD;
    let (ctx, mut app, a) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, a, "hist_kept", 3);
    let path = app.vault.as_ref().unwrap().path().to_path_buf();
    let unlock_again = |app: &mut RongoApp, pw: &str| {
        let v = Arc::new(Vault::unlock(path.clone(), pw).unwrap());
        app.unlock_answer(&ctx, UnlockResult::Unlocked { vault: v, keychain_left: None });
    };

    Vault::unlock(path.clone(), TEST_PASSWORD).unwrap().change_password(TEST_PASSWORD, "second password").unwrap();
    ask_delete(&mut app, a, &ns, 1);
    assert!(writes_done(&app) && app.vault.is_none(), "nothing sent; asks to unlock again");
    assert!(matches!(app.confirm.as_ref().map(|c| &c.pending), Some(Pending::Delete { .. })), "the question is kept");
    unlock_again(&mut app, "second password");
    let c = app.confirm.take().unwrap();
    app.confirmed(c);
    step_until(&ctx, &mut app, "the delete is done", writes_done);
    assert_eq!(count_docs(&app, a, &ns), 2, "deleted once unlocked");

    let b = connect_mock_b(&ctx, &mut app);
    app.tree_action(TreeAction::CopyCollections { conn: a, db: "demo-games".into(), colls: vec!["providers".into()] });
    app.tree_action(TreeAction::PasteCollections { conn: b, db: "demo-games".into() });
    step_until(&ctx, &mut app, "the target's collections are listed", |a| !a.requests.pending.values().any(|s| matches!(s.origin, Origin::Collections(..))));
    let d = app.paste_dialog.as_mut().expect("the Paste dialog");
    d.rows[0].mode = rongo_core::IfExists::Append(rongo_core::OnDuplicateId::Overwrite);
    let cmd = d.build();
    Vault::unlock(path.clone(), "second password").unwrap().change_password("second password", "third password").unwrap();
    app.paste_action(PasteAction::Send(cmd.clone()));
    assert!(app.vault.is_none() && !app.requests.pending.values().any(|s| matches!(s.origin, Origin::Copy { .. })), "nothing sent");
    assert!(app.paste_dialog.is_some(), "the Paste dialog stays");
    unlock_again(&mut app, "third password");
    app.paste_action(PasteAction::Send(cmd));
    step_until(&ctx, &mut app, "the paste finishes", |a| a.toasts.iter().any(|t| t.text.starts_with("Pasted")));
    assert!(app.paste_dialog.is_none());

    Vault::unlock(path.clone(), "third password").unwrap().change_password("third password", "fourth password").unwrap();
    app.toasts.clear();
    app.view_action(1, ViewAction::UpdateOne { conn: a, ns: ns.clone(), id: Bson::Int32(2), update: doc! { "$set": { "n": Bson::Null } } });
    assert!(writes_done(&app) && app.vault.is_none(), "nothing sent");
    assert_eq!(toast_texts(&app), ["The vault was changed in another rongo window: unlock again, then try again"]);
}

/// Asks to delete document `id` of `ns` as the table's Delete does and
/// confirms; returns at once (a sent delete is still waiting for its answer).
fn ask_delete(app: &mut RongoApp, cid: ConnId, ns: &Namespace, id: i32) {
    app.view_action(1, ViewAction::Delete { conn: cid, ns: ns.clone(), ids: vec![Bson::Int32(id)] });
    let c = app.confirm.take().expect("deleting asks first");
    app.confirmed(c);
}

/// Collection History is never sealed under a key another window's Reset
/// left behind: a recorded write catches the vault up first. A vault that
/// is busy just now is said, and nothing is sent. Reset there and a new
/// master password set (a new history key), a delete here sends nothing,
/// asks for the master password again and says why; no history file is
/// written, and the document is still there.
#[test]
fn a_recorded_write_after_another_windows_reset_sends_nothing() {
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, cid, "hist_reset_elsewhere", 3);
    let path = app.vault.as_ref().unwrap().path().to_path_buf();
    let store = app.history_store().unwrap();

    let held = std::fs::File::options().read(true).write(true).create(true).truncate(false).open(path.with_extension("lock")).unwrap();
    held.lock().unwrap();
    ask_delete(&mut app, cid, &ns, 1);
    drop(held);
    assert!(writes_done(&app), "busy: nothing sent");
    assert!(app.vault.is_some() && app.unlock.is_none(), "still unlocked");
    assert!(toast_texts(&app).iter().any(|t| t.contains("busy")), "{:?}", toast_texts(&app));

    // Another window: Reset, then a new master password.
    Vault::reset(&path).unwrap();
    let other = Vault::create(path.clone(), "another password", rongo_core::vault::KdfParams::TEST, BTreeMap::new()).unwrap();
    assert_ne!(other.history_key(), app.vault.as_ref().unwrap().history_key(), "a new history key");
    app.toasts.clear();
    ask_delete(&mut app, cid, &ns, 1);
    assert!(writes_done(&app), "changed: nothing sent");
    assert!(app.vault.is_none() && app.unlock.is_some(), "asks for the master password again");
    assert_eq!(toast_texts(&app), ["The vault was changed in another rongo window: unlock again, then try again"]);
    assert_eq!(count_docs(&app, cid, &ns), 3, "nothing deleted");
    assert_eq!(store.usage().unwrap(), 0, "no history written, under either key");
    let texts = drawn_texts(&ctx, &mut app);
    assert!(texts.iter().any(|t| t.contains("Unlock rongo")), "{texts:?}");
}

/// A restore catches the vault up too, before it reads its entry and
/// keeps what it overwrites: after another window changed the master
/// password, neither the restore (its question answered after the
/// change) nor the conflict check is sent, and the window asks for the
/// master password again.
#[test]
fn a_restore_after_another_windows_change_sends_nothing() {
    use crate::test_support::TEST_PASSWORD;
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, cid, "hist_restore_elsewhere", 2);
    delete_ids(&ctx, &mut app, cid, &ns, &[1]);
    open_history(&ctx, &mut app, cid, &ns);
    let tab = app.tabs[app.active_tab].id;
    let entry = history_tab(&app).selected().cloned().unwrap();
    let path = app.vault.as_ref().unwrap().path().to_path_buf();
    let other = Vault::unlock(path.clone(), TEST_PASSWORD).unwrap();
    let changed = "The vault was changed in another rongo window: unlock again, then try again";

    app.history_action(tab, crate::history_tab::HistoryAction::Restore(entry.clone()));
    step_until(&ctx, &mut app, "the restore asks", |a| a.restore_ask.is_some());
    other.change_password(TEST_PASSWORD, "another password").unwrap();
    app.toasts.clear();
    app.restore_answer(true);
    assert!(history_idle(&app), "the restore is not sent");
    assert!(app.vault.is_none() && app.unlock.is_some(), "asks for the master password again");
    assert_eq!(toast_texts(&app), [changed]);

    assert!(!app.tabs.iter().any(|t| t.id == tab), "locking again closed the History tab");

    // Unlocked again (the History tab opened again), then changed there again: the check isn't sent either.
    app.vault = Some(Arc::new(Vault::unlock(path, "another password").unwrap()));
    app.unlock = None;
    open_history(&ctx, &mut app, cid, &ns);
    let tab = app.tabs[app.active_tab].id;
    other.change_password("another password", "a third password").unwrap();
    app.toasts.clear();
    app.history_action(tab, crate::history_tab::HistoryAction::Restore(entry));
    assert!(history_idle(&app), "the check is not sent");
    assert!(app.vault.is_none() && app.unlock.is_some());
    assert_eq!(toast_texts(&app), [changed]);
    assert_eq!(count_docs(&app, cid, &ns), 1, "nothing restored");
}

/// Paste → Append → Overwrite keeps the target documents it replaces in
/// the target connection's history. A paste whose history would not fit
/// asks first and, confirmed, pastes without history.
#[test]
fn paste_overwrite_keeps_what_it_replaces() {
    let (ctx, mut app, a) = connected_mock_app();
    let b = connect_mock_b(&ctx, &mut app);
    let providers = Namespace::new("demo-games", "providers");
    app.tree_action(TreeAction::CopyCollections { conn: a, db: "demo-games".into(), colls: vec!["providers".into()] });
    let paste = |app: &mut RongoApp| {
        app.tree_action(TreeAction::PasteCollections { conn: b, db: "demo-games".into() });
        step_until(&ctx, app, "the target's collections are listed", |a| !a.requests.pending.values().any(|s| matches!(s.origin, Origin::Collections(..))));
        let d = app.paste_dialog.as_mut().expect("the Paste dialog");
        d.rows[0].mode = rongo_core::IfExists::Append(rongo_core::OnDuplicateId::Overwrite);
        let cmd = d.build();
        app.paste_action(PasteAction::Send(cmd));
    };
    let pasted = |a: &RongoApp| a.toasts.iter().any(|t| t.text.starts_with("Pasted"));

    paste(&mut app);
    step_until(&ctx, &mut app, "the paste finishes", pasted);
    let store = app.history_store().unwrap();
    let entries = store.list(b.0, &providers).unwrap();
    assert_eq!(entries.len(), 1, "{:?}", toast_texts(&app));
    assert_eq!((entries[0].kind, entries[0].count), (Kind::Update, 8));
    assert!(store.list(a.0, &providers).unwrap().is_empty(), "the target's history, not the source's");

    app.settings.history.max_mb = 0;
    app.toasts.clear();
    paste(&mut app);
    step_until(&ctx, &mut app, "the size check answers", |a| a.confirm.is_some());
    let ask = app.confirm.as_mut().unwrap();
    assert_eq!(ask.title, WITHOUT_HISTORY);
    assert!(!pasted(&app));
    let mut c = app.confirm.take().unwrap();
    c.input = Some("providers".into());
    app.confirmed(c);
    step_until(&ctx, &mut app, "the paste without history finishes", pasted);
    assert!(toast_texts(&app).iter().any(|t| t.contains("8 overwritten")), "{:?}", toast_texts(&app));
    assert_eq!(store.list(b.0, &providers).unwrap().len(), 1, "nothing more kept");
}

/// `record_for` is what every write consults: the profile it belongs
/// to and the settings it carries; nothing for a time series collection
/// (known from the tree's listing); a collection not listed yet is kept.
#[test]
fn record_for_follows_settings_and_skips_time_series() {
    let (_ctx, mut app, cid) = connected_mock_app();
    app.settings.history.keep_days = 7;
    app.settings.history.max_mb = 2;
    let plain = Namespace::new("tsdb", "plain");
    let r = app.record_for(cid, &plain, Kind::Update).expect("not listed yet: kept");
    assert_eq!((r.profile, r.conn_name.as_str(), r.keep_days, r.max_bytes), (cid.0, app.conns[&cid].profile.name.as_str(), 7, 2 << 20));
    assert_eq!(r.root, app.history_root());
    assert_ne!(app.history_root(), connected_mock_app().1.history_root(), "each test app has a history of its own");

    let c = app.conns.get_mut(&cid).unwrap();
    c.set_databases(vec![rongo_core::DbInfo { name: "tsdb".into(), size_on_disk: 0 }]);
    c.db_mut("tsdb").unwrap().colls = Some(vec![
        CollNode::new(rongo_core::CollInfo { name: "ticks".into(), kind: rongo_core::CollKind::Timeseries }),
        CollNode::new(rongo_core::CollInfo { name: "plain".into(), kind: rongo_core::CollKind::Collection }),
    ]);
    assert!(app.record_for(cid, &Namespace::new("tsdb", "ticks"), Kind::Delete).is_none(), "time series");
    assert!(app.record_for(cid, &plain, Kind::Delete).is_some());
    app.settings.history.updates = false;
    assert!(app.record_for(cid, &plain, Kind::Update).is_none(), "updates off");
    assert!(app.record_for(cid, &plain, Kind::Delete).is_some());
}

/// A recorded write that stopped part way, or finished with a problem,
/// says so in its toast, as an error.
#[test]
fn written_warnings_and_stops_are_shown() {
    let (_ctx, mut app) = test_app(false);
    let answer = |app: &mut RongoApp, id: RequestId, w: rongo_core::WriteOutcome| {
        app.requests.pending.insert(id, crate::net::Sent::new(Origin::Notify { view: None, tree: None }));
        app.handle(Response { id, conn: None, event: Event::Written(w) });
        let t = app.toasts.pop().expect("a toast");
        (t.text, t.error)
    };
    let (text, error) = answer(&mut app, 1, rongo_core::WriteOutcome { deleted: 1000, cancelled: true, ..Default::default() });
    assert_eq!(text, "Deleted 1,000 documents, then stopped (cancelled)");
    assert!(!error, "the user's own cancel");
    let warning = Some("delete: this connection is read-only".to_string());
    let (text, error) = answer(&mut app, 2, rongo_core::WriteOutcome { deleted: 1000, cancelled: true, warning, ..Default::default() });
    assert_eq!(text, "Deleted 1,000 documents, then stopped: delete: this connection is read-only");
    assert!(error);
    let warning = Some("Collection History may be incomplete: disk full".to_string());
    let (text, error) = answer(&mut app, 3, rongo_core::WriteOutcome { matched: 2, modified: 2, warning, ..Default::default() });
    assert_eq!(text, "Updated 2 of 2 matched; Collection History may be incomplete: disk full");
    assert!(error);
    // A write the server did not confirm never reads as "nothing was
    // written" (which invites the retry that could apply it twice).
    let warning = Some("network error: reset; the documents as they were are kept in Collection History".to_string());
    let (text, error) = answer(&mut app, 5, rongo_core::WriteOutcome { unconfirmed: true, warning, ..Default::default() });
    assert_eq!(text, "The server did not confirm this write: it may have been applied. Check before retrying. (network error: reset; the documents as they were are kept in Collection History)");
    assert!(error);
    let warning = Some("server error 14: Cannot apply $inc to a value of non-numeric type".to_string());
    let (text, _) = answer(&mut app, 6, rongo_core::WriteOutcome { matched: 1000, modified: 1000, unconfirmed: true, warning, ..Default::default() });
    assert_eq!(text, "The server did not confirm this write: it may have been applied. Check before retrying. Confirmed before it: updated 1,000 of 1,000 matched. (server error 14: Cannot apply $inc to a value of non-numeric type)");
    // An import whose batches met a write-concern error counts their
    // documents, which the primary took: they are never called confirmed.
    let warning = Some("write concern error 64: waiting for replication timed out".to_string());
    let (text, _) = answer(&mut app, 8, rongo_core::WriteOutcome { inserted: 2500, duplicates: 3, unconfirmed: true, unconfirmed_docs: 1000, warning: warning.clone(), ..Default::default() });
    assert_eq!(text, "The server did not confirm this write: it may have been applied. Check before retrying. Confirmed: inserted 1,500 documents, skipped 3 with an _id already in the collection. Not confirmed: 1,000 documents inserted. (write concern error 64: waiting for replication timed out)");
    let (text, _) = answer(&mut app, 9, rongo_core::WriteOutcome { inserted: 1, unconfirmed: true, unconfirmed_docs: 1, warning, ..Default::default() });
    assert_eq!(text, "The server did not confirm this write: it may have been applied. Check before retrying. Not confirmed: 1 document inserted. (write concern error 64: waiting for replication timed out)");

    // Edit Value stopped (⊗) before it changed anything: not "Value saved".
    let cid = ConnectionProfile::mock().conn_id();
    let mut value = ToolDialog::value(cid, None, Namespace::new("d", "c"), Bson::Int32(1), "n".into(), &Bson::Int32(1));
    value.running = Some(4);
    app.tool = Some(value);
    app.requests.pending.insert(4, crate::net::Sent::new(Origin::Tool { view: None, tree: None }));
    app.handle(Response { id: 4, conn: None, event: Event::Written(rongo_core::WriteOutcome { cancelled: true, ..Default::default() }) });
    assert_eq!(toast_texts(&app).last().map(String::as_str), Some("Stopped before anything was changed"));
    assert!(app.tool.is_none());
    // Edit Value the server did not confirm: not "Value saved".
    let mut value = ToolDialog::value(cid, None, Namespace::new("d", "c"), Bson::Int32(1), "n".into(), &Bson::Int32(1));
    value.running = Some(7);
    app.tool = Some(value);
    app.requests.pending.insert(7, crate::net::Sent::new(Origin::Tool { view: None, tree: None }));
    let warning = Some("network error: reset".to_string());
    app.handle(Response { id: 7, conn: None, event: Event::Written(rongo_core::WriteOutcome { unconfirmed: true, warning, ..Default::default() }) });
    assert_eq!(toast_texts(&app).last().map(String::as_str), Some("The server did not confirm this write: it may have been applied. Check before retrying. (network error: reset)"));

    // A paste whose collection may have an incomplete history: still pasted, shown as an error.
    let mut item = CopiedItem { src: Namespace::new("a", "p"), dst: Namespace::new("copy", "p"), inserted: 3, overwritten: 2, skipped: 0, recorded: None, warning: None, error: None, unconfirmed: false };
    item.warning = Some("Collection History may be incomplete: disk full".into());
    app.requests.pending.insert(5, crate::net::Sent::new(Origin::Copy { to: cid, db: "copy".into() }));
    app.handle(Response { id: 5, conn: None, event: Event::Copied(CopyReport { items: vec![item], cancelled: false }) });
    let t = app.toasts.pop().expect("the paste's toast");
    assert!(t.text.starts_with("Pasted 1 collection") && t.text.contains("may be incomplete"), "{}", t.text);
    assert!(t.error);
}

/// A write-concern error on a write without history (Paste Documents'
/// insert, the editor's Save, the Update dialog, Set to null, a drop)
/// means the primary applied it: it answers as unconfirmed, never as a
/// plain error inviting the retry that could apply it twice, and the view
/// reads what the server holds now. The editor stays open with why; a
/// drop closes the dropped collection's tabs, as the drop happened.
#[test]
fn a_write_concern_error_without_history_is_unconfirmed() {
    const SAID: &str = "The server did not confirm this write: it may have been applied. Check before retrying. (write concern error 64: waiting for replication timed out)";
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, cid, "concern", 3);
    app.open_collection(cid, ns.clone());
    let view = match &app.tabs[app.active_tab].kind {
        TabKind::Collection(c) => c.view.id,
        _ => unreachable!(),
    };
    let reading = |a: &RongoApp, page: bool| a.requests.pending.values().any(|s| match &s.origin {
        Origin::View(v, ViewReq::Page) => *v == view && page,
        Origin::View(v, ViewReq::RefreshDoc(_)) => *v == view && !page,
        _ => false,
    });
    step_until(&ctx, &mut app, "the page is shown", |a| !reading(a, true) && a.tabs.iter().any(|t| matches!(&t.kind, TabKind::Collection(c) if c.view.shown().is_some_and(|d| d.len() == 3))));
    let reply = |app: &mut RongoApp, id: RequestId, origin: Origin| {
        app.requests.pending.insert(id, Sent::new(origin));
        app.handle(Response { id, conn: Some(cid), event: Event::Failed(CoreError::WriteConcern { code: 64, msg: "waiting for replication timed out".into() }) });
    };

    // Paste Documents, Delete, Clear: a toast, as an error, and the page again.
    reply(&mut app, 90_001, Origin::Notify { view: Some(view), tree: None });
    let t = app.toasts.pop().expect("a toast");
    assert_eq!((t.text.as_str(), t.error), (SAID, true));
    step_until(&ctx, &mut app, "the page is read again", |a| reading(a, true));
    step_until(&ctx, &mut app, "and shown", |a| !reading(a, true));

    // Set to null, Remove Field: that document is read again.
    reply(&mut app, 90_002, Origin::UpdateOne { view, id: Bson::Int32(2) });
    assert_eq!(app.toasts.pop().map(|t| (t.text, t.error)), Some((SAID.to_string(), true)));
    step_until(&ctx, &mut app, "the document is read again", |a| reading(a, false));

    // The Update dialog without history: no "… finished".
    let mut update = ToolDialog::bulk(cid, ns.clone(), Some(view), false, Vec::new(), doc! {}, None);
    update.running = Some(90_003);
    app.tool = Some(update);
    reply(&mut app, 90_003, Origin::Tool { view: Some(view), tree: None });
    assert_eq!(app.toasts.pop().map(|t| (t.text, t.error)), Some((SAID.to_string(), true)));
    assert!(app.tool.is_none(), "closed, as for any write the server did not confirm");

    // The editor's insert: it stays open, ready again, saying why.
    let mut d = DocDialog::insert(cid, ns.clone(), None, Some(view));
    d.saving = Some(90_004);
    app.doc_dialog = Some(d);
    let toasts = app.toasts.len();
    reply(&mut app, 90_004, Origin::Save { view: Some(view) });
    let d = app.doc_dialog.as_ref().expect("the editor stays open");
    assert_eq!((d.saving, d.error.as_deref()), (None, Some(SAID)));
    assert_eq!(app.toasts.len(), toasts, "said in the editor");
    app.doc_dialog = None;

    // A drop: the collection is gone on the primary, so its tabs close.
    reply(&mut app, 90_005, Origin::Dropped { conn: cid, ns: ns.clone() });
    assert_eq!(app.toasts.pop().map(|t| (t.text, t.error)), Some((SAID.to_string(), true)));
    assert!(!app.tabs.iter().any(|t| matches!(&t.kind, TabKind::Collection(c) if c.ns == ns)), "its tab closed");
}

/// An import inserts as it reads, so one that fails part way (a file found
/// malformed) has left batches in: the tree and the open view of that
/// collection show them, as after an import that finished.
#[test]
fn a_failed_import_shows_what_went_in() {
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, cid, "imported", 3);
    app.open_collection(cid, ns.clone());
    let view = match &app.tabs[app.active_tab].kind {
        TabKind::Collection(c) => c.view.id,
        _ => unreachable!(),
    };
    let paging = |a: &RongoApp| a.requests.pending.values().any(|s| matches!(s.origin, Origin::View(v, ViewReq::Page) if v == view));
    step_until(&ctx, &mut app, "the page is shown", |a| !paging(a) && a.tabs.iter().any(|t| matches!(&t.kind, TabKind::Collection(c) if c.view.shown().is_some_and(|d| d.len() == 3))));
    let path = app._temp.0.join("rows.jsonl");
    let rows: String = (100..1600).map(|i| format!("{{\"_id\": {i}}}\n")).collect();
    std::fs::write(&path, format!("{rows}{{oops\n")).unwrap();
    app.tool = Some(ToolDialog::import(cid, ns.db.clone(), ns.coll.clone()));
    app.tool_send(Command::Import { ns: ns.clone(), path: path.clone(), format: rongo_core::ImportFormat::Auto });
    step_until(&ctx, &mut app, "the import fails", writes_done);
    let error = app.tool.as_ref().and_then(|t| t.error.clone()).expect("said in the dialog");
    assert!(error.starts_with("JSON: ") && error.contains("after inserting 1000 documents"), "{error}");
    assert!(app.requests.pending.values().any(|s| matches!(s.origin, Origin::Databases(c) if c == cid)), "the tree lists again");
    assert!(paging(&app), "the open view reads its page again");
    step_until(&ctx, &mut app, "the view shows the batch that went in", |a| a.tabs.iter().any(|t| matches!(&t.kind, TabKind::Collection(c) if c.view.shown().is_some_and(|d| d.len() == 50))));
}

/// The start-up purge needs no key: it goes by file names and sizes (and
/// the first bytes of small files), never decrypting.
#[test]
fn the_startup_purge_needs_no_key() {
    let root = std::env::temp_dir().join(format!("rongo-test-{}", uuid::Uuid::new_v4())).join("history");
    let store = Store::new(root.clone(), HistoryKey::new([7; 32]));
    let ns = Namespace::new("d", "c");
    let mut entry = store.begin(uuid::Uuid::from_u128(1), "c", &ns, Kind::Delete, doc! {}).unwrap();
    entry.save(&[rongo_core::bson::RawDocumentBuf::try_from(&doc! { "_id": 1 }).unwrap()]).unwrap();
    entry.finish(false).unwrap();
    assert_eq!(store.list(uuid::Uuid::from_u128(1), &ns).unwrap().len(), 1);
    let keep_nothing = HistorySettings { max_mb: 0, ..HistorySettings::default() };
    purge_history_at_start(root.clone(), &keep_nothing, Arc::new(|| {})).expect("started").join().unwrap().expect("purged");
    assert!(store.list(uuid::Uuid::from_u128(1), &ns).unwrap().is_empty(), "purged without the key");
    let _ = std::fs::remove_dir_all(root.parent().unwrap());
}

/// The Update Documents dialog and saving in the document editor keep
/// the documents they change.
#[test]
fn update_dialog_and_editor_are_recorded() {
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, cid, "hist_update", 3);
    let store = app.history_store().unwrap();

    let field = Some(("n".to_string(), Bson::Int32(100)));
    app.view_action(1, ViewAction::Bulk { conn: cid, ns: ns.clone(), delete: false, selected: vec![Bson::Int32(1), Bson::Int32(2)], query: doc! {}, field });
    let cmd = app.tool.as_mut().expect("the Update dialog").build().unwrap();
    assert!(matches!(cmd, Command::Update { .. }));
    app.tool_send(cmd);
    step_until(&ctx, &mut app, "the update is done", |a| a.tool.is_none());
    let entries = store.list(cid.0, &ns).unwrap();
    assert_eq!(entries.iter().map(|e| (e.kind, e.count)).collect::<Vec<_>>(), [(Kind::Update, 2)]);

    app.doc_dialog = Some(DocDialog::edit(cid, ns.clone(), &doc! { "_id": 3, "n": 3 }, None));
    app.save_doc(doc! { "_id": 3, "n": 300 });
    assert!(app.doc_dialog.as_ref().is_some_and(|d| d.saving.is_some()), "the editor waits");
    step_until(&ctx, &mut app, "the document is saved", |a| a.doc_dialog.is_none());
    let entries = store.list(cid.0, &ns).unwrap();
    assert_eq!(entries.iter().map(|e| (e.kind, e.count)).collect::<Vec<_>>(), [(Kind::Update, 1), (Kind::Update, 2)], "the replace is kept too");
}

/// An Update dialog whose history would not fit: the question opens over
/// the dialog (no longer busy); the typed name sends the plain update,
/// and the dialog closes with its success toast.
#[test]
fn too_large_from_the_update_dialog_asks_then_finishes() {
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, cid, "hist_big_update", 2);
    app.settings.history.max_mb = 0;
    let field = Some(("n".to_string(), Bson::Int32(7)));
    app.view_action(1, ViewAction::Bulk { conn: cid, ns: ns.clone(), delete: false, selected: vec![Bson::Int32(1), Bson::Int32(2)], query: doc! {}, field });
    let cmd = app.tool.as_mut().unwrap().build().unwrap();
    app.tool_send(cmd);
    step_until(&ctx, &mut app, "the size check answers", |a| a.confirm.is_some());
    assert!(app.tool.as_ref().is_some_and(|t| t.running.is_none()), "the dialog stays, ready again");
    let mut c = app.confirm.take().unwrap();
    assert_eq!(c.title, WITHOUT_HISTORY);
    c.input = Some(ns.coll.clone());
    app.confirmed(c);
    assert!(app.tool.as_ref().is_some_and(|t| t.running.is_some()), "the dialog waits for the plain update");
    step_until(&ctx, &mut app, "the update is done", |a| a.tool.is_none());
    assert!(toast_texts(&app).iter().any(|t| t == "Update finished: Updated 2 of 2 matched"), "{:?}", toast_texts(&app));
    assert!(app.history_store().unwrap().list(cid.0, &ns).unwrap().is_empty());
}

/// The typed name ignores spaces around it on both sides, so a
/// collection named with a trailing space can be confirmed.
#[test]
fn typed_names_ignore_spaces_around_them() {
    let (_ctx, mut app, cid) = connected_mock_app();
    let ns = Namespace::new("demo-games", "spaced ");
    app.ask_without_history(Origin::Notify { view: None, tree: None }, cid, Command::Delete { ns: ns.clone(), ids: vec![Bson::Int32(1)] }, ns.coll.clone(), "why".into());
    let c = app.confirm.as_mut().unwrap();
    c.input = Some("spaced".into());
    assert!(c.ok_enabled());
    c.input = Some(" spaced ".into());
    assert!(c.ok_enabled());
    let before = app.requests.pending.len();
    let c = app.confirm.take().unwrap();
    app.confirmed(c);
    assert_eq!(app.requests.pending.len(), before + 1, "sent: {:?}", toast_texts(&app));
}
