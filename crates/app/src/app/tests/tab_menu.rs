//! The tab bar's right-click menu, which closes tabs.

use super::*;
use crate::app::tab_ops::CloseWhich;
use rongo_core::mock::TEST_DB;

fn tab_ids(app: &RongoApp) -> Vec<u64> {
    app.tabs.iter().map(|t| t.id).collect()
}

/// Whether the tab `id` is a shell running a script.
fn runs(app: &RongoApp, id: u64) -> bool {
    app.tabs.iter().any(|t| t.id == id && matches!(&t.kind, TabKind::Shell(s) if s.is_running()))
}

/// The demo connected and its script done, then only four empty shell
/// tabs on it: their ids, left to right.
fn four_shells() -> (egui::Context, RongoApp, ConnId, Vec<u64>) {
    let (ctx, mut app, cid) = connected_mock_app();
    step_until(&ctx, &mut app, "the demo script finishes", |a| !a.tabs.iter().any(|t| matches!(&t.kind, TabKind::Shell(s) if s.is_running() || s.run_when_ready)));
    for _ in 0..4 {
        app.open_shell(cid, Some(TEST_DB.into()), None);
    }
    let shells: Vec<u64> = tab_ids(&app).split_off(app.tabs.len() - 4);
    while let Some(i) = app.tabs.iter().position(|t| !shells.contains(&t.id)) {
        app.close_tab(i);
    }
    assert_eq!(tab_ids(&app), shells);
    (ctx, app, cid, shells)
}

/// A script that runs until stopped, in a new shell tab: its id.
fn running_shell(ctx: &egui::Context, app: &mut RongoApp, cid: ConnId) -> u64 {
    app.open_shell(cid, Some(TEST_DB.into()), Some("sleep(60000)".into()));
    let id = app.tabs.last().unwrap().id;
    if let TabKind::Shell(s) = &mut app.tabs.last_mut().unwrap().kind {
        s.run_when_ready = true;
    }
    step_until(ctx, app, "the script runs", |a| runs(a, id));
    id
}

fn stop_all(ctx: &egui::Context, app: &mut RongoApp) {
    for t in &app.tabs {
        if let TabKind::Shell(s) = &t.kind {
            s.stop();
        }
    }
    step_until(ctx, app, "the scripts stop", |a| !a.tabs.iter().any(|t| runs(a, t.id)));
}

/// Close Others leaves the tab right-clicked, and shows it.
#[test]
fn close_others_keeps_the_tab_and_shows_it() {
    let (_ctx, mut app, _, ids) = four_shells();
    app.active_tab = 0;
    app.close_from_menu(2, CloseWhich::Others);
    assert_eq!(tab_ids(&app), [ids[2]]);
    assert_eq!(app.active_tab, 0);
    assert!(app.confirm.is_none());
}

/// Close Tabs to the Right closes only the tabs after it. The active tab
/// stays active when it is left; when it is closed, the tab right-clicked
/// is shown.
#[test]
fn close_to_the_right_closes_only_those_after_it() {
    let (_ctx, mut app, _, ids) = four_shells();
    app.active_tab = 0;
    app.close_from_menu(1, CloseWhich::ToTheRight);
    assert_eq!(tab_ids(&app), ids[..2]);
    assert_eq!(app.active_tab, 0, "still the one that was active");

    let (_ctx, mut app, _, ids) = four_shells();
    app.active_tab = 3;
    app.close_from_menu(1, CloseWhich::ToTheRight);
    assert_eq!(tab_ids(&app), ids[..2]);
    assert_eq!(app.active_tab, 1, "the active tab was closed: the one right-clicked is shown");
}

/// Close Tabs of a connection leaves every other connection's tabs.
#[test]
fn close_tabs_of_a_connection_leaves_the_others() {
    let (ctx, mut app, cid, ids) = four_shells();
    let other = connect_mock_b(&ctx, &mut app);
    app.open_shell(other, Some(TEST_DB.into()), None);
    let theirs = app.tabs.last().unwrap().id;
    app.push_tab(|_| TabKind::Welcome);
    let welcome = app.tabs.last().unwrap().id;
    app.close_from_menu(1, CloseWhich::OfConnection);
    assert!(app.tabs.iter().all(|t| t.conn() != Some(cid)), "{:?}", tab_ids(&app));
    assert_eq!(tab_ids(&app), [theirs, welcome]);
    assert!(ids.iter().all(|id| !tab_ids(&app).contains(id)));
}

/// Close All leaves the Welcome tab, as closing the last tab does.
#[test]
fn close_all_leaves_the_welcome_tab() {
    let (_ctx, mut app, _, _) = four_shells();
    app.close_from_menu(3, CloseWhich::All);
    assert_eq!(app.tabs.len(), 1);
    assert!(matches!(app.tabs[0].kind, TabKind::Welcome));
    assert_eq!(app.active_tab, 0);
}

