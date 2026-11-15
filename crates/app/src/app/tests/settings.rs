//! Settings, and its Collection History section.

use super::*;

/// A settings.json that can't be read is said in a toast as rongo starts
/// (and kept aside); the defaults are used.
#[test]
fn an_unreadable_settings_file_is_said_at_start() {
    let temp = crate::test_support::TempDir::new();
    std::fs::write(temp.0.join("settings.json"), r#"{"page_size": 75,}"#).unwrap();
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let app = RongoApp::new_in(&cc, runtime(false), false, temp);
    let toasts = toast_texts(&app);
    assert!(toasts.iter().any(|t| t.contains("settings.json couldn't be read") && t.contains("settings.json.unreadable")), "{toasts:?}");
    assert!(app.toasts.iter().all(|t| t.error));
    assert_eq!(app.settings, Settings::default());
    assert!(app._temp.0.join("settings.json.unreadable").exists());
}

/// The history as Settings last measured it.
fn measured(app: &RongoApp) -> crate::settings::Scan {
    app.history_files.scan().expect("measured").clone().expect("measured without an error")
}

/// Settings → Collection History shows the space the history takes and
/// every collection with one (connection, database, collection, entries
/// and size). "Clear All History…" asks first, then removes it all and
/// measures again; while a delete is still being recorded it waits,
/// also when that began after the question.
#[test]
fn settings_shows_usage_and_clears_all() {
    let (ctx, mut app, cid) = connected_mock_app();
    let a = history_coll(&ctx, &mut app, cid, "hist_set_a", 3);
    let b = history_coll(&ctx, &mut app, cid, "hist_set_b", 2);
    delete_ids(&ctx, &mut app, cid, &a, &[1]);
    delete_ids(&ctx, &mut app, cid, &a, &[2]);
    delete_ids(&ctx, &mut app, cid, &b, &[1]);
    let store = app.history_store().unwrap();
    let used = store.usage().unwrap();
    assert!(used > 0);

    open_settings(&ctx, &mut app);
    let scan = measured(&app);
    assert_eq!(scan.bytes(), used);
    let rows: Vec<_> = scan.colls.iter().map(|(p, ns, entries, _)| (*p, ns.coll.as_str(), *entries)).collect();
    assert_eq!(rows, [(cid.0, "hist_set_a", 2), (cid.0, "hist_set_b", 1)]);
    let texts = drawn_texts(&ctx, &mut app);
    let name = app.conns[&cid].profile.name.clone();
    let using = format!("Using {}", crate::settings::mb(used));
    let size = crate::settings::mb(scan.colls[0].3);
    for want in [using.as_str(), name.as_str(), "hist_set_a", "hist_set_b", size.as_str(), "Open", "Delete…", "Clear All History…"] {
        assert!(texts.iter().any(|t| t == want), "{want}: {texts:?}");
    }

    // A delete still being recorded: not even asked.
    writing(&mut app, cid, &b);
    app.settings_action(SettingsAction::ClearHistory);
    assert!(app.confirm.is_none(), "not asked while recording");
    written(&mut app);

    click_text(&ctx, &mut app, "Clear All History…", |t| t == "Clear All History…");
    let ask = app.confirm.take().expect("asks first");
    assert_eq!(ask.title, "Clear All History");
    assert!(ask.message.contains(&crate::settings::mb(used)), "{}", ask.message);
    // A delete began since the question: nothing is removed.
    writing(&mut app, cid, &b);
    app.confirmed(ask);
    step_until(&ctx, &mut app, "settled", |a| a.history_files.idle());
    assert_eq!(store.usage().unwrap(), used, "kept while recording");
    written(&mut app);

    app.settings_action(SettingsAction::ClearHistory);
    let ask = app.confirm.take().expect("asks first");
    app.confirmed(ask);
    step_until(&ctx, &mut app, "cleared and measured again", |a| a.history_files.idle() && measured(a).bytes() == 0);
    assert_eq!(store.usage().unwrap(), 0);
    assert!(app.settings_dialog.is_some(), "Settings stays open");
    let texts = drawn_texts(&ctx, &mut app);
    assert!(texts.iter().any(|t| t == "Using 0 MB"), "{texts:?}");
    assert!(!texts.iter().any(|t| t == "hist_set_a"), "{texts:?}");
}

/// One collection's history: Delete… asks first and removes only that
/// one; it waits while a delete on that collection is being recorded.
#[test]
fn settings_deletes_one_collections_history() {
    let (ctx, mut app, cid) = connected_mock_app();
    let a = history_coll(&ctx, &mut app, cid, "hist_del_a", 2);
    let b = history_coll(&ctx, &mut app, cid, "hist_del_b", 2);
    delete_ids(&ctx, &mut app, cid, &a, &[1]);
    delete_ids(&ctx, &mut app, cid, &b, &[1]);
    let store = app.history_store().unwrap();
    open_settings(&ctx, &mut app);

    writing(&mut app, cid, &a);
    app.settings_action(SettingsAction::DeleteHistory(cid.0, a.clone()));
    assert!(app.confirm.is_none(), "not asked while recording");
    written(&mut app);

    app.settings_action(SettingsAction::DeleteHistory(cid.0, a.clone()));
    let ask = app.confirm.take().expect("asks first");
    assert_eq!(ask.title, "Delete History");
    assert!(ask.message.contains("demo-games.hist_del_a"), "{}", ask.message);
    app.confirmed(ask);
    step_until(&ctx, &mut app, "deleted and measured again", |a| a.history_files.idle() && measured(a).colls.len() == 1);
    assert!(store.list(cid.0, &a).unwrap().is_empty());
    assert_eq!(store.list(cid.0, &b).unwrap().len(), 1, "the other collection's history stays");
}

/// A dropped collection keeps its history: Settings still lists it, and
/// Open (which closes Settings) shows its entries in the History tab,
/// where restoring the delete recreates the collection.
#[test]
fn all_histories_open_even_after_a_drop() {
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, cid, "hist_gone", 3);
    delete_ids(&ctx, &mut app, cid, &ns, &[1, 2]);
    app.tree_action(TreeAction::DropCollection(cid, ns.clone()));
    let c = app.confirm.take().expect("dropping asks first");
    app.confirmed(c);
    step_until(&ctx, &mut app, "dropped", writes_done);
    assert_eq!(count_docs(&app, cid, &ns), 0);

    open_settings(&ctx, &mut app);
    assert!(drawn_texts(&ctx, &mut app).iter().any(|t| t == "hist_gone"), "still listed");
    click_text(&ctx, &mut app, "Open", |t| t == "Open");
    assert!(app.settings_dialog.is_none(), "Settings closes");
    step_until(&ctx, &mut app, "the entries are listed and the newest previewed", |a| matches!(&a.tabs[a.active_tab].kind, TabKind::History(h) if h.previewed().is_some()));
    let h = history_tab(&app);
    assert_eq!((h.conn, &h.ns), (cid, &ns));
    assert_eq!(h.entries().iter().map(|e| (e.kind, e.count)).collect::<Vec<_>>(), [(Kind::Delete, 2)]);

    let tab = app.tabs[app.active_tab].id;
    let entry = h.selected().cloned().unwrap();
    app.history_action(tab, crate::history_tab::HistoryAction::Restore(entry));
    step_until(&ctx, &mut app, "the restore asks", |a| a.restore_ask.is_some());
    app.restore_answer(true);
    step_until(&ctx, &mut app, "the restore is done", |a| history_idle(a) && a.toasts.iter().any(|t| t.text.starts_with("Restored")));
    assert_eq!(count_docs(&app, cid, &ns), 2, "recreated with what was deleted");
}

