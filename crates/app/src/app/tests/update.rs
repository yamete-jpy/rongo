//! The update check at launch, and its banner. GitHub and the browser are
//! stubs here: tests never touch the network or open a page.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use super::*;
use crate::test_support::TempDir;
use crate::update::{self, Hooks, LATEST_PAGE, Version};

/// A stub GitHub that counts the requests, and a browser that keeps what
/// it was asked to open.
struct Stub {
    asked: Arc<AtomicUsize>,
    opened: Arc<Mutex<Vec<String>>>,
}

impl Stub {
    fn asked(&self) -> usize {
        self.asked.load(Ordering::SeqCst)
    }

    fn opened(&self) -> Vec<String> {
        self.opened.lock().unwrap().clone()
    }
}

/// Hooks answering `answer` to every request; the browser fails with
/// `open_fails` when given.
fn github_with(answer: Result<String, String>, open_fails: Option<&'static str>) -> (Hooks, Stub) {
    let asked = Arc::new(AtomicUsize::new(0));
    let opened = Arc::new(Mutex::new(Vec::new()));
    let (count, keep) = (asked.clone(), opened.clone());
    let fetch: update::Fetch = Arc::new(move || {
        count.fetch_add(1, Ordering::SeqCst);
        answer.clone().map(String::into_bytes)
    });
    let open: update::Open = Arc::new(move |url: &str| {
        keep.lock().unwrap().push(url.to_string());
        open_fails.map_or(Ok(()), |e| Err(e.to_string()))
    });
    (Hooks { fetch: Some(fetch), open }, Stub { asked, opened })
}

fn github(answer: Result<String, String>) -> (Hooks, Stub) {
    github_with(answer, None)
}

/// GitHub's answer for a release of `v`, its page at `url`.
fn answer_at(v: Version, url: &str) -> String {
    serde_json::json!({ "tag_name": format!("v{v}"), "html_url": url, "name": format!("rongo {v}"), "assets": [] }).to_string()
}

fn page(v: Version) -> String {
    format!("https://github.com/yamete-jpy/rongo/releases/tag/v{v}")
}

fn answer(v: Version) -> String {
    answer_at(v, &page(v))
}

/// The running version with `n` added to its patch number.
fn newer(n: u64) -> Version {
    let r = Version::running().unwrap();
    Version { patch: r.patch + n, ..r }
}

/// An app on `temp` (a settings file the test wrote, say) with `hooks`.
fn app_with(temp: TempDir, hooks: Hooks) -> (egui::Context, RongoApp) {
    let ctx = egui::Context::default();
    let cc = eframe::CreationContext::_new_kittest(ctx.clone());
    let app = RongoApp::with_hooks(&cc, runtime(false), false, temp, hooks);
    (ctx, app)
}

/// A folder whose settings.json is `json`.
fn folder_with(json: &serde_json::Value) -> TempDir {
    let temp = TempDir::new();
    std::fs::write(temp.0.join("settings.json"), json.to_string()).unwrap();
    temp
}

/// The check's answer taken in.
fn answered(ctx: &egui::Context, app: &mut RongoApp) {
    step_until(ctx, app, "the update check answers", |a| a.update.job.is_none());
}

/// settings.json as it is on disk (null when there is none).
fn on_disk(app: &RongoApp) -> serde_json::Value {
    match std::fs::read(app.settings_path.as_ref().unwrap()) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => serde_json::Value::Null,
        Err(e) => panic!("settings.json: {e}"),
    }
}

/// `last_update_check` on disk, in ms.
fn checked_at(app: &RongoApp) -> Option<i64> {
    on_disk(app)["last_update_check"].as_str().map(|t| rongo_core::bson::DateTime::parse_rfc3339_str(t).unwrap().timestamp_millis())
}

/// RFC 3339 text for `ms_ago` milliseconds before now.
fn ago(ms_ago: i64) -> String {
    rongo_core::bson::DateTime::from_millis(rongo_core::bson::DateTime::now().timestamp_millis() - ms_ago).try_to_rfc3339_string().unwrap()
}

const HOUR: i64 = 60 * 60 * 1000;

fn banner_shown(texts: &[String], v: Version) -> bool {
    texts.iter().any(|t| *t == update::available(v))
}

/// Test apps never ask GitHub: the app a test builds the usual way has no
/// request to make, so it starts no check.
#[test]
fn test_apps_never_ask_github() {
    let (ctx, mut app) = test_app(false);
    assert!(app.update.hooks.fetch.is_none());
    drawn_texts(&ctx, &mut app);
    assert!(app.update.job.is_none() && app.update.found.is_none());
}

