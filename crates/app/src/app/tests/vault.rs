//! The vault: the master password, Reset, and saved connections under it.

use super::*;

/// Until the vault is unlocked the window draws nothing but the launch
/// dialog: no toolbar, tree or tabs. Unlocked, the app is there; Quit
/// closes the window.
#[test]
fn the_app_draws_nothing_but_the_dialog_until_unlocked() {
    use crate::test_support::TEST_PASSWORD;
    use egui::accesskit::Role;
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;
    let mut h = app_harness();
    h.run_steps(2);
    assert!(texts_of(h.output()).iter().any(|t| t.contains("Connect")), "tests start unlocked: {:?}", texts_of(h.output()));
    let path = h.state().vault.as_ref().expect("tests start unlocked").path().to_path_buf();
    let lock = |h: &mut Harness<RongoApp>| {
        h.state_mut().vault = None;
        h.state_mut().unlock = Some(UnlockDialog::new(path.clone(), false, Vec::new()));
        h.run_steps(3);
    };
    lock(&mut h);
    let drawn = texts_of(h.output());
    assert!(drawn.iter().any(|t| t.contains("Unlock rongo")), "{drawn:?}");
    assert!(!drawn.iter().any(|t| t.contains("Connect")), "no toolbar: {drawn:?}");
    assert!(!drawn.iter().any(|t| t.contains("A MongoDB client")), "no welcome tab: {drawn:?}");

    h.get_by_role(Role::PasswordInput).focus();
    h.run_steps(1);
    h.get_by_role(Role::PasswordInput).type_text(TEST_PASSWORD);
    h.run_steps(1);
    h.get_by_label("Unlock").click();
    let end = Instant::now() + WAIT;
    while h.state().vault.is_none() && Instant::now() < end {
        h.step();
        std::thread::sleep(Duration::from_millis(5));
    }
    h.run_steps(2);
    assert!(h.state().vault.is_some() && h.state().unlock.is_none(), "unlocked");
    assert!(texts_of(h.output()).iter().any(|t| t.contains("Connect")), "the toolbar shows");

    lock(&mut h);
    h.get_by_label("Quit").click();
    let mut closed = false;
    for _ in 0..3 {
        h.step();
        closed |= h.output().viewport_output.values().any(|v| v.commands.contains(&egui::ViewportCommand::Close));
    }
    assert!(closed, "Quit closes the window");
    assert!(h.state().vault.is_none());
}

/// Secrets are read as another rongo window left them: reading catches
/// the vault up first. A vault changed there (its password, say) drops
/// this window's copy and asks for the master password again; a connect
/// then sends nothing.
#[test]
fn reading_secrets_catches_up_with_another_window() {
    use crate::test_support::TEST_PASSWORD;
    let (ctx, mut app) = test_app(false);
    let other = Vault::unlock(app.vault.as_ref().unwrap().path().to_path_buf(), TEST_PASSWORD).unwrap();
    let mut p = ConnectionProfile::new("p");
    p.auth = rongo_core::profile::Auth::Password { user: "u".into(), auth_db: "admin".into(), mechanism: None };
    other.set(p.id, SecretKind::Password, "from the other window").unwrap();
    assert_eq!(app.vault.as_ref().unwrap().get(p.id, SecretKind::Password), None, "not seen yet");
    let v = app.fresh_vault().expect("still unlocked");
    assert_eq!(v.get(p.id, SecretKind::Password).as_deref(), Some("from the other window"));

    other.change_password(TEST_PASSWORD, "another password").unwrap();
    app.connect(p.clone());
    assert!(!app.conns.contains_key(&p.conn_id()) && app.requests.pending.is_empty(), "nothing sent");
    assert!(app.vault.is_none() && app.unlock.is_some(), "asks for the master password again");
    let texts = drawn_texts(&ctx, &mut app);
    assert!(texts.iter().any(|t| t.contains("changed in another rongo window")), "{texts:?}");
    assert!(!texts.iter().any(|t| t.contains("Connect")), "{texts:?}");
}

/// Saving a connection after another window changed the master password:
/// nothing is written under the old key. The vault is dropped, the
/// unlock dialog asks again, the form stays open with what was typed and
/// says why, and after unlocking with the new password the vault lacks
/// the secret that was not saved.
#[test]
fn a_vault_changed_elsewhere_asks_to_unlock_again() {
    use crate::test_support::TEST_PASSWORD;
    let (ctx, mut app, other) = two_windows();
    let path = app.vault.as_ref().unwrap().path().to_path_buf();
    app.connect_dialog = Some(crate::dialogs::ConnectDialog::new_connection());
    other.change_password(TEST_PASSWORD, "another password").unwrap();

    let mut p = ConnectionProfile::new("p");
    p.auth = rongo_core::profile::Auth::Password { user: "u".into(), auth_db: "admin".into(), mechanism: None };
    let id = p.id;
    app.connect_action(ConnectAction::Save { profile: p, password: Some("typed secret".into()), ssh_secret: None, then_connect: false });

    assert!(app.vault.is_none() && app.unlock.is_some(), "asks for the master password again");
    let d = app.connect_dialog.as_ref().expect("the form stays open");
    assert!(d.has_form());
    assert_eq!(d.form_error(), Some("The vault was changed in another rongo window: unlock again, then Save"));
    assert!(toast_texts(&app).iter().any(|t| t.contains("changed in another rongo window")), "{:?}", toast_texts(&app));
    assert!(app.store.as_ref().unwrap().get(id).is_none(), "not saved");
    let texts = drawn_texts(&ctx, &mut app);
    assert!(texts.iter().any(|t| t.contains("changed in another rongo window")), "{texts:?}");

    let after = Vault::unlock(path, "another password").expect("the new password unlocks");
    assert_eq!(after.get(id, SecretKind::Password), None, "nothing written under the old key");
    assert!(Vault::unlock(after.path().to_path_buf(), TEST_PASSWORD).is_err(), "the old key is not back");
}