/// Renaming a collection takes its history along once the server has
/// renamed it, and a History tab open on it follows; a rename the
/// server refuses moves nothing. While a delete on the collection is
/// still being recorded, the rename waits.
#[test]
fn renaming_a_collection_moves_its_history() {
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, cid, "hist_ren", 3);
    let taken = history_coll(&ctx, &mut app, cid, "hist_taken", 1);
    delete_ids(&ctx, &mut app, cid, &ns, &[1]);
    open_history(&ctx, &mut app, cid, &ns);
    let store = app.history_store().unwrap();
    let rename = |ctx: &egui::Context, app: &mut RongoApp, from: &Namespace, to: &str| {
        app.tree_action(TreeAction::Rename(cid, from.clone()));
        let mut c = app.confirm.take().expect("renaming asks for the name");
        c.input = Some(to.into());
        app.confirmed(c);
        assert!(app.requests.pending.values().any(|s| matches!(s.origin, Origin::Rename { .. })), "sent");
        step_until(ctx, app, "the rename answers", |a| !a.requests.pending.values().any(|s| matches!(s.origin, Origin::Rename { .. })));
    };

    app.toasts.clear();
    rename(&ctx, &mut app, &ns, "hist_taken");
    assert!(app.toasts.iter().any(|t| t.error), "refused: {:?}", toast_texts(&app));
    assert_eq!(store.list(cid.0, &ns).unwrap().len(), 1, "nothing moved");
    assert!(store.list(cid.0, &taken).unwrap().is_empty());

    let to = Namespace::new("demo-games", "hist_ren2");
    app.toasts.clear();
    rename(&ctx, &mut app, &ns, "hist_ren2");
    assert_eq!(toast_texts(&app), ["Renamed demo-games.hist_ren to hist_ren2"]);
    // Moved off the UI thread.
    assert_eq!(app.history_files.doing(), Some("Moving a renamed collection's history…"));
    step_until(&ctx, &mut app, "the history moved", |a| a.history_files.idle());
    assert_eq!(toast_texts(&app), ["Renamed demo-games.hist_ren to hist_ren2"], "and nothing went wrong");
    assert!(store.list(cid.0, &ns).unwrap().is_empty(), "moved away");
    assert_eq!(store.list(cid.0, &to).unwrap().iter().map(|e| (e.kind, e.count)).collect::<Vec<_>>(), [(Kind::Delete, 1)]);
    assert_eq!(history_tab(&app).ns, to, "the open tab follows");
    step_until(&ctx, &mut app, "the tab lists it again", |a| history_tab(a).listed() && history_tab(a).entries().len() == 1);

    // A delete on it still being recorded: the rename waits, not even asking.
    app.toasts.clear();
    writing(&mut app, cid, &to);
    app.tree_action(TreeAction::Rename(cid, to.clone()));
    assert!(app.confirm.is_none(), "not asked");
    assert!(app.toasts.iter().any(|t| t.error), "says why");
    written(&mut app);
}