/// At launch rongo asks GitHub once, in the background, and when the
/// latest release is newer than this one, a banner under the toolbar
/// names it. When it asked is saved, so the next launch today doesn't.
#[test]
fn a_newer_release_shows_a_banner_under_the_toolbar() {
    let (hooks, stub) = github(Ok(answer(newer(1))));
    let before = rongo_core::bson::DateTime::now().timestamp_millis();
    let (ctx, mut app) = app_with(TempDir::new(), hooks);
    answered(&ctx, &mut app);
    assert_eq!(stub.asked(), 1);
    assert_eq!(app.update.found.as_ref().map(|r| (r.version, r.url.clone())), Some((newer(1), page(newer(1)))));
    let at = checked_at(&app).expect("when it asked is saved");
    assert!(at >= before - 1000 && at <= rongo_core::bson::DateTime::now().timestamp_millis(), "{at}");

    let mut frame = eframe::Frame::_new_kittest();
    let mut out = ctx.run_ui(frame_input(), |ui| eframe::App::ui(&mut app, ui, &mut frame));
    out.textures_delta.clear();
    let mut out = ctx.run_ui(frame_input(), |ui| eframe::App::ui(&mut app, ui, &mut frame));
    out.textures_delta.clear();
    let y = |want: &str| text_shapes(&out).find(|t| t.galley.text() == want).map(|t| t.pos.y).unwrap_or_else(|| panic!("{want} is drawn: {:?}", texts_of(&out)));
    let message = update::available(newer(1));
    assert!(y(&format!("rongo {}", env!("CARGO_PKG_VERSION"))) < y(&message), "under the toolbar");
    assert!(y(&message) < y("A MongoDB client in Rust"), "above the tabs");
    for want in ["Open Download Page", "Later"] {
        y(want);
    }
    let brew = texts_of(&out).iter().any(|t| t.contains(update::BREW_UPGRADE));
    assert_eq!(brew, cfg!(target_os = "macos"), "the Homebrew line on macOS only");
    for _ in 0..5 {
        drawn_texts(&ctx, &mut app);
    }
    assert_eq!(stub.asked(), 1, "asked once");
    assert!(app.toasts.is_empty());
}

/// The check starts at launch, behind the launch dialog, and is taken in
/// there; the banner waits for the main window, so it never covers the
/// dialog.
#[test]
fn the_banner_waits_for_the_unlock() {
    let (hooks, stub) = github(Ok(answer(newer(1))));
    let (ctx, mut app) = app_with(TempDir::new(), hooks);
    let vault = app.vault.take();
    let path = vault.as_ref().unwrap().path().to_path_buf();
    app.unlock = Some(UnlockDialog::new(path, false, Vec::new()));
    answered(&ctx, &mut app);
    assert_eq!(stub.asked(), 1);
    assert!(app.update.found.is_some(), "taken in behind the dialog");
    let texts = drawn_texts(&ctx, &mut app);
    assert!(texts.iter().any(|t| t.contains("Unlock rongo")), "{texts:?}");
    assert!(!banner_shown(&texts, newer(1)), "{texts:?}");

    app.vault = vault;
    app.unlock = None;
    assert!(banner_shown(&drawn_texts(&ctx, &mut app), newer(1)));
}

/// Later hides the banner and saves that version: no banner for it at
/// later launches, but there is one once a newer version is out.
#[test]
fn later_hides_the_banner_until_a_newer_release() {
    let (hooks, _stub) = github(Ok(answer(newer(1))));
    let (ctx, mut app) = app_with(TempDir::new(), hooks);
    answered(&ctx, &mut app);
    click_text(&ctx, &mut app, "Later", |t| t == "Later");
    assert!(app.update.found.is_none());
    assert!(!banner_shown(&drawn_texts(&ctx, &mut app), newer(1)));
    assert_eq!(app.settings.skipped_version, Some(newer(1)));
    assert_eq!(on_disk(&app)["skipped_version"], newer(1).to_string());

    // The next launches, a day later: the same release, then a newer one.
    let mut saved = on_disk(&app);
    saved["last_update_check"] = ago(25 * HOUR).into();
    for (latest, shown) in [(newer(1), false), (newer(2), true)] {
        let (hooks, stub) = github(Ok(answer(latest)));
        let (ctx, mut app) = app_with(folder_with(&saved), hooks);
        answered(&ctx, &mut app);
        assert_eq!(stub.asked(), 1);
        assert_eq!(banner_shown(&drawn_texts(&ctx, &mut app), latest), shown, "{latest}");
    }
}

