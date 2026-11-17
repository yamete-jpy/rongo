//! The Operations panel: what it logs, how entries end, a copy's
//! collections, an export's file, and how it shares the corner.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rongo_core::{CopyItem, CopyOptions, ExportFormat, ExportSpec, IfExists, OnDuplicateId};

use super::*;
use crate::net::LogSeed;
use crate::operations::{Answer, ChildState, ConnLabel, Entry, OpsAction, Status as OpStatus, reveal_label};
use crate::result_view::thousands;
use crate::update::Hooks;

/// Handles the core's answers one at a time until `done` holds after one,
/// so a state that lasts only from one answer to the next is seen however
/// soon the next follows (a frame handles all that have arrived).
fn handle_until(app: &mut RongoApp, what: &str, done: impl Fn(&RongoApp) -> bool) {
    let end = Instant::now() + WAIT;
    loop {
        match app.requests.core.try_recv() {
            Some(r) => {
                app.handle(r);
                if done(app) {
                    return;
                }
            }
            None => {
                assert!(Instant::now() < end, "timed out waiting for: {what}");
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
}

fn states(app: &RongoApp, id: rongo_core::RequestId) -> Vec<ChildState> {
    app.ops.get(id).expect("an entry").children.iter().map(|c| c.state.clone()).collect()
}

fn copy_cmd(to: ConnId, items: &[(Namespace, Namespace, IfExists)]) -> Command {
    let items = items.iter().map(|(src, dst, if_exists)| CopyItem { src: src.clone(), dst: dst.clone(), if_exists: *if_exists }).collect();
    Command::CopyCollections { to, items, options: CopyOptions::default() }
}

/// A write appears as it is sent, running, and ends done with its toast's
/// text as its result. Reads (the connect, the tree's listings, a find, a
/// count) never appear.
#[test]
fn a_write_is_logged_and_ends_with_its_toast() {
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, cid, "ops_clear", 5);
    let find = app.net().send(Origin::View(1, ViewReq::Page), Some(cid), Command::Find(FindSpec::all(ns.clone(), 50)));
    let count = app.net().send(Origin::View(1, ViewReq::Count), Some(cid), Command::Count { ns: ns.clone(), filter: doc! {} });
    let clear = app.net().send(Origin::Notify { view: None, tree: None }, Some(cid), Command::ClearCollection { ns: ns.clone() });
    app.log_sent();
    let e = app.ops.get(clear).expect("logged as it is sent");
    assert_eq!((e.status, e.title.as_str()), (OpStatus::Running, "Clearing demo-games.ops_clear"));
    assert_eq!(e.conns, [ConnLabel { name: ConnectionProfile::mock().name, color: None }]);
    assert!(!e.cancellable, "a clear runs to its end on the server: no ⊗");

    step_until(&ctx, &mut app, "the clear is done", |a| !a.ops.is_running(clear) && !a.requests.pending.contains_key(&find) && !a.requests.pending.contains_key(&count));
    let e = app.ops.get(clear).unwrap();
    assert_eq!(e.status, OpStatus::Done);
    assert_eq!(e.result.as_deref(), Some("Deleted 5 documents"));
    assert!(toast_texts(&app).contains(&"Deleted 5 documents".to_string()), "the toast's text: {:?}", toast_texts(&app));
    assert!(e.took.is_some());
    let titles: Vec<&str> = app.ops.entries().iter().map(|e| e.title.as_str()).collect();
    assert_eq!(titles, ["Inserting 5 documents into demo-games.ops_clear", "Clearing demo-games.ops_clear"], "writes only");
}

/// A write the server refuses ends failed, with the error as its result.
#[test]
fn a_failed_write_is_failed_with_its_error() {
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = Namespace::new("demo-games", "providers");
    let id = app.net().send(Origin::TreeWrite { conn: cid, db: ns.db.clone() }, Some(cid), Command::CreateCollection { ns });
    step_until(&ctx, &mut app, "the refusal", |a| a.ops.get(id).is_some_and(|e| e.status != OpStatus::Running));
    let e = app.ops.get(id).unwrap();
    assert_eq!(e.status, OpStatus::Failed);
    let why = e.result.clone().expect("the error");
    assert!(toast_texts(&app).contains(&why), "the toast's text: {why} / {:?}", toast_texts(&app));
    assert!(why.to_lowercase().contains("exists"), "{why}");
}

/// ⊗ in the panel stops a copy between batches: it ends stopped, its text
/// says exactly how many documents went across, the collection it was
/// copying says where it stopped (with its count) and those after it were
/// not copied. The click comes as soon as the panel draws ⊗ with the
/// copy's progress (three collections of 25,000 documents run far longer
/// than the three frames of a click), and what is asserted holds wherever
/// the stop lands.
#[test]
fn a_copy_stopped_in_the_panel_ends_stopped_with_exact_counts() {
    const ALL: u64 = 25_000;
    let (ctx, mut app, a) = connected_mock_app();
    let b = connect_mock_b(&ctx, &mut app);
    app.ops.open = true;
    let bets = Namespace::new("demo-games", "bet_logs");
    let targets: Vec<Namespace> = ["a", "b", "c"].iter().map(|c| Namespace::new("stop", *c)).collect();
    let items: Vec<_> = targets.iter().map(|t| (bets.clone(), t.clone(), IfExists::Replace)).collect();
    let id = app.net().send(Origin::Copy { to: b, db: "stop".into() }, Some(a), copy_cmd(b, &items));
    let mut frame = eframe::Frame::_new_kittest();
    let mut run = |app: &mut RongoApp, events: Vec<egui::Event>| {
        let mut input = frame_input();
        input.events = events;
        let mut out = ctx.run_ui(input, |ui| eframe::App::ui(app, ui, &mut frame));
        out.textures_delta.clear();
        out
    };
    let end = Instant::now() + WAIT;
    let at = loop {
        let out = run(&mut app, vec![]);
        let reporting = app.ops.get(id).is_some_and(|e| e.progress.is_some());
        if let Some(at) = text_shapes(&out).find(|t| t.galley.text() == icon::X_CIRCLE).map(|t| t.galley.rect.translate(t.pos.to_vec2()).center())
            && reporting
        {
            break at;
        }
        assert!(Instant::now() < end, "timed out waiting for the panel's ⊗");
        std::thread::sleep(Duration::from_millis(5));
    };
    let press = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
    for events in [vec![egui::Event::PointerMoved(at)], vec![press(true)], vec![press(false)]] {
        run(&mut app, events);
    }
    assert!(app.requests.pending.get(&id).is_none_or(|s| s.stop_asked), "⊗ asked it to stop");
    step_until(&ctx, &mut app, "the copy answers", |a| !a.ops.is_running(id));
    assert_eq!(app.ops.get(id).unwrap().status, OpStatus::Stopped);
    let counts: Vec<u64> = targets.iter().map(|t| count_docs(&app, b, t)).collect();
    let total: u64 = counts.iter().sum();
    assert!(total > 0 && total < 3 * ALL, "stopped part way: {counts:?}");
    let text = app.ops.get(id).unwrap().result.clone().unwrap();
    assert!(text.starts_with(&format!("Copy cancelled after {} documents", thousands(total))), "{text}");
    let st = states(&app, id);
    let short = counts.iter().position(|&n| n < ALL).expect("one was cut short or not copied");
    for (k, (state, n)) in st.iter().zip(&counts).enumerate() {
        match k.cmp(&short) {
            std::cmp::Ordering::Less => assert_eq!(*state, ChildState::Copied { inserted: ALL, skipped: 0, overwritten: 0, warning: None }, "{st:?}"),
            // Stopped where it was; or, stopped between two collections, the next one not begun.
            std::cmp::Ordering::Equal => assert!(*state == ChildState::Stopped { done: *n } || (*n == 0 && *state == ChildState::NotCopied), "{st:?} {counts:?}"),
            std::cmp::Ordering::Greater => assert_eq!(*state, ChildState::NotCopied, "{st:?}"),
        }
    }
}

/// A stop asked for outside ⊗ (a History tab's Cancel sends its own
/// `Command::Cancel`) is the user's: the entry says Stopping… and ends ⊘,
/// not ⚠ as a stop by a problem would.
#[test]
fn a_cancel_sent_elsewhere_is_the_users_stop() {
    let (ctx, mut app, a) = connected_mock_app();
    let b = connect_mock_b(&ctx, &mut app);
    let bets = Namespace::new("demo-games", "bet_logs");
    let items: Vec<_> = ["x", "y"].iter().map(|c| (bets.clone(), Namespace::new("cancelled", *c), IfExists::Replace)).collect();
    let id = app.net().send(Origin::Copy { to: b, db: "cancelled".into() }, Some(a), copy_cmd(b, &items));
    handle_until(&mut app, "the copy reports", |a| a.ops.get(id).is_some_and(|e| e.progress.is_some()));
    app.net().send(Origin::None, None, Command::Cancel(id));
    app.log_sent();
    assert!(app.ops.get(id).unwrap().stopping, "Stopping…");
    step_until(&ctx, &mut app, "the copy answers", |a| !a.ops.is_running(id));
    assert_eq!(app.ops.get(id).unwrap().status, OpStatus::Stopped);
}

/// A stopped answer says whether anything was written: a stop by a
/// problem is then ⚠, or ✗ when nothing was (an unconfirmed write may have
/// been applied: not nothing).
#[test]
fn a_stop_says_whether_anything_was_written() {
    use crate::app::ops::answer_of;
    let stopped = |deleted, unconfirmed| Event::Written(rongo_core::WriteOutcome { deleted, cancelled: true, unconfirmed, ..Default::default() });
    let a = answer_of(&stopped(0, false));
    assert_eq!((a.status, a.nothing_done), (OpStatus::Stopped, true));
    assert!(!answer_of(&stopped(3, false)).nothing_done);
    assert!(!answer_of(&stopped(0, true)).nothing_done);
    let restored = Event::Restored { restored: 0, skipped: 4, overwritten: 0, inserted_new: 0, recorded: None, cancelled: true, warning: Some("read-only".into()), unconfirmed: false, unconfirmed_docs: 0 };
    assert!(answer_of(&restored).nothing_done, "skipped ones are left as they were");
}

/// A copy of three collections: one line each. While the second runs,
/// the first has ended with its count and the third waits; the answer
/// gives each its count, and the one that failed its error, the copy
/// then done with something to know.
#[test]
fn a_copy_shows_each_collection() {
    let (ctx, mut app, a) = connected_mock_app();
    let b = connect_mock_b(&ctx, &mut app);
    let ns = Namespace::new;
    let items = [
        (ns("demo-games", "advertisers"), ns("copy3", "advertisers"), IfExists::Replace),
        (ns("demo-games", "providers"), ns("copy3", "providers"), IfExists::Replace),
        // Mock B's demo holds these providers already: the first _id stops it.
        (ns("demo-games", "providers"), ns("demo-games", "providers"), IfExists::Append(OnDuplicateId::Stop)),
    ];
    let id = app.net().send(Origin::Copy { to: b, db: "copy3".into() }, Some(a), copy_cmd(b, &items));
    app.log_sent();
    assert_eq!(states(&app, id), [ChildState::Waiting, ChildState::Waiting, ChildState::Waiting]);
    let names: Vec<&str> = app.ops.get(id).unwrap().conns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, [ConnectionProfile::mock().name.as_str(), "Mock B"], "both ends");

    handle_until(&mut app, "the second collection runs", |a| matches!(states(a, id)[1], ChildState::Running { .. }));
    let now = states(&app, id);
    assert_eq!(now[0], ChildState::Ended { done: 4 }, "{now:?}");
    assert_eq!(now[2], ChildState::Waiting, "{now:?}");

    handle_until(&mut app, "the copy answers", |a| !a.ops.is_running(id));
    let e = app.ops.get(id).unwrap();
    assert_eq!(e.status, OpStatus::Warning);
    let done = states(&app, id);
    assert_eq!(done[0], ChildState::Copied { inserted: 4, skipped: 0, overwritten: 0, warning: None });
    assert_eq!(done[1], ChildState::Copied { inserted: 8, skipped: 0, overwritten: 0, warning: None });
    let ChildState::Failed { error, .. } = &done[2] else { panic!("the third failed: {done:?}") };
    let toast = toast_texts(&app).into_iter().find(|t| t.starts_with("Pasted")).expect("the copy's toast");
    assert!(toast.contains(error.as_str()), "{toast} / {error}");
    assert_eq!(e.result.as_deref(), Some(toast.as_str()));
    assert!(e.children.iter().take(2).all(|c| c.detail().is_some()));
}

/// An export keeps its file: Reveal in Finder (Show in Explorer on
/// Windows) hands it to the file manager hook; a failing one says so.
#[test]
fn an_export_can_show_its_file() {
    let shown: Arc<Mutex<Vec<PathBuf>>> = Arc::default();
    let seen = shown.clone();
    let hooks = Hooks {
        reveal: Arc::new(move |p: &std::path::Path| {
            seen.lock().unwrap().push(p.to_path_buf());
            Ok(())
        }),
        ..Hooks::none()
    };
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let mut app = RongoApp::with_hooks(&cc, runtime(true), true, crate::test_support::TempDir::new(), hooks);
    let cid = ConnectionProfile::mock().conn_id();
    step_until(&ctx, &mut app, "the demo connects", |a| matches!(a.conns.get(&cid).map(|c| &c.status), Some(Status::Connected(_))));
    let path = app._temp.0.join("providers.json");
    let spec = ExportSpec { ns: Namespace::new("demo-games", "providers"), filter: doc! {}, sort: None, projection: None, format: ExportFormat::JsonArray, path: path.clone(), fields: None };
    let id = app.net().send(Origin::Tool { view: None, tree: None }, Some(cid), Command::Export(Box::new(spec)));
    step_until(&ctx, &mut app, "the export is done", |a| a.ops.get(id).is_some_and(|e| e.status != OpStatus::Running));
    let e = app.ops.get(id).unwrap();
    assert_eq!((e.status, e.path.as_ref()), (OpStatus::Done, Some(&path)));
    let toast = toast_texts(&app).into_iter().find(|t| t.contains("Exported 8 documents to")).expect("the export's toast");
    assert_eq!(e.result.as_deref(), Some(toast.as_str()));

    app.ops.open = true;
    click_text_with(&ctx, &mut app, "the export's entry", egui::PointerButton::Secondary, |t| t.starts_with("Exporting demo-games.providers"));
    click_text(&ctx, &mut app, "the menu's Reveal", |t| t.ends_with(reveal_label()));
    step_until(&ctx, &mut app, "the file manager is asked", |a| a.revealing.is_none() && !shown.lock().unwrap().is_empty());
    assert_eq!(shown.lock().unwrap().clone(), vec![path.clone()]);

    app.update.hooks.reveal = Arc::new(|_| Err("no file manager here".into()));
    app.ops_action(OpsAction::Reveal(path));
    step_until(&ctx, &mut app, "the failure is said", |a| toast_texts(a).iter().any(|t| t == "Couldn't show the file: no file manager here"));
}

/// A recorded write refused up front because its history would not fit
/// leaves no entry (nothing was written); sent again without history, it
/// gets one of its own.
#[test]
fn a_write_refused_for_its_history_leaves_no_entry() {
    let (ctx, mut app, cid) = connected_mock_app();
    let ns = history_coll(&ctx, &mut app, cid, "ops_big", 3);
    app.settings.history.max_mb = 0;
    app.view_action(1, ViewAction::Delete { conn: cid, ns: ns.clone(), ids: vec![Bson::Int32(1), Bson::Int32(2)] });
    let c = app.confirm.take().unwrap();
    app.confirmed(c);
    let refused = *app.requests.unrecorded.keys().next().expect("sent recorded");
    app.log_sent();
    assert!(app.ops.is_running(refused), "logged while it waits");
    step_until(&ctx, &mut app, "the size check answers", |a| a.confirm.is_some());
    assert!(!app.ops.has(refused), "refused before writing: no entry");

    let mut c = app.confirm.take().unwrap();
    c.input = Some(ns.coll.clone());
    app.confirmed(c);
    step_until(&ctx, &mut app, "the plain delete is done", writes_done);
    let last = app.ops.entries().last().unwrap();
    assert_eq!((last.title.as_str(), last.status, last.result.as_deref()), ("Deleting 2 documents from demo-games.ops_big", OpStatus::Done, Some("Deleted 2 documents")));
}

/// With the panel showing, the corner leaves out what the panel lists but
/// still shows the rest (connecting, say); folded, or with the sidebar
/// hidden, the corner is as it was.
#[test]
fn the_corner_leaves_the_panels_work_to_the_panel() {
    let (_ctx, mut app) = test_app(false);
    let now = Instant::now();
    let ago = |s| now - Duration::from_secs(s);
    let cid = ConnectionProfile::mock().conn_id();
    let clearing = Sent { what: Some("Clearing d.c".into()), started: ago(3), log: Some(LogSeed::default()), ..Sent::new(Origin::Notify { view: None, tree: None }) };
    let connecting = Sent { what: Some("Connecting".into()), started: ago(2), ..Sent::new(Origin::Connect(cid)) };
    app.requests.pending.insert(1, clearing);
    app.requests.pending.insert(2, connecting);
    let corner = |app: &RongoApp| app.operations(now).iter().map(|o| o.text.clone()).collect::<Vec<_>>();
    assert_eq!(corner(&app), ["Clearing d.c", "Connecting"], "folded: as before");
    app.ops.open = true;
    assert_eq!(corner(&app), ["Connecting"], "the panel shows the clear");
    app.sidebar.open = false;
    assert_eq!(corner(&app), ["Clearing d.c", "Connecting"], "the sidebar hidden: as before");
}

/// While the sidebar shows, the corner floats above the Operations panel's
/// header: with a logged write in the corner, a click on the folded header
/// still unfolds the panel (the corner once covered it).
#[test]
fn the_header_can_be_clicked_while_the_corner_shows() {
    let (ctx, mut app) = test_app(false);
    let sent = Sent { what: Some("Copying 3 collections".into()), started: Instant::now() - Duration::from_secs(2), log: Some(LogSeed::default()), ..Sent::new(Origin::Notify { view: None, tree: None }) };
    app.requests.pending.insert(9_999, sent);
    let out = (0..3).map(|_| {
        let mut frame = eframe::Frame::_new_kittest();
        let mut out = ctx.run_ui(frame_input(), |ui| eframe::App::ui(&mut app, ui, &mut frame));
        out.textures_delta.clear();
        out
    }).last().unwrap();
    let rect = |pick: &dyn Fn(&str) -> bool| text_shapes(&out).find(|t| pick(t.galley.text())).map(|t| t.galley.rect.translate(t.pos.to_vec2()));
    let corner = rect(&|t| t.starts_with("Copying 3 collections")).expect("the corner shows the write");
    let header = rect(&|t| t.contains("Operations · 1 running")).expect("the folded header");
    assert!(corner.bottom() < header.top(), "the corner above the header: {corner:?} {header:?}");
    click_text(&ctx, &mut app, "the header", |t| t.contains("Operations · 1 running"));
    assert!(app.ops.open, "unfolded");
    // At 760 points wide, the corner stays on screen.
    let narrow = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(760.0, 720.0))), ..Default::default() };
    app.ops.open = false;
    let mut frame = eframe::Frame::_new_kittest();
    let mut out = ctx.run_ui(narrow.clone(), |ui| eframe::App::ui(&mut app, ui, &mut frame));
    out.textures_delta.clear();
    let mut out = ctx.run_ui(narrow, |ui| eframe::App::ui(&mut app, ui, &mut frame));
    out.textures_delta.clear();
    let corner = text_shapes(&out).find(|t| t.galley.text().starts_with("Copying 3 collections")).map(|t| t.galley.rect.translate(t.pos.to_vec2())).expect("the corner");
    assert!(corner.left() >= 0.0 && corner.right() <= 760.0 && corner.top() >= 0.0 && corner.bottom() <= 720.0, "{corner:?}");
}