/// A script typed into a shell tab goes with it: closing several tabs that
/// hold scripts asks first, and only Close closes them. One tab closes
/// without a question, as ⌘W and its ✕ do.
#[test]
fn closing_several_scripts_asks_first() {
    let (_ctx, mut app, cid, _) = four_shells();
    app.open_shell(cid, Some(TEST_DB.into()), Some("db.orders.find({})".into()));
    app.open_shell(cid, Some(TEST_DB.into()), Some("db.products.find({})".into()));
    let before = tab_ids(&app);
    app.close_from_menu(0, CloseWhich::All);
    let ask = app.confirm.take().expect("asks first");
    assert_eq!(tab_ids(&app), before, "nothing closed yet");
    assert!(ask.message.contains("6 tabs") && ask.message.contains("2 IntelliShell tabs"), "{}", ask.message);
    assert_eq!(ask.ok_label, "Close");
    app.confirmed(ask);
    assert_eq!(app.tabs.len(), 1);
    assert!(matches!(app.tabs[0].kind, TabKind::Welcome));

    // Empty shells close without a question; so does one tab with a script.
    let (_ctx, mut app, cid, ids) = four_shells();
    app.close_from_menu(0, CloseWhich::Others);
    assert!(app.confirm.is_none());
    assert_eq!(tab_ids(&app), [ids[0]]);
    app.open_shell(cid, Some(TEST_DB.into()), Some("db.orders.find({})".into()));
    app.close_from_menu(1, CloseWhich::This);
    assert!(app.confirm.is_none());
    assert_eq!(tab_ids(&app), [ids[0]]);
}

/// A tab whose script is running stays open, said once for all of them,
/// and is not counted among the scripts the question warns of.
#[test]
fn running_scripts_stay_open_said_once() {
    let (ctx, mut app, cid, ids) = four_shells();
    let a = running_shell(&ctx, &mut app, cid);
    let b = running_shell(&ctx, &mut app, cid);
    app.close_from_menu(0, CloseWhich::All);
    assert!(app.confirm.is_none(), "a running script is not closed, so not asked about");
    assert_eq!(tab_ids(&app), [a, b]);
    assert!(ids.iter().all(|id| !tab_ids(&app).contains(id)));
    let toasts = toast_texts(&app);
    assert_eq!(toasts, ["2 tabs stay open: their scripts are running"], "{toasts:?}");
    stop_all(&ctx, &mut app);
}

/// Right-clicking a tab offers the closes; Close Others acts on the tab
/// right-clicked, not the active one.
#[test]
fn right_clicking_a_tab_offers_the_closes() {
    use egui_kittest::kittest::Queryable;
    let mut h = harness(egui::vec2(1200.0, 800.0), true);
    let cid = ConnectionProfile::mock().conn_id();
    harness_until(&mut h, "the demo connects and its script ends", |a| {
        matches!(a.conns.get(&cid).map(|c| &c.status), Some(Status::Connected(_))) && !a.tabs.iter().any(|t| matches!(&t.kind, TabKind::Shell(s) if s.is_running() || s.run_when_ready))
    });
    for _ in 0..2 {
        h.state_mut().open_shell(cid, Some(TEST_DB.into()), None);
    }
    h.state_mut().active_tab = 0;
    h.run_steps(3);
    let shells: Vec<u64> = h.state().tabs.iter().filter(|t| matches!(t.kind, TabKind::Shell(_))).map(|t| t.id).collect();
    assert_eq!(shells.len(), 3, "the demo's shell and two more");
    // The tab titles, left to right (each shell tab also names itself below).
    let mut titles: Vec<_> = h.get_all_by_label_contains("IntelliShell: Demo (mock)").collect();
    titles.sort_by(|a, b| a.rect().top().total_cmp(&b.rect().top()).then(a.rect().left().total_cmp(&b.rect().left())));
    let top = titles[0].rect().top();
    titles.retain(|n| (n.rect().top() - top).abs() < 1.0);
    assert_eq!(titles.len(), 3, "one title per tab");
    titles[1].click_secondary();
    h.run_steps(3);
    for item in ["Close", "Close Others", "Close Tabs to the Right", "Close Tabs of Demo (mock)", "Close All"] {
        assert!(h.query_all_by_label_contains(item).count() > 0, "{item}");
    }
    h.get_by_label_contains("Close Others").click();
    h.run_steps(3);
    // The demo's shell holds its script: asked first.
    let message = h.state().confirm.as_ref().map(|c| c.message.clone()).expect("asks first");
    assert!(message.contains("The script in 1 IntelliShell tab"), "{message}");
    h.get_by_label("Close").click();
    h.run_steps(3);
    assert_eq!(tab_ids(h.state()), [shells[1]]);
}