/// Deleting a saved connection asks first and offers "Also delete its
/// Collection History (N MB)", ticked. Ticked, the history goes with the
/// profile; unticked, it stays, listed in Settings under "deleted
/// profile", and still opens there (restoring waits for a connection).
/// A profile without history isn't asked about it.
#[test]
fn deleting_a_profile_offers_to_delete_its_history() {
    let (ctx, mut app, _) = connected_mock_app();
    let [gone, kept, plain] = ["Gone with it", "Kept", "No history"].map(ConnectionProfile::new);
    for p in [&gone, &kept, &plain] {
        app.store.as_mut().unwrap().upsert(p.clone(), None, None, app.vault.as_deref().unwrap()).unwrap();
    }
    let ns = Namespace::new("shop", "orders");
    saved_history(&app, gone.id, &ns, 1);
    saved_history(&app, kept.id, &ns, 2);
    let store = app.history_store().unwrap();
    let checkbox = |app: &RongoApp| app.confirm.as_ref().and_then(|c| c.checkbox.clone());
    let ask = |ctx: &egui::Context, app: &mut RongoApp, id| {
        app.connect_action(ConnectAction::Delete(id));
        assert_eq!(app.confirm.as_ref().map(|c| c.title.as_str()), Some("Delete Connection"));
        step_until(ctx, app, "its history is measured", |a| a.history_files.idle() && checkbox(a).is_none_or(|(l, _)| l.ends_with("MB)")));
    };

    let bytes = store.usage_by_profile().unwrap()[&gone.id];
    ask(&ctx, &mut app, gone.id);
    assert_eq!(checkbox(&app), Some((format!("Also delete its Collection History ({})", crate::settings::mb(bytes)), true)), "ticked by default");
    let c = app.confirm.take().unwrap();
    app.confirmed(c);
    assert!(app.store.as_ref().unwrap().get(gone.id).is_none(), "the profile is deleted");
    step_until(&ctx, &mut app, "its history is deleted", |a| a.history_files.idle());
    assert!(store.list(gone.id, &ns).unwrap().is_empty());

    ask(&ctx, &mut app, kept.id);
    let mut c = app.confirm.take().unwrap();
    c.checkbox.as_mut().expect("offered").1 = false;
    app.confirmed(c);
    assert!(app.store.as_ref().unwrap().get(kept.id).is_none(), "the profile is deleted");
    step_until(&ctx, &mut app, "settled", |a| a.history_files.idle());
    assert_eq!(store.list(kept.id, &ns).unwrap().len(), 1, "unticked: kept");

    ask(&ctx, &mut app, plain.id);
    assert_eq!(checkbox(&app), None, "no history, nothing to offer");
    app.confirm = None;

    open_settings(&ctx, &mut app);
    let texts = drawn_texts(&ctx, &mut app);
    assert!(texts.iter().any(|t| t == "deleted profile"), "{texts:?}");
    assert_eq!(measured(&app).colls.iter().map(|c| c.0).collect::<Vec<_>>(), [kept.id], "only the kept history");
    click_text(&ctx, &mut app, "Open", |t| t == "Open");
    step_until(&ctx, &mut app, "the entries are listed", |a| matches!(&a.tabs[a.active_tab].kind, TabKind::History(h) if h.listed()));
    let h = history_tab(&app);
    assert_eq!((h.conn, h.entries().len()), (ConnId(kept.id), 1));
    assert_eq!(h.restore_blocked(app.conns.get(&h.conn)), Some(crate::history_tab::NOT_CONNECTED), "restoring waits for a connection");
}