/// With a list long enough to scroll, ⊗ stays clear of the scrollbar: a
/// click at its centre stops the request (the bar, floating over the
/// list's right edge, once took clicks on its right half).
#[test]
fn the_stop_button_is_clear_of_the_scrollbar() {
    let (ctx, mut app) = test_app(false);
    let now = Instant::now();
    let at = rongo_core::bson::DateTime::now();
    for id in 1..=60 {
        app.ops.add(Entry::new(id, format!("Clearing shop.c{id}"), Vec::new(), at, now, false, &[]));
        app.ops.finish(id, Answer { status: OpStatus::Done, nothing_done: false, fallback: Some("Deleted 12 documents".into()), path: None, copied: None }, now);
    }
    let sent = Sent { what: Some("Copying 3 collections".into()), started: now, log: Some(LogSeed::default()), cancellable: true, ..Sent::new(Origin::Notify { view: None, tree: None }) };
    app.requests.pending.insert(9_999, sent);
    app.ops.open = true;
    let out = (0..3).map(|_| {
        let mut frame = eframe::Frame::_new_kittest();
        let mut out = ctx.run_ui(frame_input(), |ui| eframe::App::ui(&mut app, ui, &mut frame));
        out.textures_delta.clear();
        out
    }).last().unwrap();
    let stop = text_shapes(&out).find(|t| t.galley.text() == icon::X_CIRCLE).map(|t| t.galley.rect.translate(t.pos.to_vec2())).expect("⊗ is drawn");
    let bar = ctx.global_style().spacing.scroll.bar_width;
    assert!(stop.right() <= app.sidebar_rect.right() - 6.0 - bar, "⊗ left of the bar: {stop:?} {:?}", app.sidebar_rect);
    click_text(&ctx, &mut app, "the copy's ⊗", |t| t == icon::X_CIRCLE);
    assert!(app.ops.get(9_999).unwrap().stopping && app.requests.pending[&9_999].stop_asked, "stopped by a click at its centre");
}

