//! Helpers the app's tests share.

use super::*;

pub(super) fn frame_input() -> egui::RawInput {
    egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1440.0, 900.0))),
        ..Default::default()
    }
}

/// The runtime a test app's core runs on: two workers for the demo
/// server, one otherwise.
pub(super) fn runtime(mock: bool) -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().worker_threads(if mock { 2 } else { 1 }).enable_all().build().unwrap()
}

/// An app of the test's own (its own temp folder and vault, unlocked) and
/// its context; `mock`: the demo server opens.
pub(super) fn test_app(mock: bool) -> (egui::Context, RongoApp) {
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let app = RongoApp::new(&cc, runtime(mock), mock);
    (ctx, app)
}

/// A fresh app (its own temp vault, unlocked) and a second handle on the
/// same vault, as another rongo window would hold.
pub(super) fn two_windows() -> (egui::Context, RongoApp, Vault) {
    use crate::test_support::TEST_PASSWORD;
    let (ctx, app) = test_app(false);
    let other = Vault::unlock(app.vault.as_ref().unwrap().path().to_path_buf(), TEST_PASSWORD).unwrap();
    (ctx, app, other)
}

/// The whole app in a harness (its own temp folder and vault, unlocked).
pub(super) fn app_harness() -> egui_kittest::Harness<'static, RongoApp> {
    harness(egui::vec2(1200.0, 800.0), false)
}

/// The whole app in a harness of `size`; `mock`: the demo server opens.
pub(super) fn harness(size: egui::Vec2, mock: bool) -> egui_kittest::Harness<'static, RongoApp> {
    egui_kittest::Harness::builder().with_size(size).build_eframe(move |cc| RongoApp::new(cc, runtime(mock), mock))
}

/// The text shapes a frame drew.
pub(super) fn text_shapes(out: &egui::FullOutput) -> impl Iterator<Item = &egui::epaint::TextShape> {
    out.shapes.iter().filter_map(|s| match &s.shape {
        egui::Shape::Text(t) => Some(t),
        _ => None,
    })
}

/// The texts a frame drew.
pub(super) fn texts_of(out: &egui::FullOutput) -> Vec<String> {
    text_shapes(out).map(|t| t.galley.text().to_string()).collect()
}

/// Steps the harness until `done` holds (work runs on threads of its own).
pub(super) fn harness_until(h: &mut egui_kittest::Harness<'_, RongoApp>, what: &str, done: impl Fn(&RongoApp) -> bool) {
    let end = Instant::now() + WAIT;
    while !done(h.state()) {
        assert!(Instant::now() < end, "timed out waiting for: {what}");
        h.step();
        std::thread::sleep(Duration::from_millis(5));
    }
    h.run_steps(2);
}

/// A saved connection with a password (and that password in the vault).
pub(super) fn saved_with_password(app: &mut RongoApp, name: &str) -> ConnectionProfile {
    let mut p = ConnectionProfile::new(name);
    p.auth = rongo_core::profile::Auth::Password { user: "u".into(), auth_db: "admin".into(), mechanism: None };
    let vault = app.vault.clone().expect("unlocked");
    app.store.as_mut().expect("a store of the test's own").upsert(p.clone(), Some(format!("{name}'s password")), None, &vault).unwrap();
    p
}

/// A mock app whose demo connection is connected and saved in the
/// test's own store (the demo is normally unsaved; a real profile is).
pub(super) fn connected_mock_app() -> (egui::Context, RongoApp, ConnId) {
    let (ctx, mut app) = test_app(true);
    let profile = ConnectionProfile::mock();
    let cid = profile.conn_id();
    app.store.as_mut().expect("a store of the test's own").upsert(profile, None, None, app.vault.as_deref().unwrap()).unwrap();
    step_until(&ctx, &mut app, "the demo connection connects", |a| matches!(a.conns[&cid].status, Status::Connected(_)));
    (ctx, app, cid)
}

/// How long a test waits for the core: by the clock, not a count of
/// frames, which a loaded machine (the whole suite in parallel) draws
/// slowly. Generous, since it only guards against a hang.
pub(super) const WAIT: Duration = Duration::from_secs(60);