/// New retention limits apply at once: saving Settings with a smaller
/// size trims the history off the UI thread, though not while a delete
/// is still being recorded (then it trims once that is done).
#[test]
fn new_limits_trim_the_history_at_once() {
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, cid, "hist_trim", 2);
    delete_ids(&ctx, &mut app, cid, &ns, &[1]);
    let store = app.history_store().unwrap();
    assert!(store.usage().unwrap() > 0);

    writing(&mut app, cid, &ns);
    let smaller = Settings { history: HistorySettings { max_mb: 0, ..app.settings.history.clone() }, ..app.settings.clone() };
    app.settings_action(SettingsAction::Save(smaller));
    let ask = app.confirm.take().expect("a lower limit asks first");
    app.confirmed(ask);
    for _ in 0..5 {
        drawn_texts(&ctx, &mut app);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(store.usage().unwrap() > 0, "nothing trimmed while recording");
    assert_eq!(app.history_files.doing(), Some("Trimming to the new limits…"), "it waits");
    written(&mut app);
    step_until(&ctx, &mut app, "trimmed", |a| a.history_files.idle() && store.usage().unwrap() == 0);
}

/// Lower limits remove history as soon as they are saved, so saving
/// them asks first, saying roughly what goes; Cancel leaves Settings
/// open with nothing saved or removed. Limits that remove nothing are
/// saved without a question.
#[test]
fn lower_limits_ask_before_removing_history() {
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, cid, "hist_lower", 2);
    delete_ids(&ctx, &mut app, cid, &ns, &[1]);
    let store = app.history_store().unwrap();
    let used = store.usage().unwrap();
    open_settings(&ctx, &mut app);
    let with = |app: &RongoApp, keep_days: u32, max_mb: u64| Settings { history: HistorySettings { keep_days, max_mb, ..app.settings.history.clone() }, ..app.settings.clone() };
    let fake_use = |app: &mut RongoApp, mb: u64| app.history_files.set_scan(crate::settings::Scan { by_profile: [(cid.0, mb << 20)].into(), colls: Vec::new() });

    // Higher limits: saved, nothing asked.
    let higher = with(&app, 60, 2048);
    app.settings_action(SettingsAction::Save(higher.clone()));
    assert!(app.confirm.is_none());
    assert_eq!(app.settings, higher);

    // Fewer days: asks, naming them; Cancel keeps everything.
    open_settings(&ctx, &mut app);
    let fewer = with(&app, 7, 2048);
    app.settings_action(SettingsAction::Save(fewer.clone()));
    let ask = app.confirm.take().expect("asks first");
    assert!(ask.danger && ask.message.contains("entries older than 7 days"), "{}", ask.message);
    assert_eq!(app.settings, higher, "nothing saved yet");
    assert!(app.settings_dialog.is_some(), "Settings stays open");
    assert!(app.history_files.idle(), "nothing queued");
    // A smaller size the history fits in: nothing goes, nothing asked.
    fake_use(&mut app, 300);
    app.settings_action(SettingsAction::Save(with(&app, 60, 500)));
    assert!(app.confirm.is_none());
    assert_eq!(app.settings.history.max_mb, 500);

    // A size the history doesn't fit in: asks, saying about how much goes; then trims.
    open_settings(&ctx, &mut app);
    fake_use(&mut app, 300);
    app.settings_action(SettingsAction::Save(with(&app, 60, 100)));
    let ask = app.confirm.take().expect("asks first");
    assert!(ask.message.contains("about 200 MB of the oldest entries"), "{}", ask.message);
    assert!(ask.message.starts_with("Keep Collection History for 60 days and at most 100 MB?") && !ask.message.contains("older than"), "{}", ask.message);
    app.confirmed(ask);
    assert_eq!(app.settings.history.max_mb, 100);
    assert!(app.settings_dialog.is_none(), "saved: Settings closes");
    assert_eq!(app.history_files.doing(), Some("Trimming to the new limits…"));
    step_until(&ctx, &mut app, "trimmed", |a| a.history_files.idle());
    assert_eq!(store.usage().unwrap(), used, "under 100 MB: nothing removed");
}