/// Open Download Page opens the release's page in the browser, and only a
/// page of the repository's releases: anything else in the answer opens
/// the latest release's page. A browser that can't open it is said, with
/// the address.
#[test]
fn open_download_page_opens_only_the_release_page() {
    for (url, opens) in [(page(newer(1)), page(newer(1))), ("https://evil.example/rongo.dmg".to_string(), LATEST_PAGE.to_string())] {
        let (hooks, stub) = github(Ok(answer_at(newer(1), &url)));
        let (ctx, mut app) = app_with(TempDir::new(), hooks);
        answered(&ctx, &mut app);
        click_text(&ctx, &mut app, "Open Download Page", |t| t == "Open Download Page");
        let opened = stub.opened.clone();
        step_until(&ctx, &mut app, "the page opens", move |a| !opened.lock().unwrap().is_empty() && a.update.opening.is_none());
        assert_eq!(stub.opened(), [opens]);
        assert!(app.toasts.is_empty());
        assert!(banner_shown(&drawn_texts(&ctx, &mut app), newer(1)), "the banner stays until Later");
    }

    let (hooks, stub) = github_with(Ok(answer(newer(1))), Some("no browser"));
    let (ctx, mut app) = app_with(TempDir::new(), hooks);
    answered(&ctx, &mut app);
    click_text(&ctx, &mut app, "Open Download Page", |t| t == "Open Download Page");
    step_until(&ctx, &mut app, "the failure is said", |a| !a.toasts.is_empty());
    assert_eq!(stub.opened(), [page(newer(1))]);
    let toasts = toast_texts(&app);
    assert!(toasts[0].contains("no browser") && toasts[0].contains(&page(newer(1))), "{toasts:?}");
}

/// Turned off in Settings, rongo never asks (nothing is written either);
/// turning it off hides a banner already shown.
#[test]
fn turned_off_it_never_asks() {
    let json = serde_json::json!({ "check_updates": false });
    let (hooks, stub) = github(Ok(answer(newer(1))));
    let (ctx, mut app) = app_with(folder_with(&json), hooks);
    assert!(!app.settings.check_updates);
    for _ in 0..5 {
        drawn_texts(&ctx, &mut app);
    }
    assert_eq!(stub.asked(), 0, "no request at all");
    assert!(app.update.job.is_none());
    assert_eq!(on_disk(&app), json, "nothing written");

    let (hooks, _stub) = github(Ok(answer(newer(1))));
    let (ctx, mut app) = app_with(TempDir::new(), hooks);
    answered(&ctx, &mut app);
    assert!(banner_shown(&drawn_texts(&ctx, &mut app), newer(1)));
    let off = Settings { check_updates: false, ..app.settings.clone() };
    app.settings_action(SettingsAction::Save(off));
    assert!(!banner_shown(&drawn_texts(&ctx, &mut app), newer(1)), "hidden once turned off");
}

/// Within a day of the last request rongo doesn't ask again; a day or
/// more after, it does and saves the new time.
#[test]
fn it_asks_at_most_once_a_day() {
    // A last time an hour ahead: the clock was turned back since.
    for (last, asks) in [(HOUR, false), (23 * HOUR, false), (25 * HOUR, true), (-HOUR, true)] {
        let json = serde_json::json!({ "last_update_check": ago(last) });
        let (hooks, stub) = github(Ok(answer(newer(1))));
        let before = rongo_core::bson::DateTime::now().timestamp_millis();
        let (ctx, mut app) = app_with(folder_with(&json), hooks);
        for _ in 0..3 {
            drawn_texts(&ctx, &mut app);
        }
        answered(&ctx, &mut app);
        assert_eq!(stub.asked(), usize::from(asks), "last asked {last} ms ago");
        if asks {
            let at = checked_at(&app).unwrap();
            assert!(at >= before - 1000 && at <= rongo_core::bson::DateTime::now().timestamp_millis(), "the new time is saved, not the one ahead: {at}");
            assert!(app.update.found.is_some());
            // The next launch, at once: nothing stuck, it doesn't ask again.
            let (hooks, again) = github(Ok(answer(newer(1))));
            let (ctx, mut next) = app_with(folder_with(&on_disk(&app)), hooks);
            drawn_texts(&ctx, &mut next);
            answered(&ctx, &mut next);
            assert_eq!(again.asked(), 0, "last asked {last} ms ago: asked once");
        } else {
            assert_eq!(on_disk(&app), json, "nothing written");
            assert!(app.update.found.is_none());
        }
    }
}