/// Entries for the panel's drawing tests: a running copy of two
/// collections, a done write, a failed one with a long error.
fn some_entries(app: &mut RongoApp) {
    let now = Instant::now();
    let conn = |name: &str| ConnLabel { name: name.into(), color: theme::conn_color(Some("green")) };
    let items = [(Namespace::new("shop", "users"), Namespace::new("shop", "users_copy")), (Namespace::new("shop", "orders"), Namespace::new("uat", "orders"))];
    let at = rongo_core::bson::DateTime::now();
    app.ops.add(Entry::new(1, "Clearing shop.carts".into(), vec![conn("SIT")], at, now, false, &[]));
    app.ops.finish(1, Answer { status: OpStatus::Done, nothing_done: false, fallback: Some("Deleted 12 documents".into()), path: None, copied: None }, now);
    app.ops.add(Entry::new(2, "Dropping shop.old".into(), vec![conn("SIT")], at, now, false, &[]));
    let long = "not authorized on shop to execute command { drop: \"old\" }; ".repeat(8);
    app.ops.finish(2, Answer { status: OpStatus::Failed, nothing_done: true, fallback: Some(long), path: None, copied: None }, now);
    app.ops.add(Entry::new(3, "Copying 2 collections".into(), vec![conn("SIT"), conn("UAT")], at, now, true, &items));
    app.ops.progress(3, 1000, Some(2500), "shop.users_copy (1/2)", Some(0), now);
}