/// Deleting a connection after the vault changed elsewhere says so in a
/// toast, locks again, and keeps the connection.
#[test]
fn a_delete_after_the_vault_changed_asks_to_unlock_again() {
    use crate::test_support::TEST_PASSWORD;
    let (_ctx, mut app, other) = two_windows();
    let p = ConnectionProfile::new("p");
    app.store.as_mut().unwrap().upsert(p.clone(), None, None, app.vault.as_deref().unwrap()).unwrap();
    other.change_password(TEST_PASSWORD, "another password").unwrap();
    app.delete_profile(p.id, false);
    assert!(app.vault.is_none() && app.unlock.is_some());
    assert!(toast_texts(&app).iter().any(|t| t.contains("changed in another rongo window")), "{:?}", toast_texts(&app));
    assert!(app.store.as_ref().unwrap().get(p.id).is_some(), "still listed");
}

/// A Save that fails for any other reason (profiles.json unreadable, the
/// vault busy in another window) also gives the form back as typed, with
/// the error on it, and saves nothing.
#[test]
fn any_failed_save_gives_the_form_back() {
    let (_ctx, mut app, _other) = two_windows();
    let path = app._temp.0.join("profiles.json");
    let mut p = ConnectionProfile::new("p");
    p.auth = rongo_core::profile::Auth::Password { user: "u".into(), auth_db: "admin".into(), mechanism: None };
    let save = |app: &mut RongoApp| {
        app.connect_dialog = Some(crate::dialogs::ConnectDialog::new_connection());
        // The form is closed by Save and kept until the app has answered.
        app.connect_dialog.as_mut().unwrap().close_form_for_save();
        app.connect_action(ConnectAction::Save { profile: p.clone(), password: Some("typed secret".into()), ssh_secret: None, then_connect: false });
    };

    std::fs::write(&path, b"{ damaged").unwrap();
    save(&mut app);
    let d = app.connect_dialog.as_ref().expect("the manager stays");
    assert!(d.has_form(), "the form is back");
    assert!(d.form_error().is_some_and(|e| e.contains("profiles.json")), "{:?}", d.form_error());
    assert_eq!(app.vault.as_ref().unwrap().get(p.id, SecretKind::Password), None, "nothing saved");
    assert!(app.vault.is_some(), "not locked");

    std::fs::remove_file(&path).unwrap();
    let held = std::fs::File::options().read(true).write(true).open(app._temp.0.join("vault.lock")).unwrap();
    held.lock().unwrap();
    save(&mut app);
    drop(held);
    let d = app.connect_dialog.as_ref().unwrap();
    assert!(d.has_form() && d.form_error().is_some_and(|e| e.contains("busy")), "{:?}", d.form_error());
    assert!(app.store.as_ref().unwrap().get(p.id).is_none() && !path.exists(), "nothing saved");
}

/// Saving a connection with password authentication turned off deletes
/// its saved password (in the same vault write as the rest), so turning
/// it on again later doesn't connect with the forgotten one.
#[test]
fn turning_authentication_off_deletes_the_saved_password() {
    let (_ctx, mut app, other) = two_windows();
    let p = saved_with_password(&mut app, "p");
    app.connect_dialog = Some(ConnectDialog::new());
    let off = ConnectionProfile { auth: rongo_core::profile::Auth::None, ..p.clone() };
    app.connect_action(ConnectAction::Save { profile: off, password: None, ssh_secret: None, then_connect: false });
    assert!(toast_texts(&app).iter().any(|t| t == "Connection saved"), "{:?}", toast_texts(&app));
    assert_eq!(app.vault.as_ref().unwrap().get(p.id, SecretKind::Password), None);
    other.reload_if_changed().unwrap();
    assert_eq!(other.get(p.id, SecretKind::Password), None, "gone from the file too");
    app.connect_action(ConnectAction::Save { profile: p.clone(), password: None, ssh_secret: None, then_connect: false });
    assert_eq!(app.vault.as_ref().unwrap().get(p.id, SecretKind::Password), None, "on again: nothing comes back");
}