/// A newer release found once is remembered: the banner shows at every
/// launch, without asking GitHub, also on a day the check fails, until
/// Later hides it or rongo is that version or newer. A newer answer
/// replaces it.
#[test]
fn a_found_update_shows_at_every_launch_until_later_or_an_update() {
    // Found at the first launch, remembered for the next one the same day.
    let (hooks, _stub) = github(Ok(answer(newer(1))));
    let (ctx, mut app) = app_with(TempDir::new(), hooks);
    answered(&ctx, &mut app);
    assert_eq!(app.settings.latest_version, Some(newer(1)));
    assert_eq!(on_disk(&app)["latest_version"], newer(1).to_string());
    let (hooks, stub) = github(Ok(answer(newer(1))));
    let (ctx, mut next) = app_with(folder_with(&on_disk(&app)), hooks);
    assert!(banner_shown(&drawn_texts(&ctx, &mut next), newer(1)), "the same day");
    assert_eq!(stub.asked(), 0, "without asking");
    assert_eq!(next.update.found.as_ref().map(|r| r.url.clone()), Some(page(newer(1))), "its release's page");

    let remembered = |latest: Version, skipped: Option<Version>, last: i64| {
        let mut json = serde_json::json!({ "latest_version": latest.to_string(), "last_update_check": ago(last) });
        if let Some(s) = skipped {
            json["skipped_version"] = s.to_string().into();
        }
        json
    };
    let running = Version::running().unwrap();
    for (why, json, reply, shown) in [
        ("a day later, offline", remembered(newer(1), None, 25 * HOUR), Err("offline".to_string()), Some(newer(1))),
        ("hidden by Later", remembered(newer(1), Some(newer(1)), HOUR), Ok(answer(newer(1))), None),
        ("updated to it", remembered(running, None, HOUR), Ok(answer(running)), None),
        ("a newer answer replaces it", remembered(newer(1), None, 25 * HOUR), Ok(answer(newer(2))), Some(newer(2))),
        ("a newer answer, after Later", remembered(newer(1), Some(newer(1)), 25 * HOUR), Ok(answer(newer(2))), Some(newer(2))),
    ] {
        let (hooks, _stub) = github(reply);
        let (ctx, mut app) = app_with(folder_with(&json), hooks);
        answered(&ctx, &mut app);
        let texts = drawn_texts(&ctx, &mut app);
        assert_eq!(app.update.found.as_ref().map(|r| r.version), shown, "{why}");
        for v in [newer(1), newer(2)] {
            assert_eq!(banner_shown(&texts, v), shown == Some(v), "{why}: {texts:?}");
        }
        if shown == Some(newer(2)) {
            assert_eq!(on_disk(&app)["latest_version"], newer(2).to_string(), "{why}");
        }
    }
}

/// The launch check saves when it asked, and what it found, on its own
/// thread: no settings write holds up a frame. The file is written before
/// the app has drawn a single frame to take the answer in.
#[test]
fn the_launch_check_saves_off_the_ui_thread() {
    let (hooks, _stub) = github(Ok(answer(newer(1))));
    let (_ctx, app) = app_with(TempDir::new(), hooks);
    let path = app.settings_path.clone().unwrap();
    let end = Instant::now() + WAIT;
    while Settings::read(&path).ok().flatten().is_none_or(|s| s.latest_version.is_none()) {
        assert!(Instant::now() < end, "timed out waiting for the check to save");
        std::thread::sleep(Duration::from_millis(5));
    }
    let saved = Settings::read(&path).unwrap().unwrap();
    assert!(saved.last_update_check.is_some() && saved.latest_version == Some(newer(1)));
    assert!(app.update.job.is_some(), "no frame took the answer in: the UI thread wrote nothing");
}