/// The panel as the UI tour shows it: a copy of three collections running
/// (one done, one running, one waiting), a Duplicate done, an import done
/// with something to know, and a drop that failed. Ids far past the
/// tour's own requests.
pub(super) fn tour_entries(app: &mut RongoApp) {
    let now = Instant::now();
    let ago = |s: u64| now - Duration::from_secs(s);
    let wall = |s: i64| rongo_core::bson::DateTime::from_millis(rongo_core::bson::DateTime::now().timestamp_millis() - s * 1000);
    let conn = |name: &str, color: &str| ConnLabel { name: name.into(), color: theme::conn_color(Some(color)) };
    let ns = Namespace::new;
    let base = 9_000_000;
    let ok = |status, text: &str| Answer { status, nothing_done: false, fallback: Some(text.into()), path: None, copied: None };
    app.ops.add(Entry::new(base + 1, "Dropping database legacy".into(), vec![conn("PROD", "red")], wall(340), ago(340), false, &[]));
    app.ops.finish(base + 1, ok(OpStatus::Failed, "not authorized on legacy to execute command { dropDatabase: 1 }"), ago(339));
    app.ops.add(Entry::new(base + 2, "Importing into shop.orders".into(), vec![conn("UAT", "orange")], wall(200), ago(200), true, &[]));
    app.ops.finish(base + 2, ok(OpStatus::Warning, "Import finished: inserted 9,412 documents, skipped 88 with an _id already in the collection"), ago(188));
    app.ops.add(Entry::new(base + 3, "Duplicating shop.users".into(), vec![conn("SIT", "green")], wall(95), ago(95), true, &[]));
    app.ops.finish(base + 3, ok(OpStatus::Done, "Duplicated shop.users to users_copy"), ago(91));
    let items = [(ns("shop", "users"), ns("shop", "users")), (ns("shop", "orders"), ns("shop", "orders")), (ns("shop", "carts"), ns("shop", "carts"))];
    app.ops.add(Entry::new(base + 4, "Copying 3 collections".into(), vec![conn("SIT", "green"), conn("UAT", "orange")], wall(14), ago(14), true, &items));
    app.ops.progress(base + 4, 1200, Some(1200), "shop.users (1/3)", Some(0), ago(14));
    app.ops.progress(base + 4, 18_000, Some(52_310), "shop.orders (2/3)", Some(1), ago(9));
}