/// Step frames until `done` holds (the core answers on its own thread).
pub(super) fn step_until(ctx: &egui::Context, app: &mut RongoApp, what: &str, done: impl Fn(&RongoApp) -> bool) {
    let mut frame = eframe::Frame::_new_kittest();
    let end = Instant::now() + WAIT;
    while Instant::now() < end {
        let mut out = ctx.run_ui(frame_input(), |ui| eframe::App::ui(app, ui, &mut frame));
        out.textures_delta.clear();
        if done(app) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for: {what}");
}

/// A second mock connection, "Mock B", connected the normal way; each
/// mock connection has a demo store of its own.
pub(super) fn connect_mock_b(ctx: &egui::Context, app: &mut RongoApp) -> ConnId {
    let mut profile = ConnectionProfile::mock();
    profile.id = uuid::Uuid::new_v4();
    profile.name = "Mock B".into();
    let cid = profile.conn_id();
    app.connect(profile);
    step_until(ctx, app, "Mock B connects", |a| matches!(a.conns.get(&cid).map(|c| &c.status), Some(Status::Connected(_))));
    cid
}

/// The core's answer to `cmd` on `cid`, asked straight of it (other
/// answers that arrive meanwhile are dropped).
pub(super) fn ask_core(app: &RongoApp, cid: ConnId, cmd: Command) -> Event {
    let id = app.requests.core.send(Some(cid), cmd);
    let end = Instant::now() + WAIT;
    while Instant::now() < end {
        while let Some(r) = app.requests.core.try_recv() {
            if r.id == id {
                return r.event;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("the core never answered");
}

/// How many documents `ns` holds on `cid`, asked straight of the core.
pub(super) fn count_docs(app: &RongoApp, cid: ConnId, ns: &Namespace) -> u64 {
    match ask_core(app, cid, Command::Count { ns: ns.clone(), filter: doc! {} }) {
        Event::Count(n) => n,
        other => panic!("count answered {other:?}"),
    }
}

/// The documents of `ns` matching `filter`, asked straight of the core.
pub(super) fn find_docs(app: &RongoApp, cid: ConnId, ns: &Namespace, filter: Document) -> Vec<Document> {
    match ask_core(app, cid, Command::Find(FindSpec { filter, ..FindSpec::all(ns.clone(), 100) })) {
        Event::Page { docs, .. } => docs.into_iter().map(|d| Document::try_from(d).unwrap()).collect(),
        other => panic!("find answered {other:?}"),
    }
}

pub(super) fn toast_texts(app: &RongoApp) -> Vec<String> {
    app.toasts.iter().map(|t| t.text.clone()).collect()
}

pub(super) fn server_info() -> rongo_core::ServerInfo {
    rongo_core::ServerInfo { version: "7.0.0".into(), host: "h:27017".into(), role: "Primary".into(), replica_set: None, members: Vec::new(), user: None }
}

/// No write the app sent is waiting for its answer, whatever origin it
/// went with (a drop and a rename have their own, as does a restore).
pub(super) fn writes_done(app: &RongoApp) -> bool {
    !app.requests.pending.values().any(|s| {
        matches!(
            s.origin,
            Origin::Notify { .. } | Origin::TreeWrite { .. } | Origin::UpdateOne { .. } | Origin::Tool { .. } | Origin::Save { .. } | Origin::Copy { .. } | Origin::Dropped { .. } | Origin::Rename { .. } | Origin::History { req: HistoryReq::Restore, .. }
        )
    })
}

/// A collection of its own on `cid` holding `_id`s 1..=n.
pub(super) fn history_coll(ctx: &egui::Context, app: &mut RongoApp, cid: ConnId, name: &str, n: i32) -> Namespace {
    let ns = Namespace::new("demo-games", name);
    let docs = (1..=n).map(|i| doc! { "_id": i, "n": i, "name": format!("document {i}") }).collect();
    app.net().send(Origin::Notify { view: None, tree: None }, Some(cid), Command::InsertMany { ns: ns.clone(), docs });
    step_until(ctx, app, "the documents are inserted", writes_done);
    ns
}

/// The texts drawn by the app's next frames (a new modal is drawn from its second frame).
pub(super) fn drawn_texts(ctx: &egui::Context, app: &mut RongoApp) -> Vec<String> {
    let mut frame = eframe::Frame::_new_kittest();
    let mut texts = Vec::new();
    for _ in 0..3 {
        let mut out = ctx.run_ui(frame_input(), |ui| eframe::App::ui(app, ui, &mut frame));
        out.textures_delta.clear();
        texts = texts_of(&out);
    }
    texts
}

/// The History tab on screen.
pub(super) fn history_tab(app: &RongoApp) -> &crate::history_tab::HistoryTab {
    match &app.tabs[app.active_tab].kind {
        TabKind::History(h) => h,
        _ => panic!("the History tab is not on screen"),
    }
}

/// Opens the History tab of `ns` from the tree and waits for its
/// entries and the newest one's preview.
pub(super) fn open_history(ctx: &egui::Context, app: &mut RongoApp, cid: ConnId, ns: &Namespace) {
    app.tree_action(TreeAction::OpenHistory(cid, ns.clone()));
    step_until(ctx, app, "the entries are listed and the newest previewed", |a| matches!(&a.tabs[a.active_tab].kind, TabKind::History(h) if h.previewed().is_some()));
}

/// Deletes documents by `_id` as the table's Delete does.
pub(super) fn delete_ids(ctx: &egui::Context, app: &mut RongoApp, cid: ConnId, ns: &Namespace, ids: &[i32]) {
    app.view_action(1, ViewAction::Delete { conn: cid, ns: ns.clone(), ids: ids.iter().map(|i| Bson::Int32(*i)).collect() });
    let c = app.confirm.take().expect("deleting asks first");
    app.confirmed(c);
    step_until(ctx, app, "the delete is done", writes_done);
}

/// No conflict check, restore or comparison of a History tab is running.
pub(super) fn history_idle(app: &RongoApp) -> bool {
    !app.requests.pending.values().any(|s| matches!(s.origin, Origin::History { .. }))
}

/// Whether a History request of this kind is waiting for its answer.
pub(super) fn history_sent(app: &RongoApp, kind: HistoryReq) -> bool {
    app.requests.pending.values().any(|s| matches!(s.origin, Origin::History { req, .. } if req == kind))
}

/// Clicks the middle of the first drawn text that `pick` accepts.
pub(super) fn click_text(ctx: &egui::Context, app: &mut RongoApp, what: &str, pick: impl Fn(&str) -> bool) -> Vec<String> {
    click_text_with(ctx, app, what, egui::PointerButton::Primary, pick)
}

/// `click_text` with `button` (`Secondary`: a right-click, its menu then
/// opens). Returns what the click copied to the clipboard.
pub(super) fn click_text_with(ctx: &egui::Context, app: &mut RongoApp, what: &str, button: egui::PointerButton, pick: impl Fn(&str) -> bool) -> Vec<String> {
    let mut frame = eframe::Frame::_new_kittest();
    let mut at = None;
    // A new modal is drawn from its second frame.
    for _ in 0..3 {
        let mut out = ctx.run_ui(frame_input(), |ui| eframe::App::ui(app, ui, &mut frame));
        out.textures_delta.clear();
        at = text_shapes(&out).find(|t| pick(t.galley.text())).map(|t| t.galley.rect.translate(t.pos.to_vec2()).center());
    }
    let at = at.unwrap_or_else(|| panic!("{what} is drawn"));
    let press = |pressed| egui::Event::PointerButton { pos: at, button, pressed, modifiers: Modifiers::NONE };
    let mut copied = Vec::new();
    for events in [vec![egui::Event::PointerMoved(at)], vec![press(true)], vec![press(false)], vec![]] {
        let mut input = frame_input();
        input.events = events;
        let mut out = ctx.run_ui(input, |ui| eframe::App::ui(app, ui, &mut frame));
        out.textures_delta.clear();
        copied.extend(out.platform_output.commands.into_iter().filter_map(|c| match c {
            egui::OutputCommand::CopyText(t) => Some(t),
            _ => None,
        }));
    }
    copied
}

/// Opens Settings and waits until it has measured the history.
pub(super) fn open_settings(ctx: &egui::Context, app: &mut RongoApp) {
    app.open_settings();
    step_until(ctx, app, "the history is measured", |a| a.history_files.idle() && a.history_files.scan().is_some());
}

/// An entry of `n` deleted documents in `profile`'s history of `ns`, as
/// a recorded delete keeps it (for profiles that never connect here).
pub(super) fn saved_history(app: &RongoApp, profile: uuid::Uuid, ns: &Namespace, n: i32) {
    let mut w = app.history_store().unwrap().begin(profile, "test", ns, Kind::Delete, doc! {}).unwrap();
    let docs: Vec<_> = (1..=n).map(|i| rongo_core::bson::RawDocumentBuf::try_from(&doc! { "_id": i }).unwrap()).collect();
    w.save(&docs).unwrap();
    w.finish(false).unwrap();
}

/// A recorded delete on `ns` that is still waiting for its answer, its
/// entry still being written; `written` ends it.
pub(super) fn writing(app: &mut RongoApp, cid: ConnId, ns: &Namespace) {
    let mut sent = crate::net::Sent::new(Origin::Notify { view: None, tree: None });
    sent.conn = Some(cid);
    app.requests.pending.insert(9_999, sent);
    app.requests.unrecorded.insert(9_999, Unrecorded { cmd: Command::Delete { ns: ns.clone(), ids: vec![Bson::Int32(2)] }, name: ns.coll.clone() });
}

pub(super) fn written(app: &mut RongoApp) {
    app.requests.pending.remove(&9_999);
    app.requests.unrecorded.remove(&9_999);
}
