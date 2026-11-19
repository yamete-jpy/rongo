//! Find in IntelliShell's script (⌘F): who gets ⌘F, and the script's keys
//! and its tab with the find bar open.

use super::*;
use crate::tabs::ShellTab;
use rongo_core::mock::TEST_DB;

/// The demo connected and its script done, then a shell tab of its own on
/// screen whose script has run, its result under the script: the app and
/// the tab's id.
fn shell_with_results() -> (egui::Context, RongoApp, u64) {
    let (ctx, mut app, cid) = connected_mock_app();
    step_until(&ctx, &mut app, "the demo script finishes", |a| !a.tabs.iter().any(|t| matches!(&t.kind, TabKind::Shell(s) if s.is_running() || s.run_when_ready)));
    app.open_shell(cid, Some(TEST_DB.into()), Some("db.getCollection(\"advertisers\").find({})\n".into()));
    let id = app.tabs.last().unwrap().id;
    if let TabKind::Shell(s) = &mut app.tabs.last_mut().unwrap().kind {
        s.run_when_ready = true;
    }
    step_until(&ctx, &mut app, "the script ran", |a| a.requests.pending.is_empty() && shell(a, id).is_some_and(|s| !s.run_when_ready && !s.is_running() && !s.results.is_empty()));
    (ctx, app, id)
}

/// The shell tab `id`, if it is open.
fn shell(app: &RongoApp, id: u64) -> Option<&ShellTab> {
    app.tabs.iter().find(|t| t.id == id).and_then(|t| match &t.kind {
        TabKind::Shell(s) => Some(&**s),
        _ => None,
    })
}

/// One frame of the app with `events`.
fn run(ctx: &egui::Context, app: &mut RongoApp, events: Vec<egui::Event>) {
    let mut frame = eframe::Frame::_new_kittest();
    let input = egui::RawInput { events, ..frame_input() };
    let mut out = ctx.run_ui(input, |ui| eframe::App::ui(app, ui, &mut frame));
    out.textures_delta.clear();
}

fn key(key: Key, modifiers: Modifiers) -> egui::Event {
    egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }
}

/// The script's find bar is open, and its field has the keyboard focus.
fn finding(ctx: &egui::Context, app: &RongoApp, id: u64) -> (bool, bool) {
    let open = shell(app, id).is_some_and(|s| s.script().find().is_some());
    (open, ctx.memory(|m| m.has_focus(crate::doc_find::field_id(script_id(id)))))
}

fn script_id(tab: u64) -> egui::Id {
    egui::Id::new(("shell-editor", tab))
}

const OVER_SIDEBAR: egui::Pos2 = egui::pos2(150.0, 400.0);
const OVER_RESULTS: egui::Pos2 = egui::pos2(900.0, 700.0);

/// A place on the script's first lines.
fn over_script(ctx: &egui::Context, id: u64) -> egui::Pos2 {
    ctx.read_response(script_id(id)).expect("the script drawn").rect.left_top() + egui::vec2(200.0, 30.0)
}

fn tree_search(ctx: &egui::Context) -> bool {
    ctx.memory(|m| m.has_focus(egui::Id::new("tree-search")))
}

/// The results under the script have their find bar open.
fn results_find(app: &RongoApp, id: u64) -> bool {
    shell(app, id).is_some_and(|s| s.results.iter().any(|r| r.view.find_open()))
}

/// Puts the script's caret at its end.
fn caret_at_end(ctx: &egui::Context, app: &RongoApp, id: u64) {
    let n = shell(app, id).unwrap().script().text.chars().count();
    let mut state = egui::TextEdit::load_state(ctx, script_id(id)).expect("the script's state");
    state.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(n))));
    state.store(ctx, script_id(id));
}