/// Unfolded, the panel lists the entries under the tree: the running copy
/// first with its collections, then the finished ones newest first; a
/// long result wraps whole, never cut, and nothing pushes the times past
/// the sidebar's edge. Folded, only its header shows.
#[test]
fn the_panel_lists_entries_under_the_tree() {
    let (ctx, mut app) = test_app(false);
    some_entries(&mut app);
    let mut frame = eframe::Frame::_new_kittest();
    let mut draw = |app: &mut RongoApp| {
        let mut out = ctx.run_ui(frame_input(), |ui| eframe::App::ui(app, ui, &mut frame));
        out.textures_delta.clear();
        out
    };
    draw(&mut app);
    let out = draw(&mut app);
    let texts = texts_of(&out);
    assert!(texts.iter().any(|t| t.contains("Operations · 1 running")), "{texts:?}");
    assert!(!texts.iter().any(|t| t.starts_with("Copying 2 collections")), "folded: no entries");

    app.ops.open = true;
    app.ops.height = 600.0;
    // The copy moves on to its second collection.
    app.ops.progress(3, 40, Some(80), "uat.orders (2/2)", Some(1), Instant::now());
    draw(&mut app);
    let out = draw(&mut app);
    let glyphs = |g: &str| text_shapes(&out).filter(|t| t.galley.text() == g).count();
    assert_eq!(glyphs(icon::CHECK), 1, "only the clear's ✓: the copy's first collection waits for the copy's answer");
    assert_eq!(glyphs(icon::CIRCLE), 1, "that collection ended, its outcome pending");
    let shapes: Vec<_> = text_shapes(&out).map(|t| (t.galley.text().to_string(), t.galley.rect.translate(t.pos.to_vec2()), t.galley.elided)).collect();
    let find = |s: &str| shapes.iter().find(|(t, ..)| t.starts_with(s)).unwrap_or_else(|| panic!("{s} is drawn: {shapes:?}")).clone();
    let (copy, clear, drop) = (find("Copying 2 collections"), find("Clearing shop.carts"), find("Dropping shop.old"));
    assert!(copy.1.top() < drop.1.top() && drop.1.top() < clear.1.top(), "running first, then newest first");
    let arrow = |s: &str| s.replace('→', icon::ARROW_RIGHT);
    assert!(find(&arrow("shop.users → users_copy")).1.top() > copy.1.top(), "its collections under it");
    find(&arrow("shop.orders → uat.orders"));
    find(&arrow("SIT → UAT  uat.orders (2/2) 40 / 80"));
    find("1,000 documents");
    let (error, rect, elided) = find("SIT  not authorized on shop");
    assert!(!elided && error.ends_with("}; ") && rect.height() > 40.0, "wrapped whole: {rect:?}");
    let sidebar = app.sidebar_rect;
    assert!(copy.1.top() > sidebar.top() + 200.0, "under the tree: {copy:?}");
    let inside = shapes.iter().filter(|(_, r, _)| r.top() >= sidebar.top() && r.left() < sidebar.right());
    assert!(inside.clone().all(|(_, r, _)| r.right() <= sidebar.right() + 1.0), "nothing past the sidebar's edge: {:?}", inside.collect::<Vec<_>>());
}