/// Colouring or locking a connection edits it as it is on disk now, so
/// another window's edits to it stay; this window's Save never lifts a
/// lock saved there (it asks Allow Writes first); and a connection deleted
/// in another window is not brought back by recolouring or locking it:
/// a toast says so, it leaves the list, and its open connection stays.
#[test]
fn colour_and_lock_edit_the_connection_as_it_is_on_disk() {
    let (_ctx, mut app, other_vault) = two_windows();
    let path = app._temp.0.join("profiles.json");
    let p = ConnectionProfile::new("p");
    let cid = p.conn_id();
    app.store.as_mut().unwrap().upsert(p.clone(), None, None, app.vault.as_deref().unwrap()).unwrap();
    app.conns.insert(cid, crate::sidebar::OpenConn::new(p.clone()));
    let mut other = ProfileStore::load(path.clone()).unwrap();
    let on_disk = || ProfileStore::load(path.clone()).unwrap().get(p.id).cloned();

    other.upsert(ConnectionProfile { name: "renamed elsewhere".into(), ..p.clone() }, None, None, &other_vault).unwrap();
    app.tree_action(TreeAction::SetColor(cid, Some("red".into())));
    let q = on_disk().unwrap();
    assert_eq!((q.name.as_str(), q.color.as_deref()), ("renamed elsewhere", Some("red")), "the other window's name stays");

    other.update(p.id, |q| q.read_only = true).unwrap();
    app.connect_dialog = Some(ConnectDialog::new());
    app.connect_action(ConnectAction::Save { profile: ConnectionProfile { name: "saved here".into(), ..p.clone() }, password: None, ssh_secret: None, then_connect: false });
    let q = on_disk().unwrap();
    assert_eq!((q.name.as_str(), q.read_only), ("saved here", true), "saved, the lock kept");
    assert!(matches!(app.confirm.as_ref().map(|c| &c.pending), Some(Pending::AllowWrites { .. })), "Allow Writes is asked first");
    assert!(app.conns[&cid].profile.read_only, "the open connection is locked too");
    app.confirm = None;

    other.remove(p.id, &other_vault).unwrap();
    app.toasts.clear();
    app.set_read_only(cid, true);
    assert!(on_disk().is_none(), "not brought back");
    assert!(app.store.as_ref().unwrap().get(p.id).is_none(), "gone from this window's list");
    assert!(app.conns.contains_key(&cid), "its open connection stays");
    assert!(toast_texts(&app).iter().any(|t| t.contains("deleted in another rongo window")), "{:?}", toast_texts(&app));
    // No longer listed here: recolouring it is the open connection's alone, as for the demo.
    app.toasts.clear();
    app.tree_action(TreeAction::SetColor(cid, Some("blue".into())));
    assert!(on_disk().is_none(), "nor by recolouring it");
    assert_eq!(app.conns[&cid].profile.color.as_deref(), Some("blue"));
    assert!(app.toasts.is_empty(), "{:?}", toast_texts(&app));
}