/// ⌘F with the script focused opens its find bar and focuses the bar's
/// field wherever the pointer is: over the sidebar, over the results under
/// the script, over the script. Esc closes the bar, the tab stays, the
/// script has the focus. With the script not focused, ⌘F goes where it
/// did: the results' find bar under the pointer (not over the script,
/// which is no part of them), the tree search over the sidebar or with
/// nothing under the pointer claiming it.
#[test]
fn command_f_in_the_script_finds_in_the_script() {
    let (ctx, mut app, id) = shell_with_results();
    let results_find = |app: &RongoApp| results_find(app, id);
    let script = over_script(&ctx, id);
    for at in [OVER_SIDEBAR, OVER_RESULTS, script] {
        run(&ctx, &mut app, vec![egui::Event::PointerMoved(at)]);
        ctx.memory_mut(|m| m.request_focus(script_id(id)));
        run(&ctx, &mut app, vec![]);
        run(&ctx, &mut app, vec![key(Key::F, Modifiers::COMMAND)]);
        run(&ctx, &mut app, vec![]);
        assert_eq!(finding(&ctx, &app, id), (true, true), "{at:?}: the script's find bar opened, its field focused");
        assert!(!tree_search(&ctx) && !results_find(&app), "{at:?}: nothing else took ⌘F");
        run(&ctx, &mut app, vec![key(Key::Escape, Modifiers::NONE)]);
        run(&ctx, &mut app, vec![]);
        assert_eq!(finding(&ctx, &app, id), (false, false), "{at:?}: Esc closed the bar");
        assert!(shell(&app, id).is_some(), "the tab stays");
        assert!(ctx.memory(|m| m.has_focus(script_id(id))), "the script has the focus");
    }

    ctx.memory_mut(|m| m.surrender_focus(script_id(id)));
    run(&ctx, &mut app, vec![egui::Event::PointerMoved(script)]);
    run(&ctx, &mut app, vec![]);
    run(&ctx, &mut app, vec![key(Key::F, Modifiers::COMMAND)]);
    run(&ctx, &mut app, vec![]);
    assert!(!results_find(&app), "⌘F over the script, not focused: the results under it are not under the pointer");
    assert!(tree_search(&ctx), "nothing under the pointer claimed it: the tree search");
    assert!(!finding(&ctx, &app, id).0, "and not the script's");

    run(&ctx, &mut app, vec![egui::Event::PointerMoved(OVER_RESULTS)]);
    run(&ctx, &mut app, vec![]);
    run(&ctx, &mut app, vec![key(Key::F, Modifiers::COMMAND)]);
    run(&ctx, &mut app, vec![]);
    assert!(results_find(&app), "⌘F over the results, the script not focused: the results' find bar");
    assert!(!tree_search(&ctx) && !finding(&ctx, &app, id).0, "and nothing else");

    run(&ctx, &mut app, vec![egui::Event::PointerMoved(OVER_SIDEBAR)]);
    run(&ctx, &mut app, vec![]);
    run(&ctx, &mut app, vec![key(Key::F, Modifiers::COMMAND)]);
    run(&ctx, &mut app, vec![]);
    assert!(tree_search(&ctx), "⌘F over the sidebar, the script not focused: the tree search");
    assert!(!finding(&ctx, &app, id).0, "and not the script's");
}

/// Esc closing the suggestions leaves the script its focus: ⌘F then finds
/// in the script, not in the results under it.
#[test]
fn command_f_after_esc_closes_the_suggestions() {
    let (ctx, mut app, id) = shell_with_results();
    run(&ctx, &mut app, vec![egui::Event::PointerMoved(over_script(&ctx, id))]);
    ctx.memory_mut(|m| m.request_focus(script_id(id)));
    run(&ctx, &mut app, vec![]);
    caret_at_end(&ctx, &app, id);
    run(&ctx, &mut app, vec![]);
    run(&ctx, &mut app, vec![egui::Event::Text("db.getNa".into())]);
    run(&ctx, &mut app, vec![]);
    assert!(shell(&app, id).unwrap().script().completing(), "the suggestions opened");
    run(&ctx, &mut app, vec![key(Key::Escape, Modifiers::NONE)]);
    run(&ctx, &mut app, vec![]);
    assert!(!shell(&app, id).unwrap().script().completing(), "Esc closed them");
    assert!(ctx.memory(|m| m.has_focus(script_id(id))), "the script kept the focus");
    run(&ctx, &mut app, vec![key(Key::F, Modifiers::COMMAND)]);
    run(&ctx, &mut app, vec![]);
    assert_eq!(finding(&ctx, &app, id), (true, true), "⌘F found in the script");
    assert!(!results_find(&app, id) && !tree_search(&ctx), "and nowhere else");
}