/// Clear Finished keeps the running entries.
#[test]
fn clear_finished_keeps_what_runs() {
    let (ctx, mut app) = test_app(false);
    some_entries(&mut app);
    app.ops.open = true;
    app.ops.height = 600.0;
    // Right-click → Remove takes one finished entry; a running one offers no Remove.
    click_text_with(&ctx, &mut app, "the clear's entry", egui::PointerButton::Secondary, |t| t == "Clearing shop.carts");
    click_text(&ctx, &mut app, "Remove", |t| t.ends_with("Remove"));
    assert_eq!(app.ops.entries().iter().map(|e| e.id).collect::<Vec<_>>(), [2, 3]);
    click_text_with(&ctx, &mut app, "the copy's entry", egui::PointerButton::Secondary, |t| t == "Copying 2 collections");
    let menu = drawn_texts(&ctx, &mut app);
    assert!(menu.iter().any(|t| t.ends_with("  Copy")), "its menu opened: {menu:?}");
    assert!(!menu.iter().any(|t| t.ends_with("Remove")), "nothing to remove while it runs");
    click_text(&ctx, &mut app, "Clear Finished", |t| t == icon::ERASER);
    assert_eq!(app.ops.entries().iter().map(|e| e.id).collect::<Vec<_>>(), [3]);
}