/// With profiles.json unreadable at launch (no saved connections), Save
/// keeps the form, with why on it; opening the Connection Manager reads
/// the file again, so one fixed meanwhile is picked up and Save works.
#[test]
fn saving_without_the_saved_connections_keeps_the_form() {
    let (_ctx, mut app, _other) = two_windows();
    let path = app._temp.0.join("profiles.json");
    std::fs::write(&path, b"{ damaged").unwrap();
    app.store = None;
    let p = ConnectionProfile::new("p");
    let save = |app: &mut RongoApp| {
        app.connect_dialog.as_mut().unwrap().close_form_for_save();
        app.connect_action(ConnectAction::Save { profile: p.clone(), password: None, ssh_secret: None, then_connect: false });
    };
    app.open_connection_manager(true);
    assert!(app.store.is_none(), "still unreadable");
    save(&mut app);
    let d = app.connect_dialog.as_ref().expect("the manager stays");
    assert!(d.has_form(), "the form is back");
    assert!(d.form_error().is_some_and(|e| e.contains("profiles.json")), "{:?}", d.form_error());

    std::fs::write(&path, br#"{"profiles":[]}"#).unwrap();
    app.connect_dialog = None;
    app.open_connection_manager(true);
    assert!(app.store.is_some(), "read again");
    save(&mut app);
    assert!(ProfileStore::load(path).unwrap().get(p.id).is_some(), "saved");
}

/// Connections saved in another rongo window show up when the
/// Connection Manager opens (its list is read again), and saving here
/// keeps them; a file that can't be read again is said, the list kept.
#[test]
fn the_manager_shows_connections_saved_in_another_window() {
    let (_ctx, mut app, other_vault) = two_windows();
    let path = app._temp.0.join("profiles.json");
    let mut other = ProfileStore::load(path.clone()).unwrap();
    let theirs = ConnectionProfile::new("saved in the other window");
    other.upsert(theirs.clone(), None, None, &other_vault).unwrap();
    assert!(app.store.as_ref().unwrap().get(theirs.id).is_none(), "not seen yet");
    app.open_connection_manager(false);
    assert!(app.connect_dialog.is_some());
    assert!(app.store.as_ref().unwrap().get(theirs.id).is_some(), "listed");
    let ours = ConnectionProfile::new("ours");
    app.connect_action(ConnectAction::Save { profile: ours.clone(), password: None, ssh_secret: None, then_connect: false });
    let names: Vec<String> = ProfileStore::load(path.clone()).unwrap().profiles().iter().map(|p| p.name.clone()).collect();
    assert_eq!(names, ["saved in the other window", "ours"]);

    app.connect_dialog = None;
    std::fs::write(&path, b"{ damaged").unwrap();
    app.open_connection_manager(true);
    assert!(app.connect_dialog.as_ref().is_some_and(|d| d.has_form()), "New Connection opens all the same");
    assert_eq!(app.store.as_ref().unwrap().profiles().len(), 2, "the list is kept");
    assert!(toast_texts(&app).iter().any(|t| t.contains("Could not read the saved connections")), "{:?}", toast_texts(&app));
}

/// Deleting a connection whose secrets the vault then fails to delete
/// (its folder read-only here; profiles.json kept elsewhere): the
/// connection is gone, its history is deleted as asked, and a toast says
/// its passwords stay in the vault. The window stays unlocked.
#[cfg(unix)]
#[test]
fn a_deleted_connection_whose_secrets_stay_says_so() {
    use std::os::unix::fs::PermissionsExt;
    let (_ctx, mut app) = test_app(false);
    let elsewhere = crate::test_support::TempDir::new();
    app.store = Some(ProfileStore::load(elsewhere.0.join("profiles.json")).unwrap());
    let p = saved_with_password(&mut app, "p");
    let dir = app._temp.0.clone();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    app.delete_profile(p.id, true);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(app.store.as_ref().unwrap().get(p.id).is_none(), "deleted");
    assert!(app.history_files.next().is_some(), "its history is still deleted");
    let toasts = toast_texts(&app);
    assert!(toasts.iter().any(|t| t.contains("p was deleted") && t.contains("stay in the vault")), "{toasts:?}");
    assert!(app.vault.is_some() && app.unlock.is_none());
    assert_eq!(app.vault.as_ref().unwrap().get(p.id, SecretKind::Password).as_deref(), Some("p's password"));
}

/// Reading while another window holds the vault's lock is neither a
/// change nor silent: a toast says why, and this window's copy serves.
#[test]
fn a_busy_vault_is_a_toast_not_silence() {
    let (_ctx, mut app, _other) = two_windows();
    let path = app.vault.as_ref().unwrap().path().to_path_buf();
    let held = std::fs::File::options().read(true).write(true).create(true).truncate(false).open(path.with_extension("lock")).unwrap();
    held.lock().unwrap();
    let v = app.fresh_vault();
    drop(held);
    assert!(v.is_some() && app.vault.is_some() && app.unlock.is_none(), "still unlocked");
    let toasts = toast_texts(&app);
    assert!(toasts.iter().any(|t| t.contains("busy")), "{toasts:?}");
    assert!(app.toasts.iter().all(|t| t.error));
}

/// Waits until the Reset question has measured the history: without any,
/// its checkbox goes and the buttons move up, so a click before that
/// could land where they were.
fn reset_ask_measured(h: &mut egui_kittest::Harness<'_, RongoApp>) {
    harness_until(h, "the Reset question is measured", |a| a.reset_ask.as_ref().is_some_and(|c| c.checkbox.as_ref().is_none_or(|(l, _)| l != DELETE_ALL_HISTORY)));
}

/// Types `text` into the `n`th password field, as a person would.
fn type_password(h: &mut egui_kittest::Harness<'_, RongoApp>, n: usize, text: &str) {
    use egui::accesskit::Role;
    use egui_kittest::kittest::Queryable;
    h.get_all_by_role(Role::PasswordInput).nth(n).expect("a password field").focus();
    h.run_steps(1);
    h.get_all_by_role(Role::PasswordInput).nth(n).expect("a password field").type_text(text);
    h.run_steps(1);
}

/// Settings → Change Master Password…: the current password, then the
/// new one twice. A wrong current one says so and changes nothing. The
/// right one seals the same secrets and Collection History key under the
/// new password: a fresh unlock with it reads them, the old one no
/// longer opens the vault, and this window goes on reading history and
/// saving secrets.
#[test]
fn change_master_password_keeps_secrets_and_history() {
    use crate::test_support::TEST_PASSWORD;
    use egui::accesskit::Role;
    use egui_kittest::kittest::Queryable;
    let mut h = app_harness();
    let p = saved_with_password(h.state_mut(), "p");
    let ns = Namespace::new("shop", "orders");
    saved_history(h.state(), p.id, &ns, 2);
    let path = h.state().vault.as_ref().unwrap().path().to_path_buf();
    let key = h.state().vault.as_ref().unwrap().history_key();
    h.state_mut().open_settings();
    // Measured, the history list is drawn above the button: clicked once it stays put.
    harness_until(&mut h, "the history is measured", |a| a.history_files.idle() && a.history_files.scan().is_some());
    h.get_by_label("Change Master Password…").click();
    h.run_steps(3);
    assert_eq!(h.query_all_by_role(Role::PasswordInput).count(), 3, "current, new, and new again");

    type_password(&mut h, 0, "not the password");
    type_password(&mut h, 1, "a new password");
    type_password(&mut h, 2, "a new password");
    h.get_by_label("Change").click();
    // Not "not busy": that holds before the click is even taken in.
    harness_until(&mut h, "the wrong password is refused", |a| a.change_password.as_ref().is_some_and(|d| !d.busy() && d.error().is_some_and(|e| e.contains("Wrong current"))));
    assert!(texts_of(h.output()).iter().any(|t| t.contains("Wrong current master password")), "{:?}", texts_of(h.output()));
    assert!(Vault::unlock(path.clone(), TEST_PASSWORD).is_ok(), "unchanged");

    type_password(&mut h, 0, TEST_PASSWORD);
    h.get_by_label("Change").click();
    harness_until(&mut h, "the password is changed", |a| a.change_password.is_none());
    assert!(toast_texts(h.state()).iter().any(|t| t == "Master password changed"), "{:?}", toast_texts(h.state()));
    assert!(h.state().settings_dialog.is_some(), "Settings stays open");
    let fresh = Vault::unlock(path.clone(), "a new password").expect("the new password opens it");
    assert_eq!(fresh.get(p.id, SecretKind::Password).as_deref(), Some("p's password"));
    assert_eq!(fresh.history_key(), key, "the same Collection History key");
    assert!(Vault::unlock(path.clone(), TEST_PASSWORD).is_err(), "the old password no longer opens it");

    let app = h.state();
    assert_eq!(app.history_store().unwrap().list(p.id, &ns).unwrap().iter().map(|e| e.count).collect::<Vec<_>>(), [2]);
    app.vault.as_ref().unwrap().set(p.id, SecretKind::Ssh, "saved after").unwrap();
    assert_eq!(Vault::unlock(path, "a new password").unwrap().get(p.id, SecretKind::Ssh).as_deref(), Some("saved after"));
}

/// Settings → Reset…: asks first, naming what is lost, with "Also delete
/// all Collection History (N MB)" ticked. Then the vault is gone (every
/// saved password with it) and the Set dialog asks for a new master
/// password; saved connections stay listed. Ticked, the history files
/// go too; unticked, they stay, and can't be read under the new key.
#[test]
fn reset_forgets_secrets_keeps_connections() {
    for tick in [true, false] {
        let (ctx, mut app) = test_app(false);
        let p = saved_with_password(&mut app, "p");
        let plain = ConnectionProfile::new("plain");
        app.store.as_mut().unwrap().upsert(plain.clone(), None, None, app.vault.as_deref().unwrap()).unwrap();
        let ns = Namespace::new("shop", "orders");
        saved_history(&app, p.id, &ns, 2);
        let old = app.vault.clone().unwrap();
        let path = old.path().to_path_buf();
        let used = app.history_store().unwrap().usage().unwrap();
        assert!(used > 0);

        open_settings(&ctx, &mut app);
        app.settings_action(SettingsAction::ResetVault);
        let measured_offer = |a: &RongoApp| a.reset_ask.as_ref().is_some_and(|c| c.checkbox.as_ref().is_some_and(|(l, _)| l.ends_with("MB)")));
        step_until(&ctx, &mut app, "the history is measured", measured_offer);
        let ask = app.reset_ask.as_ref().unwrap();
        assert_eq!(ask.title, "Reset Master Password");
        assert!(ask.danger && ask.message.contains("passwords") && ask.message.contains("Collection History") && ask.message.contains("connections stay"), "{}", ask.message);
        assert_eq!(ask.checkbox, Some((format!("Also delete all Collection History ({})", mb(used)), true)), "ticked by default");
        assert!(app.settings_dialog.is_some(), "Settings stays behind the question");
        assert!(path.exists(), "nothing deleted before the answer");

        let mut ask = app.reset_ask.take().unwrap();
        ask.checkbox.as_mut().unwrap().1 = tick;
        app.confirmed(ask);
        assert!(app.vault.is_none() && !path.exists(), "the vault is gone");
        assert!(Vault::was_reset(&path), "remembered until a new vault is made");
        assert!(app.settings_dialog.is_none(), "Settings closes");
        let names: Vec<_> = app.store.as_ref().unwrap().profiles().iter().map(|p| p.name.clone()).collect();
        assert_eq!(names, ["p", "plain"], "the connections stay");
        let texts = drawn_texts(&ctx, &mut app);
        assert!(texts.iter().any(|t| t.contains("Set a Master Password")), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains("was reset")), "{texts:?}");
        assert!(!texts.iter().any(|t| t.contains("Connect")), "locked: {texts:?}");

        step_until(&ctx, &mut app, "the history files are done", |a| a.history_files.idle());
        let files = Store::new(app.history_root(), HistoryKey::new([0; 32]));
        assert_eq!(files.usage().unwrap(), if tick { 0 } else { used }, "ticked: {tick}");

        // A new master password: the connections are there, their passwords are not.
        let new = Arc::new(Vault::create(path.clone(), "a new password", rongo_core::vault::KdfParams::TEST, BTreeMap::new()).unwrap());
        app.unlock_answer(&ctx, UnlockResult::Unlocked { vault: new.clone(), keychain_left: None });
        assert!(!Vault::was_reset(&path));
        assert_eq!(new.get(p.id, SecretKind::Password), None);
        assert_ne!(new.history_key(), old.history_key());
        assert!(app.store.as_ref().unwrap().get(p.id).is_some());
        if !tick {
            let entries = app.history_store().unwrap().list(p.id, &ns).unwrap();
            assert_eq!(entries.iter().map(|e| (e.count, e.conn_name.as_str())).collect::<Vec<_>>(), [(0, "")], "listed, but it can't be read");
            app.open_history(ConnId(p.id), ns.clone());
            step_until(&ctx, &mut app, "the entry is previewed", |a| matches!(&a.tabs[a.active_tab].kind, TabKind::History(h) if h.previewed().is_some()));
            let texts = drawn_texts(&ctx, &mut app);
            assert!(texts.iter().any(|t| t.contains("saved under a master password that was reset since")), "{texts:?}");
        }
    }
}