/// A click on the find bar (its magnifier) keeps the keys in its field:
/// ⌘F then goes to the bar again, not to the results under the script.
#[test]
fn command_f_after_a_click_on_the_find_bar() {
    let (ctx, mut app, id) = shell_with_results();
    ctx.memory_mut(|m| m.request_focus(script_id(id)));
    run(&ctx, &mut app, vec![]);
    run(&ctx, &mut app, vec![key(Key::F, Modifiers::COMMAND)]);
    run(&ctx, &mut app, vec![egui::Event::Text("find".into())]);
    run(&ctx, &mut app, vec![]);
    assert_eq!(finding(&ctx, &app, id), (true, true));
    let field = ctx.read_response(crate::doc_find::field_id(script_id(id))).expect("the find field drawn").rect;
    let magnifier = egui::pos2(field.left() - 10.0, field.center().y);
    let button = |pressed| egui::Event::PointerButton { pos: magnifier, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
    for events in [vec![egui::Event::PointerMoved(magnifier)], vec![button(true)], vec![button(false)], vec![]] {
        run(&ctx, &mut app, events);
    }
    assert_eq!(finding(&ctx, &app, id), (true, true), "the click left the keys in the field");
    run(&ctx, &mut app, vec![key(Key::F, Modifiers::COMMAND)]);
    run(&ctx, &mut app, vec![]);
    assert_eq!(finding(&ctx, &app, id), (true, true), "⌘F went to the script's bar");
    assert!(!results_find(&app, id) && !tree_search(&ctx), "and nowhere else");
}

/// With the find bar open, ⌘↩ runs the script, from the script and from
/// the bar's field.
#[test]
fn the_script_runs_with_the_find_bar_open() {
    let (ctx, mut app, id) = shell_with_results();
    // A run's results are new views, with new ids.
    let views = |a: &RongoApp| shell(a, id).map(|s| s.results.iter().map(|r| r.view.id).collect::<Vec<_>>()).unwrap_or_default();
    let ran_again = |before: Vec<u64>| move |a: &RongoApp| a.requests.pending.is_empty() && shell(a, id).is_some_and(|s| !s.is_running()) && !views(a).is_empty() && views(a) != before;
    ctx.memory_mut(|m| m.request_focus(script_id(id)));
    run(&ctx, &mut app, vec![]);
    run(&ctx, &mut app, vec![key(Key::F, Modifiers::COMMAND)]);
    run(&ctx, &mut app, vec![egui::Event::Text("find".into())]);
    run(&ctx, &mut app, vec![]);
    assert_eq!(finding(&ctx, &app, id), (true, true));
    let before = views(&app);
    run(&ctx, &mut app, vec![key(Key::Enter, Modifiers::COMMAND)]);
    step_until(&ctx, &mut app, "⌘↩ in the find field ran the script", ran_again(before));
    assert_eq!(finding(&ctx, &app, id), (true, true), "the bar stayed, its field focused");

    ctx.memory_mut(|m| m.request_focus(script_id(id)));
    run(&ctx, &mut app, vec![]);
    let before = views(&app);
    run(&ctx, &mut app, vec![key(Key::Enter, Modifiers::COMMAND)]);
    step_until(&ctx, &mut app, "⌘↩ in the script ran it", ran_again(before));
    assert!(finding(&ctx, &app, id).0, "the bar stayed");
    assert_eq!(shell(&app, id).unwrap().script().text, "db.getCollection(\"advertisers\").find({})\n", "⌘↩ typed nothing");
}

/// The find bar belongs to its tab: switching to another tab and back
/// finds it as it was, its needle and count.
#[test]
fn the_find_bar_stays_with_its_tab() {
    let (ctx, mut app, id) = shell_with_results();
    ctx.memory_mut(|m| m.request_focus(script_id(id)));
    run(&ctx, &mut app, vec![]);
    run(&ctx, &mut app, vec![key(Key::F, Modifiers::COMMAND)]);
    run(&ctx, &mut app, vec![egui::Event::Text("find".into())]);
    run(&ctx, &mut app, vec![]);
    let shown = |app: &RongoApp| shell(app, id).and_then(|s| s.script().find()).map(|f| f.needle().to_string());
    assert_eq!(shown(&app).as_deref(), Some("find"));
    let here = app.active_tab;
    app.active_tab = 0;
    for _ in 0..3 {
        run(&ctx, &mut app, vec![]);
    }
    app.active_tab = here;
    for _ in 0..3 {
        run(&ctx, &mut app, vec![]);
    }
    assert_eq!(shown(&app).as_deref(), Some("find"), "the bar is still open, on the same needle");
    assert!(drawn_texts(&ctx, &mut app).iter().any(|t| t == "1 of 1"), "its count shows");
}