/// Offline, blocked, rate-limited, or an answer it doesn't expect: no
/// banner and no message, and no time saved: it isn't today's check, so
/// the next launch asks again. An answer that read (the same version, say)
/// is today's check.
#[test]
fn a_failed_check_shows_and_says_nothing() {
    let older = Version::running().unwrap();
    for (why, reply) in [
        ("offline", Err("Connection refused".to_string())),
        ("rate-limited", Err("http status: 403".to_string())),
        ("not JSON", Ok("<html>blocked</html>".to_string())),
        ("no tag", Ok(r#"{"message": "Not Found"}"#.to_string())),
        ("a tag that isn't a version", Ok(serde_json::json!({ "tag_name": "nightly" }).to_string())),
        ("the same version", Ok(answer(older))),
        ("too large", Ok(format!(r#"{{"tag_name": "v{}", "body": "{}"}}"#, newer(1), "x".repeat(update::MAX_BODY)))),
    ] {
        let (hooks, stub) = github(reply);
        let (ctx, mut app) = app_with(TempDir::new(), hooks);
        answered(&ctx, &mut app);
        assert_eq!(stub.asked(), 1, "{why}");
        assert!(app.update.found.is_none(), "{why}");
        let texts = drawn_texts(&ctx, &mut app);
        assert!(!texts.iter().any(|t| t.contains("is available")), "{why}: {texts:?}");
        assert!(app.toasts.is_empty(), "{why}: nothing said");
        let read = why == "the same version";
        assert_eq!(checked_at(&app).is_some(), read, "{why}: only an answer that read counts as today's request");
        assert_eq!(app.settings.last_update_check.is_some(), read, "{why}: nor in memory");
    }
}

/// A launch before the network is up leaves no check time, in the file or
/// (so a later Save in Settings can't write one) in memory; the next launch
/// asks again.
#[test]
fn a_failed_check_is_asked_again_at_the_next_launch() {
    let (hooks, stub) = github(Err("Connection refused".to_string()));
    let (ctx, mut app) = app_with(TempDir::new(), hooks);
    answered(&ctx, &mut app);
    assert_eq!(stub.asked(), 1);
    app.settings_action(SettingsAction::Save(Settings { page_size: 75, ..app.settings.clone() }));
    assert_eq!(on_disk(&app)["page_size"].as_i64(), Some(75));
    assert_eq!(checked_at(&app), None, "a Save in Settings writes no check time either");
    let (hooks, again) = github(Ok(answer(newer(1))));
    let (ctx, mut next) = app_with(folder_with(&on_disk(&app)), hooks);
    drawn_texts(&ctx, &mut next);
    answered(&ctx, &mut next);
    assert_eq!(again.asked(), 1, "asked again");
    assert!(checked_at(&next).is_some() && next.update.found.is_some());
}

/// Saving Settings keeps what the update check saved meanwhile: the
/// dialog, opened before Later, doesn't bring the banner back.
#[test]
fn saving_settings_keeps_what_the_update_check_saved() {
    let (hooks, _stub) = github(Ok(answer(newer(1))));
    let (ctx, mut app) = app_with(TempDir::new(), hooks);
    answered(&ctx, &mut app);
    // As a dialog opened before the answer and Later has them.
    let opened_before = Settings { last_update_check: None, latest_version: None, ..app.settings.clone() };
    assert_eq!(opened_before.skipped_version, None);
    click_text(&ctx, &mut app, "Later", |t| t == "Later");
    app.settings_action(SettingsAction::Save(Settings { page_size: 75, ..opened_before }));
    assert_eq!((app.settings.page_size, app.settings.skipped_version, app.settings.latest_version), (75, Some(newer(1)), Some(newer(1))));
    let disk = on_disk(&app);
    assert_eq!((disk["page_size"].as_i64(), disk["skipped_version"].as_str(), disk["latest_version"].as_str()), (Some(75), Some(newer(1).to_string().as_str()), Some(newer(1).to_string().as_str())));
    assert!(checked_at(&app).is_some());
}

/// What the update check saves goes into settings.json as it is on disk,
/// so it never undoes Settings another window saved meanwhile; a file
/// rongo can't read is left alone.
#[test]
fn the_update_check_never_undoes_another_windows_settings() {
    let (hooks, _stub) = github(Ok(answer(newer(1))));
    let (ctx, mut app) = app_with(TempDir::new(), hooks);
    answered(&ctx, &mut app);
    let path = app.settings_path.clone().unwrap();
    let other = Settings { page_size: 75, ..Settings::load(&path).0 };
    other.save(&path).unwrap();
    click_text(&ctx, &mut app, "Later", |t| t == "Later");
    let disk = on_disk(&app);
    assert_eq!((disk["page_size"].as_i64(), disk["skipped_version"].as_str()), (Some(75), Some(newer(1).to_string().as_str())));
    assert_eq!(app.settings.page_size, 50, "this window's settings stay as they are");

    let (hooks, _stub) = github(Ok(answer(newer(1))));
    let (ctx, mut app) = app_with(TempDir::new(), hooks);
    answered(&ctx, &mut app);
    let path = app.settings_path.clone().unwrap();
    std::fs::write(&path, "{ a hand-made typo").unwrap();
    click_text(&ctx, &mut app, "Later", |t| t == "Later");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ a hand-made typo", "left alone");
    assert!(app.update.found.is_none() && app.toasts.is_empty());
}