/// Reset waits while Collection History is being written (a recorded
/// delete or update, a paste that overwrites, a restore): Settings →
/// Reset… and Forgot? say to wait and ask nothing, and a question asked
/// before such a write began is refused when answered. Nothing is reset
/// meanwhile. Once it is done, Reset goes on, and its "delete all
/// Collection History" keeps what is recorded after it.
#[test]
fn reset_waits_for_history_being_written() {
    use crate::test_support::TEST_PASSWORD;
    let (ctx, mut app) = test_app(false);
    let p = saved_with_password(&mut app, "p");
    let path = app.vault.as_ref().unwrap().path().to_path_buf();
    let ns = Namespace::new("shop", "orders");
    saved_history(&app, p.id, &ns, 2);
    let waits = |app: &RongoApp| toast_texts(app).iter().any(|t| t.starts_with("Wait for") && t.contains("before resetting"));
    writing(&mut app, ConnId(p.id), &ns);

    app.settings_action(SettingsAction::ResetVault);
    assert!(app.reset_ask.is_none(), "nothing asked");
    assert!(waits(&app), "{:?}", toast_texts(&app));
    assert!(path.exists() && app.vault.is_some());

    // Forgot? at launch: the same.
    app.toasts.clear();
    app.vault = None;
    app.unlock = Some(UnlockDialog::new(path.clone(), false, Vec::new()));
    app.unlock_answer(&ctx, UnlockResult::Reset);
    assert!(app.reset_ask.is_none() && app.unlock.is_some(), "back to Unlock, nothing asked");
    assert!(waits(&app), "{:?}", toast_texts(&app));
    app.unlock_answer(&ctx, UnlockResult::Unlocked { vault: Arc::new(Vault::unlock(path.clone(), TEST_PASSWORD).unwrap()), keychain_left: None });

    // Asked while nothing was written, answered once something is: refused.
    written(&mut app);
    app.toasts.clear();
    app.settings_action(SettingsAction::ResetVault);
    let ask = app.reset_ask.take().expect("asks first");
    writing(&mut app, ConnId(p.id), &ns);
    app.confirmed(ask);
    assert!(waits(&app), "{:?}", toast_texts(&app));
    assert!(path.exists() && app.vault.is_some() && app.unlock.is_none(), "nothing reset");

    // Done: Reset goes on.
    written(&mut app);
    app.settings_action(SettingsAction::ResetVault);
    let ask = app.reset_ask.take().expect("asks first");
    assert!(ask.checkbox.as_ref().is_some_and(|(_, ticked)| *ticked));
    app.confirmed(ask);
    assert!(!path.exists() && app.vault.is_none());
    // A later millisecond than the Reset.
    std::thread::sleep(Duration::from_millis(5));

    // A new master password, and a delete recorded under it before the deletion ran.
    let new = Arc::new(Vault::create(path, "a new password", rongo_core::vault::KdfParams::TEST, BTreeMap::new()).unwrap());
    app.unlock_answer(&ctx, UnlockResult::Unlocked { vault: new, keychain_left: None });
    saved_history(&app, p.id, &ns, 3);
    step_until(&ctx, &mut app, "the deletion ran", |a| a.history_files.idle());
    let counts: Vec<u64> = app.history_store().unwrap().list(p.id, &ns).unwrap().iter().map(|e| e.count).collect();
    assert_eq!(counts, [3], "only the entry recorded after the Reset stays");
    assert!(app.toasts.iter().all(|t| !t.text.starts_with("Collection History:")), "nothing refused: {:?}", toast_texts(&app));
}