/// Tests never read or write the user's real settings, connections or
/// history: each test app keeps them in a temporary folder of its own,
/// and saving Settings writes there.
#[test]
fn test_apps_never_use_the_real_app_support_folder() {
    let (_ctx, mut app, _) = connected_mock_app();
    let real = directories::ProjectDirs::from("dev", "rongo", "rongo").expect("an app-support folder");
    let path = app.settings_path.clone().expect("a settings file");
    let dir = path.parent().unwrap().to_path_buf();
    let root = app.history_root.clone().expect("a history folder");
    for p in [&path, &root] {
        assert!(p.starts_with(std::env::temp_dir()), "{p:?}");
        // The history folder is under the local data folder (a different one from the config and data folders on Windows).
        assert!(!p.starts_with(real.config_dir()) && !p.starts_with(real.data_dir()) && !p.starts_with(real.data_local_dir()), "{p:?}");
    }
    assert_eq!(root.parent(), Some(dir.as_path()));
    assert!(dir.join("profiles.json").is_file(), "the saved demo profile went there too");
    let s = Settings { open_collection_in: OpenIn::CollectionTab, ..app.settings.clone() };
    app.settings_action(SettingsAction::Save(s.clone()));
    assert_eq!(Settings::load(&path), (s, None), "saved in the test's own folder");
}

/// A change Settings queued starts only while nothing is being written
/// where it removes entries, checked when it starts: a delete that began
/// after the question was confirmed makes Clear All wait, not race it.
/// Rename waits for the change too.
#[test]
fn queued_changes_wait_for_writes_that_begin_meanwhile() {
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, cid, "hist_queue", 2);
    let other = history_coll(&ctx, &mut app, cid, "hist_queue_other", 1);
    delete_ids(&ctx, &mut app, cid, &ns, &[1]);
    let store = app.history_store().unwrap();
    let used = store.usage().unwrap();
    open_settings(&ctx, &mut app);

    app.settings_action(SettingsAction::ClearHistory);
    let ask = app.confirm.take().expect("asks first");
    app.confirmed(ask);
    writing(&mut app, cid, &ns);
    for _ in 0..5 {
        drawn_texts(&ctx, &mut app);
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(store.usage().unwrap(), used, "not started while recording");
    assert_eq!(app.history_files.doing(), Some("Clearing…"), "it waits");

    app.toasts.clear();
    app.tree_action(TreeAction::Rename(cid, other.clone()));
    assert!(app.confirm.is_none(), "Rename waits for the change to the history files");
    assert!(toast_texts(&app).iter().any(|t| t.contains("Clearing")), "{:?}", toast_texts(&app));

    written(&mut app);
    step_until(&ctx, &mut app, "cleared", |a| a.history_files.idle());
    assert_eq!(store.usage().unwrap(), 0);
}

/// Settings' connection names are kept between frames, and made again
/// when a saved connection is added or renamed.
#[test]
fn profile_names_follow_the_saved_connections() {
    let (_ctx, mut app, cid) = connected_mock_app();
    assert!(app.profile_names().contains_key(&cid.0), "the open demo connection");
    let mut p = ConnectionProfile::new("Brand new");
    assert!(!app.profile_names().contains_key(&p.id));
    app.store.as_mut().unwrap().upsert(p.clone(), None, None, app.vault.as_deref().unwrap()).unwrap();
    assert_eq!(app.profile_names().get(&p.id).map(String::as_str), Some("Brand new"));
    p.name = "Renamed".into();
    app.store.as_mut().unwrap().upsert(p.clone(), None, None, app.vault.as_deref().unwrap()).unwrap();
    assert_eq!(app.profile_names().get(&p.id).map(String::as_str), Some("Renamed"));
}

/// A move that failed may have stopped part way: the toast says the
/// entries may be under either name, not that they all stayed.
#[test]
fn a_failed_move_says_where_the_history_may_be() {
    let (from, to) = (Namespace::new("d", "a"), Namespace::new("d", "b"));
    let text = move_failed(&from, &to, CoreError::History("permission denied".into()));
    assert!(text.starts_with("d.a's entries could not all be moved to b"), "{text}");
    assert!(text.contains("permission denied") && text.contains("some entries may be under each name") && text.contains("lists both"), "{text}");
    assert!(!text.contains("stayed"), "{text}");
}