/// A click on the header folds and unfolds the panel.
#[test]
fn the_header_folds_the_panel() {
    let (ctx, mut app) = test_app(false);
    some_entries(&mut app);
    click_text(&ctx, &mut app, "the header", |t| t.contains("Operations · 1 running"));
    assert!(app.ops.open);
    assert!(drawn_texts(&ctx, &mut app).iter().any(|t| t == "Copying 2 collections"), "unfolded: the entries");
    click_text(&ctx, &mut app, "the header", |t| t.contains("Operations · 1 running"));
    assert!(!app.ops.open);
}

/// Dragging the handle above the header makes the panel taller, up to
/// its share of the sidebar.
#[test]
fn the_handle_sets_the_panels_height() {
    let (ctx, mut app) = test_app(false);
    app.ops.open = true;
    let mut frame = eframe::Frame::_new_kittest();
    let mut run = |app: &mut RongoApp, events: Vec<egui::Event>| {
        let mut input = frame_input();
        input.events = events;
        let mut out = ctx.run_ui(input, |ui| eframe::App::ui(app, ui, &mut frame));
        out.textures_delta.clear();
        out
    };
    run(&mut app, vec![]);
    let out = run(&mut app, vec![]);
    let header = text_shapes(&out).find(|t| t.galley.text().contains("Operations")).map(|t| t.galley.rect.translate(t.pos.to_vec2())).expect("the header");
    // The handle is the strip just above the header.
    let at = egui::pos2(header.center().x + 60.0, header.top() - 8.0);
    let to = at - egui::vec2(0.0, 100.0);
    let button = |pos, pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
    for events in [vec![egui::Event::PointerMoved(at)], vec![button(at, true)], vec![egui::Event::PointerMoved(at - egui::vec2(0.0, 50.0))], vec![egui::Event::PointerMoved(to)], vec![button(to, false)], vec![]] {
        run(&mut app, events);
    }
    assert!((app.ops.height - (crate::operations::DEFAULT_HEIGHT + 100.0)).abs() < 1.0, "{}", app.ops.height);
}