/// The vault changed in another window while Change Master Password was
/// open: locking again closes it (its vault is gone).
#[test]
fn locking_again_closes_the_change_dialog() {
    use crate::test_support::TEST_PASSWORD;
    let (_ctx, mut app, other) = two_windows();
    app.settings_action(SettingsAction::ChangeMasterPassword);
    assert!(app.change_password.is_some());
    other.change_password(TEST_PASSWORD, "another password").unwrap();
    assert!(app.fresh_vault().is_none());
    assert!(app.unlock.is_some() && app.change_password.is_none());
}

/// The vault changed in another window while History tabs were open:
/// locking again closes them, as Reset does (they hold a store sealed
/// with this window's key); other tabs stay.
#[test]
fn locking_again_closes_history_tabs() {
    use crate::test_support::TEST_PASSWORD;
    let (_ctx, mut app, other) = two_windows();
    let p = ConnectionProfile::new("p");
    let ns = Namespace::new("shop", "orders");
    saved_history(&app, p.id, &ns, 1);
    app.open_history(ConnId(p.id), ns.clone());
    assert!(app.tabs.iter().any(|t| matches!(t.kind, TabKind::History(_))));
    other.change_password(TEST_PASSWORD, "another password").unwrap();
    assert!(app.fresh_vault().is_none());
    assert!(app.unlock.is_some());
    assert!(!app.tabs.iter().any(|t| matches!(t.kind, TabKind::History(_))), "History tabs closed");
    assert!(!app.tabs.is_empty(), "the welcome tab is left");
}

/// Reset deletes rongo's items left in the Keychain (the vault item and
/// each saved connection's older items), so a later launch can't move
/// them back; others stay. A Keychain that refuses is said in a toast and
/// never stops the Reset. (A Keychain in memory: tests never touch the real one.)
#[test]
fn reset_deletes_rongos_items_left_in_the_keychain() {
    use crate::test_support::{KEYCHAIN_VAULT, Mem};
    use rongo_core::vault::vault_key;
    for refuse in [false, true] {
        let (ctx, mut app) = test_app(false);
        let p = saved_with_password(&mut app, "p");
        let q = ConnectionProfile::new("q");
        app.store.as_mut().unwrap().upsert(q.clone(), None, None, app.vault.as_deref().unwrap()).unwrap();
        let mem = Arc::new(if refuse { Mem::refusing() } else { Mem::default() });
        for account in [KEYCHAIN_VAULT.to_string(), vault_key(p.id, SecretKind::Password), vault_key(p.id, SecretKind::Ssh), vault_key(q.id, SecretKind::Ssh), "something else".into()] {
            mem.put(&account, "old");
        }
        app.keychain = Some(mem.clone());
        let path = app.vault.as_ref().unwrap().path().to_path_buf();

        app.settings_action(SettingsAction::ResetVault);
        let ask = app.reset_ask.take().expect("asks first");
        app.confirmed(ask);
        assert!(app.vault.is_none() && app.unlock.is_some() && !path.exists(), "reset, refused or not");
        step_until(&ctx, &mut app, "the Keychain is done", |a| a.keychain_forget.is_none());
        if refuse {
            assert!(mem.accounts().contains(&KEYCHAIN_VAULT.to_string()));
            let toasts = toast_texts(&app);
            assert!(toasts.iter().any(|t| t.contains("Keychain") && t.contains("the user said no")), "{toasts:?}");
        } else {
            assert_eq!(mem.accounts(), ["something else"]);
            assert!(app.toasts.iter().all(|t| !t.error), "{:?}", toast_texts(&app));
        }
    }
}

/// A Reset whose Keychain clean-up was refused leaves rongo's items
/// there, but they can't come back: the next launch asks to set a
/// password (never to move from the Keychain), imports nothing, and
/// once the new vault is made the Reset is no longer remembered.
#[test]
fn a_refused_keychain_clean_up_is_not_moved_back() {
    use crate::test_support::{KEYCHAIN_VAULT, Mem};
    use egui_kittest::kittest::Queryable;
    use rongo_core::vault::vault_key;
    let mut h = app_harness();
    let p = saved_with_password(h.state_mut(), "p");
    let path = h.state().vault.as_ref().unwrap().path().to_path_buf();
    let mem = Arc::new(Mem::refusing());
    mem.put(KEYCHAIN_VAULT, &serde_json::to_string(&BTreeMap::from([(vault_key(p.id, SecretKind::Password), "from the Keychain")])).unwrap());
    mem.put(&vault_key(p.id, SecretKind::Password), "from the Keychain");
    let keychain: Arc<dyn SecretBackend> = mem.clone();
    h.state_mut().keychain = Some(keychain.clone());
    h.state_mut().settings_action(SettingsAction::ResetVault);
    let ask = h.state_mut().reset_ask.take().expect("asks first");
    h.state_mut().confirmed(ask);
    harness_until(&mut h, "the Keychain refuses", |a| a.keychain_forget.is_none());
    assert_eq!(mem.accounts().len(), 2, "the Keychain kept them");
    assert!(!path.exists() && Vault::was_reset(&path));

    // The next launch, with that Keychain and a connection that uses a password.
    let profiles = h.state().store.as_ref().unwrap().profiles().to_vec();
    assert!(!crate::unlock::should_move(&path, Some(&profiles), &h.state().history_root()), "nothing to move after a Reset");
    assert!(!crate::unlock::should_move(&path, None, &h.state().history_root()), "nor with the connections unreadable");
    h.state_mut().unlock = Some(UnlockDialog::for_test(path.clone(), rongo_core::vault::KdfParams::TEST, Some((keychain, profiles))));
    h.run_steps(3);
    let texts = texts_of(h.output());
    assert!(texts.iter().any(|t| t.contains("Set a Master Password")), "{texts:?}");
    assert!(!texts.iter().any(|t| t.contains("instead of in the Keychain")), "Set, not Move: {texts:?}");
    type_password(&mut h, 0, "a brand new password");
    type_password(&mut h, 1, "a brand new password");
    h.get_by_label("Set").click();
    harness_until(&mut h, "unlocked", |a| a.vault.is_some());
    assert_eq!(h.state().vault.as_ref().unwrap().get(p.id, SecretKind::Password), None, "nothing imported");
    assert!(!Vault::was_reset(&path), "the new vault is there: the Reset is no longer remembered");
    assert_eq!(mem.accounts().len(), 2, "and the Keychain is left alone");
}

/// Forgot? at launch opens the same Reset question (Cancel goes back to
/// Unlock, deleting nothing); Reset then asks for a new master password,
/// which opens an empty vault, the connections still listed.
#[test]
fn forgot_at_launch_resets_then_sets_a_new_password() {
    use egui_kittest::kittest::Queryable;
    let mut h = app_harness();
    let p = saved_with_password(h.state_mut(), "p");
    let path = h.state().vault.as_ref().unwrap().path().to_path_buf();
    h.state_mut().vault = None;
    h.state_mut().unlock = Some(UnlockDialog::new(path.clone(), false, Vec::new()));
    h.run_steps(3);
    assert!(texts_of(h.output()).iter().any(|t| t.contains("Unlock rongo")));

    h.get_by_label("Forgot?").click();
    h.run_steps(1);
    reset_ask_measured(&mut h);
    let texts = texts_of(h.output());
    assert!(texts.iter().any(|t| t == "Reset Master Password"), "{texts:?}");
    assert!(!texts.iter().any(|t| t.contains("Unlock rongo")), "the question alone: {texts:?}");
    h.get_by_label("Cancel").click();
    h.run_steps(3);
    assert!(texts_of(h.output()).iter().any(|t| t.contains("Unlock rongo")), "back to Unlock");
    assert!(path.exists() && h.state().reset_ask.is_none());

    h.get_by_label("Forgot?").click();
    h.run_steps(1);
    reset_ask_measured(&mut h);
    h.get_by_label("Reset").click();
    h.run_steps(3);
    assert!(!path.exists() && h.state().vault.is_none());
    let texts = texts_of(h.output());
    assert!(texts.iter().any(|t| t.contains("Set a Master Password")), "{texts:?}");

    type_password(&mut h, 0, "a brand new password");
    type_password(&mut h, 1, "a brand new password");
    h.get_by_label("Set").click();
    harness_until(&mut h, "unlocked", |a| a.vault.is_some());
    let vault = h.state().vault.clone().unwrap();
    assert_eq!(vault.get(p.id, SecretKind::Password), None, "its password is gone");
    assert!(h.state().store.as_ref().unwrap().get(p.id).is_some(), "the connection stays");
    assert!(Vault::unlock(path, "a brand new password").is_ok());
    assert!(texts_of(h.output()).iter().any(|t| t.contains("Connect")), "the app is there");
}

/// A move out of the Keychain that failed (a damaged history key there)
/// offers Reset…: the same question, then the Keychain's items are
/// deleted and the Set dialog (not Move again) asks for a new password.
#[test]
fn a_failed_move_can_be_reset() {
    use crate::test_support::{KEYCHAIN_VAULT, Mem};
    use egui_kittest::kittest::Queryable;
    use rongo_core::profile::SecretBackend;
    let mut h = app_harness();
    let path = h.state().vault.as_ref().unwrap().path().to_path_buf();
    std::fs::remove_file(&path).unwrap();
    let mem = Arc::new(Mem::default());
    let damaged = BTreeMap::from([(rongo_core::vault::vault_key(uuid::Uuid::nil(), SecretKind::HistoryKey), "damaged")]);
    mem.put(KEYCHAIN_VAULT, &serde_json::to_string(&damaged).unwrap());
    mem.put("something else", "x");
    let keychain: Arc<dyn SecretBackend> = mem.clone();
    h.state_mut().keychain = Some(keychain.clone());
    h.state_mut().vault = None;
    h.state_mut().unlock = Some(UnlockDialog::for_test(path.clone(), rongo_core::vault::KdfParams::TEST, Some((keychain, Vec::new()))));
    h.run_steps(3);
    type_password(&mut h, 0, "a good password");
    type_password(&mut h, 1, "a good password");
    h.get_by_label("Set").click();
    let end = Instant::now() + WAIT;
    while !texts_of(h.output()).iter().any(|t| t.contains("Couldn't move")) {
        assert!(Instant::now() < end, "the move never failed");
        h.step();
        std::thread::sleep(Duration::from_millis(5));
    }

    h.get_by_label("Reset…").click();
    h.run_steps(1);
    reset_ask_measured(&mut h);
    assert!(texts_of(h.output()).iter().any(|t| t == "Reset Master Password"));
    h.get_by_label("Reset").click();
    // Answered (the click is taken in the next frame), then the Keychain's clean-up done.
    harness_until(&mut h, "the Keychain is done", |a| a.reset_ask.is_none() && a.keychain_forget.is_none());
    assert_eq!(mem.accounts(), ["something else"], "the damaged item is gone");
    let texts = texts_of(h.output());
    assert!(texts.iter().any(|t| t.contains("Set a Master Password")), "{texts:?}");
    assert!(!texts.iter().any(|t| t.contains("instead of in the Keychain") || t.contains("Couldn't move")), "Set, not Move: {texts:?}");

    type_password(&mut h, 0, "a good password");
    type_password(&mut h, 1, "a good password");
    h.get_by_label("Set").click();
    harness_until(&mut h, "unlocked", |a| a.vault.is_some());
    assert!(Vault::unlock(path, "a good password").is_ok());
}

/// A move out of the Keychain that worked, but whose clean-up the
/// Keychain refused: the vault stays (with every secret), the app opens,
/// and a toast says rongo's old Keychain items could not be removed.
#[test]
fn a_refused_clean_up_after_a_move_is_said() {
    use crate::test_support::{KEYCHAIN_VAULT, Mem};
    use egui_kittest::kittest::Queryable;
    use rongo_core::profile::SecretBackend;
    let mut h = app_harness();
    let path = h.state().vault.as_ref().unwrap().path().to_path_buf();
    std::fs::remove_file(&path).unwrap();
    let p = ConnectionProfile::new("p");
    let mem = Arc::new(Mem::refusing());
    mem.put(KEYCHAIN_VAULT, &serde_json::to_string(&BTreeMap::from([(vault_key(p.id, SecretKind::Password), "from the Keychain")])).unwrap());
    let keychain: Arc<dyn SecretBackend> = mem.clone();
    h.state_mut().vault = None;
    h.state_mut().unlock = Some(UnlockDialog::for_test(path.clone(), rongo_core::vault::KdfParams::TEST, Some((keychain, vec![p.clone()]))));
    h.run_steps(3);
    type_password(&mut h, 0, "a good password");
    type_password(&mut h, 1, "a good password");
    h.get_by_label("Set").click();
    harness_until(&mut h, "unlocked", |a| a.vault.is_some());
    assert_eq!(h.state().vault.as_ref().unwrap().get(p.id, SecretKind::Password).as_deref(), Some("from the Keychain"));
    assert_eq!(mem.accounts(), [KEYCHAIN_VAULT], "the Keychain kept it");
    let toasts = toast_texts(h.state());
    assert!(toasts.iter().any(|t| t.contains("Keychain") && t.contains("could not all be deleted") && t.contains("the user said no")), "{toasts:?}");
    assert!(!toasts.iter().any(|t| t.contains("from the Keychain")), "no secret in a toast: {toasts:?}");
}

/// Reset's Keychain clean-up whose thread stops (a panic, say) is said in
/// words of its own, not a history read's ("reading it stopped").
#[test]
fn a_keychain_clean_up_that_stops_says_so() {
    let (ctx, mut app) = test_app(false);
    app.keychain_forget = Some(crate::job::Job::start(&ctx, "keychain-forget", |_| -> rongo_core::CoreResult<()> { panic!("test: the clean-up stops") }));
    step_until(&ctx, &mut app, "the clean-up is answered", |a| a.keychain_forget.is_none());
    let toasts = toast_texts(&app);
    assert!(toasts.iter().any(|t| t.contains("could not all be deleted (the clean-up stopped unexpectedly)")), "{toasts:?}");
    assert!(!toasts.iter().any(|t| t.contains("reading")), "{toasts:?}");
}
