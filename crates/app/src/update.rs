//! The update check: at launch, at most once a day, rongo asks GitHub for
//! its latest release on a thread of its own, and a banner under the
//! toolbar says when that is newer than the running version; the newest
//! version found is remembered (Settings' `latest_version`), so the banner
//! shows at every launch until Later or an update. rongo never
//! downloads or installs anything itself, and a check that fails (offline,
//! blocked, rate-limited, an answer it doesn't expect) shows and says
//! nothing. The request (`fetch_latest`) and the browser (`open_page`) are
//! the app's `Hooks`: test apps get none, or stubs, never the real ones.

use std::fmt;
use std::sync::Arc;

use egui::{Align, Layout, RichText, Ui};
use egui_phosphor::regular as icon;
use rongo_core::bson::DateTime;

use crate::job::Job;
use crate::theme;

/// GitHub's latest release of the public repository (drafts and
/// pre-releases are never "latest"). Not in test builds: tests never ask.
#[cfg(not(test))]
pub const LATEST_API: &str = "https://api.github.com/repos/yamete-jpy/rongo/releases/latest";

/// The only pages the banner opens start with this.
pub const RELEASES: &str = "https://github.com/yamete-jpy/rongo/releases/";

/// What the banner opens when the answer's page is not one of those.
pub const LATEST_PAGE: &str = "https://github.com/yamete-jpy/rongo/releases/latest";

/// The most of an answer that is read: GitHub's is a few KB.
pub const MAX_BODY: usize = 1 << 20;

/// How often rongo asks, at most.
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// The Homebrew line's command (macOS).
pub const BREW_UPGRADE: &str = "brew upgrade --cask rongo";

/// A release's number, compared number by number (0.1.10 is newer than 0.1.9).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl Version {
    /// Exactly three numbers of ASCII digits, separated by dots ("0.1.6").
    pub fn parse(s: &str) -> Option<Version> {
        // Digits only: `u64::from_str` would also take "+1".
        let number = |n: &str| if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) { n.parse().ok() } else { None };
        let mut parts = s.split('.');
        let v = Version { major: number(parts.next()?)?, minor: number(parts.next()?)?, patch: number(parts.next()?)? };
        parts.next().is_none().then_some(v)
    }

    /// The version running now; None for one that isn't three numbers (a
    /// pre-release, say), which then never asks.
    pub fn running() -> Option<Version> {
        Version::parse(env!("CARGO_PKG_VERSION"))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// A release tag as `scripts/release.sh` makes them, `v0.1.6` (the `v` may
/// be left out); anything else is no version.
pub fn parse_tag(tag: &str) -> Option<Version> {
    Version::parse(tag.strip_prefix('v').unwrap_or(tag))
}

/// A release newer than this one may be: its number, and the page to open
/// for it (always one `page_url` lets through).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    pub url: String,
}

impl Release {
    /// A version an earlier check found, with its release's page (the tag
    /// `scripts/release.sh` makes).
    pub fn remembered(version: Version) -> Release {
        Release { version, url: format!("{RELEASES}tag/v{version}") }
    }
}

/// The two fields of GitHub's answer rongo reads; serde skips the rest.
/// Either may be of any type: one that isn't text counts as missing.
#[derive(serde::Deserialize)]
struct Answer {
    tag_name: Option<serde_json::Value>,
    html_url: Option<serde_json::Value>,
}

/// GitHub's answer: its `tag_name` and `html_url`. An answer over
/// `MAX_BODY`, not a JSON object, without a tag of three numbers, or with a
/// field given twice is no release.
pub fn parse_release(body: &[u8]) -> Option<Release> {
    // An object, as GitHub answers: serde would also read the struct from
    // an array of two values.
    if body.len() > MAX_BODY || body.trim_ascii_start().first() != Some(&b'{') {
        return None;
    }
    let answer: Answer = serde_json::from_slice(body).ok()?;
    let version = parse_tag(answer.tag_name?.as_str()?)?;
    Some(Release { version, url: page_url(answer.html_url.as_ref().and_then(|u| u.as_str())) })
}

/// The page to open for an answer's `html_url`: that page when it is a
/// release's page, `releases/tag/<tag>` (the tag one plain path segment)
/// or `releases/latest`; otherwise the latest release's page. Nothing else
/// from the answer is ever opened, a download among them.
pub fn page_url(html_url: Option<&str>) -> String {
    let plain = |seg: &str| !seg.is_empty() && seg != "." && seg != ".." && seg.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
    match html_url.and_then(|u| u.strip_prefix(RELEASES)) {
        Some(rest) if rest == "latest" || rest.strip_prefix("tag/").is_some_and(plain) => format!("{RELEASES}{rest}"),
        _ => LATEST_PAGE.into(),
    }
}

/// Whether to ask now, last asked at `last`: never asked, a day or more
/// ago, or "later" than now (the clock was turned back).
pub fn due(last: Option<DateTime>, now: DateTime) -> bool {
    last.is_none_or(|last| {
        let since = now.timestamp_millis().saturating_sub(last.timestamp_millis());
        !(0..DAY_MS).contains(&since)
    })
}

/// Whether the banner offers `release`: newer than the running version and,
/// after Later, newer than the version Later hid.
pub fn offer(release: &Release, running: Version, skipped: Option<Version>) -> bool {
    release.version > running && skipped.is_none_or(|s| release.version > s)
}

/// Asks GitHub for the latest release: the answer's body, or why not.
pub type Fetch = Arc<dyn Fn() -> Result<Vec<u8>, String> + Send + Sync>;

/// Opens a page in the default browser.
pub type Open = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// Shows a file in the file manager (an export's, from the Operations panel).
pub type Reveal = Arc<dyn Fn(&std::path::Path) -> Result<(), String> + Send + Sync>;

/// How the app reaches the outside: the request, the browser and the file
/// manager. The app has the real ones (`system`); test apps have none
/// unless a test gives them stubs.
pub struct Hooks {
    /// None: no check at all.
    pub fetch: Option<Fetch>,
    pub open: Open,
    pub reveal: Reveal,
}

impl Hooks {
    /// The real request, browser and file manager.
    #[cfg(not(test))]
    pub fn system() -> Self {
        Hooks { fetch: Some(Arc::new(fetch_latest)), open: Arc::new(open_page), reveal: Arc::new(|p| rongo_core::fsx::reveal(p).map_err(|e| e.to_string())) }
    }

    /// No request, and a browser and file manager that only say they
    /// aren't there.
    #[cfg(test)]
    pub fn none() -> Self {
        Hooks { fetch: None, open: Arc::new(|_| Err("tests never open a browser".into())), reveal: Arc::new(|_| Err("tests never open a file manager".into())) }
    }
}

/// One GET of `LATEST_API`, with `Accept` and `User-Agent: rongo/<version>`
/// only; 10 s for all of it, at most `MAX_BODY` read; HTTPS only.
#[cfg(not(test))]
pub fn fetch_latest() -> Result<Vec<u8>, String> {
    // Set on the agent, so a proxy gets the same two headers.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .https_only(true)
        .user_agent(format!("rongo/{}", env!("CARGO_PKG_VERSION")))
        .accept("application/vnd.github+json")
        .build()
        .into();
    let mut answer = agent.get(LATEST_API).call().map_err(|e| e.to_string())?;
    answer.body_mut().with_config().limit(MAX_BODY as u64).read_to_vec().map_err(|e| e.to_string())
}

/// The default browser, without a shell in between.
#[cfg(not(test))]
pub fn open_page(url: &str) -> Result<(), String> {
    open::that_detached(url).map_err(|e| e.to_string())
}

/// The check as the app holds it: the request running, the release the
/// banner offers, the page being opened.
pub struct UpdateCheck {
    pub hooks: Hooks,
    pub job: Option<Job<Option<Release>>>,
    /// A newer release, not hidden by Later: the banner shows it.
    pub found: Option<Release>,
    /// Opening the page, off the UI thread (macOS's `open` waits for the
    /// browser to take it): why it failed, if it did.
    pub opening: Option<Job<Result<(), String>>>,
}

impl UpdateCheck {
    pub fn new(hooks: Hooks) -> Self {
        Self { hooks, job: None, found: None, opening: None }
    }
}

/// What the banner's buttons ask for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BannerAction {
    /// Open Download Page.
    Open,
    /// Later: hide it until a newer version is out.
    Later,
}

/// The banner's message.
pub fn available(v: Version) -> String {
    format!("rongo {v} is available.")
}

/// The banner under the toolbar: the message (on macOS with the Homebrew
/// command), and Open Download Page and Later on the right.
pub fn banner(ui: &mut Ui, release: &Release) -> Option<BannerAction> {
    let mut action = None;
    // The buttons first, from the right; the message in the room they
    // leave, the Homebrew line cut short rather than widening the window.
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.button("Later").on_hover_text("Hide this until a newer version is out").clicked() {
                action = Some(BannerAction::Later);
            }
            if ui.add(theme::primary_button("Open Download Page", false)).on_hover_text(&release.url).clicked() {
                action = Some(BannerAction::Open);
            }
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                ui.label(RichText::new(icon::ARROW_CIRCLE_UP).color(theme::ACCENT));
                ui.label(RichText::new(available(release.version)).strong());
                if cfg!(target_os = "macos") {
                    let mut line = egui::text::LayoutJob::default();
                    let style = ui.style().clone();
                    for part in [RichText::new("Installed with Homebrew? Run ").color(theme::MUTED), RichText::new(BREW_UPGRADE).font(theme::mono(12.5)).color(theme::TEXT), RichText::new(".").color(theme::MUTED)] {
                        part.append_to(&mut line, &style, egui::FontSelection::Default, Align::Center);
                    }
                    ui.add_space(6.0);
                    ui.add(egui::Label::new(line).truncate());
                }
            });
        });
    });
    action
}

/// Settings' `last_update_check`: RFC 3339 text in UTC. Text that doesn't
/// read as a time is no time (the next launch asks).
pub mod rfc3339 {
    use rongo_core::bson::DateTime;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(t: &Option<DateTime>, s: S) -> Result<S::Ok, S::Error> {
        match t.and_then(|t| t.try_to_rfc3339_string().ok()) {
            Some(text) => s.serialize_some(&text),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<DateTime>, D::Error> {
        Ok(Option::<String>::deserialize(d)?.and_then(|t| DateTime::parse_rfc3339_str(t).ok()))
    }
}

/// Settings' `skipped_version`: "0.1.6". Text that isn't a version is none
/// (the banner may show again).
pub mod version_text {
    use serde::{Deserialize, Deserializer, Serializer};

    use super::Version;

    pub fn serialize<S: Serializer>(v: &Option<Version>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(v) => s.serialize_some(&v.to_string()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Version>, D::Error> {
        Ok(Option::<String>::deserialize(d)?.and_then(|t| Version::parse(&t)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(major: u64, minor: u64, patch: u64) -> Version {
        Version { major, minor, patch }
    }

    /// Versions compare number by number: 0.1.10 is newer than 0.1.9. A tag
    /// is `v` and three numbers of digits; anything else is no version.
    #[test]
    fn versions_compare_by_number() {
        assert_eq!(parse_tag("v0.1.6"), Some(v(0, 1, 6)));
        assert_eq!(parse_tag("0.1.6"), Some(v(0, 1, 6)), "the v may be left out");
        assert_eq!(Version::parse("10.20.30"), Some(v(10, 20, 30)));
        assert!(parse_tag("v0.1.10") > parse_tag("v0.1.9"));
        assert!(parse_tag("v0.2.0") > parse_tag("v0.1.99"));
        assert!(parse_tag("v1.0.0") > parse_tag("v0.99.99"));
        assert_eq!(parse_tag("v0.1.6"), parse_tag("v0.1.6"));
        assert!(parse_tag("v0.1.5") < parse_tag("v0.1.6"));
        for bad in ["", "v", "v1", "v1.2", "v1.2.3.4", "v1.2.x", "v1.2.3-beta", "v1.2.3+build", "v+1.2.3", "v1.+2.3", "v-1.2.3", "v1..3", "v 1.2.3", "v1.2.3 ", "V1.2.3", "vv1.2.3", "v١.٢.٣", "v99999999999999999999999.0.0", "latest", "nightly"] {
            assert_eq!(parse_tag(bad), None, "{bad:?}");
        }
        assert_eq!(Version::parse("v0.1.6"), None, "a version has no v; only a tag does");
        assert_eq!(Version::running().map(|v| v.to_string()).as_deref(), Some(env!("CARGO_PKG_VERSION")), "the workspace version is three numbers");
        assert_eq!(v(0, 1, 10).to_string(), "0.1.10");
    }

    fn answer(tag: &str, url: Option<&str>) -> String {
        let url = url.map_or(String::new(), |u| format!(r#""html_url": {},"#, serde_json::Value::from(u)));
        format!(r#"{{"id": 1, {url} "tag_name": {}, "name": "rongo", "draft": false, "prerelease": false, "assets": [{{"name": "rongo-0.1.6-arm64.dmg", "size": 9000000}}]}}"#, serde_json::Value::from(tag))
    }

    /// GitHub's answer gives the tag and the page; an answer missing the
    /// tag, with a tag that isn't three numbers, not JSON, or over 1 MiB is
    /// no release. A missing or foreign page falls back to the latest
    /// release's page.
    #[test]
    fn answers_are_read_with_care() {
        let page = "https://github.com/yamete-jpy/rongo/releases/tag/v0.1.6";
        assert_eq!(parse_release(answer("v0.1.6", Some(page)).as_bytes()), Some(Release { version: v(0, 1, 6), url: page.into() }));
        assert_eq!(parse_release(answer("v0.1.6", None).as_bytes()).map(|r| r.url), Some(LATEST_PAGE.into()), "no page: the latest release's");
        assert_eq!(parse_release(answer("v0.1.6", Some("https://evil.example/rongo.dmg")).as_bytes()).map(|r| r.url), Some(LATEST_PAGE.into()), "a foreign page is never opened");
        assert_eq!(parse_release(br#"{"tag_name": "v0.1.6", "html_url": null}"#).map(|r| r.url), Some(LATEST_PAGE.into()));
        assert_eq!(parse_release(br#"{"tag_name": "v0.1.6", "html_url": 7}"#).map(|r| r.url), Some(LATEST_PAGE.into()));
        for (why, body) in [
            ("no tag", br#"{"html_url": "https://github.com/yamete-jpy/rongo/releases/tag/v0.1.6"}"#.to_vec()),
            ("a tag that isn't three numbers", answer("v0.1.6-rc1", Some(page)).into_bytes()),
            ("a tag that isn't text", br#"{"tag_name": 106}"#.to_vec()),
            ("null", b"null".to_vec()),
            ("an array", br#"[{"tag_name": "v0.1.6"}]"#.to_vec()),
            ("not JSON", b"<html>rate limited</html>".to_vec()),
            ("nothing", Vec::new()),
            ("half an answer", answer("v0.1.6", Some(page)).as_bytes()[..40].to_vec()),
            ("GitHub's error", br#"{"message": "API rate limit exceeded", "documentation_url": "https://docs.github.com"}"#.to_vec()),
            ("the two values in an array", br#"["v0.1.6", "https://github.com/yamete-jpy/rongo/releases/tag/v0.1.6"]"#.to_vec()),
            ("a tag given twice", br#"{"tag_name": "v0.1.6", "tag_name": "v9.9.9"}"#.to_vec()),
        ] {
            assert_eq!(parse_release(&body), None, "{why}");
        }
        // Up to 1 MiB is read; past it, nothing.
        let padded = |len: usize| {
            let head = r#"{"tag_name": "v0.1.6", "body": ""#;
            let tail = r#""}"#;
            format!("{head}{}{tail}", "x".repeat(len - head.len() - tail.len()))
        };
        assert_eq!(parse_release(padded(MAX_BODY).as_bytes()).map(|r| r.version), Some(v(0, 1, 6)));
        assert_eq!(parse_release(padded(MAX_BODY + 1).as_bytes()), None, "over 1 MiB");
    }

    /// Only a release's page opens (`releases/tag/<tag>`, or
    /// `releases/latest`); anything else in the answer, a download among
    /// them, opens the latest release's page instead.
    #[test]
    fn only_release_pages_open() {
        for ok in ["https://github.com/yamete-jpy/rongo/releases/tag/v0.1.6", "https://github.com/yamete-jpy/rongo/releases/latest", "https://github.com/yamete-jpy/rongo/releases/tag/v0.1.10"] {
            assert_eq!(page_url(Some(ok)), ok);
        }
        for foreign in [
            "https://github.com/yamete-jpy/rongo/releases/download/v0.1.6/rongo-0.1.6-arm64.dmg",
            "https://github.com/yamete-jpy/rongo/releases/latest/download/rongo.dmg",
            "https://github.com/yamete-jpy/rongo/releases/new",
            "https://github.com/yamete-jpy/rongo/releases/edit/v0.1.6",
            "https://github.com/yamete-jpy/rongo/releases/v0.1.6",
            "https://github.com/yamete-jpy/rongo/releases/tag/v0.1.6/assets",
            "https://github.com/yamete-jpy/rongo/releases/tag/v0.1.6/",
            "https://github.com/yamete-jpy/rongo/releases/tag/",
            "https://github.com/yamete-jpy/rongo/releases/tag",
            "https://github.com/yamete-jpy/rongo/releases/latest/",
            "https://evil.example/",
            "http://github.com/yamete-jpy/rongo/releases/tag/v0.1.6",
            "https://github.com/yamete-jpy/rongo/releasesX/tag/v0.1.6",
            "https://github.com/yamete-jpy/rongo/releases",
            "https://github.com/yamete-jpy/rongo/releases/",
            "https://github.com/yamete-jpy/rongo/releases/../../other/repo",
            "https://github.com/yamete-jpy/rongo/releases/tag/./v1",
            "https://github.com/yamete-jpy/rongo/releases//evil.example",
            "https://github.com/yamete-jpy/rongo/releases/tag/%2e%2e",
            "https://github.com/yamete-jpy/rongo/releases/tag/v1?redirect=https://evil.example",
            "https://github.com/yamete-jpy/rongo/releases/tag/v1#x",
            "https://github.com/yamete-jpy/rongo/releases/tag/v1 & calc.exe",
            "https://github.com/yamete-jpy/rongo/releases/tag/v1\"",
            "https://github.com/yamete-jpy/rongo/releases/tag/v1\n",
            "https://github.com/yamete-jpy/rongo/releases/tag/v1\\..\\x",
            "https://github.com.evil.example/yamete-jpy/rongo/releases/tag/v1",
            "https://github.com/yamete-jpy/rongo-evil/releases/tag/v1",
            "https://user@github.com/yamete-jpy/rongo/releases/tag/v1",
            "HTTPS://GITHUB.COM/yamete-jpy/rongo/releases/tag/v1",
            "javascript:alert(1)",
            "file:///etc/passwd",
            "",
        ] {
            assert_eq!(page_url(Some(foreign)), LATEST_PAGE, "{foreign:?}");
        }
        assert_eq!(page_url(None), LATEST_PAGE);
    }

    /// A version remembered from an earlier check opens its release's page,
    /// one the allow-list lets through.
    #[test]
    fn a_remembered_release_opens_its_tag_page() {
        let r = Release::remembered(v(0, 1, 6));
        assert_eq!(r, Release { version: v(0, 1, 6), url: "https://github.com/yamete-jpy/rongo/releases/tag/v0.1.6".into() });
        assert_eq!(page_url(Some(&r.url)), r.url);
    }

    /// rongo asks at most once a day: not again within 24 hours of the last
    /// time, but when it never asked, a day or more ago, or when the last
    /// time is ahead of the clock (turned back since).
    #[test]
    fn asks_at_most_once_a_day() {
        let now = DateTime::from_millis(1_800_000_000_000);
        let ago = |ms: i64| Some(DateTime::from_millis(now.timestamp_millis() - ms));
        let hour = 60 * 60 * 1000;
        assert!(due(None, now), "never asked");
        assert!(!due(Some(now), now));
        assert!(!due(ago(hour), now));
        assert!(!due(ago(24 * hour - 1), now));
        assert!(due(ago(24 * hour), now));
        assert!(due(ago(30 * 24 * hour), now));
        assert!(due(ago(-hour), now), "the clock was turned back");
    }

    /// The banner offers a release newer than this one; Later hides that
    /// version, but not a newer one.
    #[test]
    fn later_hides_that_version_but_not_a_newer_one() {
        let release = |version| Release { version, url: LATEST_PAGE.into() };
        let running = v(0, 1, 4);
        assert!(offer(&release(v(0, 1, 6)), running, None));
        assert!(offer(&release(v(0, 1, 10)), v(0, 1, 9), None));
        assert!(!offer(&release(v(0, 1, 4)), running, None), "the same version");
        assert!(!offer(&release(v(0, 1, 3)), running, None), "an older one");
        assert!(!offer(&release(v(0, 1, 6)), running, Some(v(0, 1, 6))), "hidden by Later");
        assert!(offer(&release(v(0, 1, 7)), running, Some(v(0, 1, 6))), "a newer one shows");
        assert!(!offer(&release(v(0, 1, 6)), running, Some(v(0, 1, 7))), "older than the one Later hid");
    }

    /// The banner: the message, Open Download Page and Later, all on screen
    /// in the narrowest window; the Homebrew line only on macOS.
    #[test]
    fn the_banner_names_the_version_and_offers_the_page_and_later() {
        let ctx = egui::Context::default();
        theme::install(&ctx);
        let release = Release { version: v(0, 1, 6), url: LATEST_PAGE.into() };
        let size = egui::vec2(960.0, 80.0);
        let mut drawn = Vec::new();
        for _ in 0..3 {
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)), ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| _ = banner(ui, &release));
            out.textures_delta.clear();
            drawn = out.shapes.iter().filter_map(|s| match &s.shape {
                egui::Shape::Text(t) => Some((t.galley.text().to_string(), t.galley.rect.translate(t.pos.to_vec2()))),
                _ => None,
            }).collect::<Vec<_>>();
        }
        let texts: Vec<&str> = drawn.iter().map(|(t, _)| t.as_str()).collect();
        for want in ["rongo 0.1.6 is available.", "Open Download Page", "Later"] {
            assert!(texts.contains(&want), "{want}: {texts:?}");
        }
        let brew = texts.iter().find(|t| t.contains(BREW_UPGRADE));
        if cfg!(target_os = "macos") {
            assert_eq!(brew, Some(&"Installed with Homebrew? Run brew upgrade --cask rongo."), "{texts:?}");
        } else {
            assert_eq!(brew, None, "Homebrew is macOS only: {texts:?}");
        }
        for (text, rect) in &drawn {
            assert!(rect.left() >= 0.0 && rect.right() <= size.x, "{text} is inside the window: {rect:?}");
        }
        let x = |want: &str| drawn.iter().find(|(t, _)| t == want).unwrap().1.left();
        assert!(x("rongo 0.1.6 is available.") < x("Open Download Page") && x("Open Download Page") < x("Later"), "the message, then the buttons on the right");
    }

    /// The bookkeeping Settings keeps reads back; text that isn't a time or
    /// a version is none rather than an unreadable settings file.
    #[test]
    fn bookkeeping_reads_back() {
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Kept {
            #[serde(with = "rfc3339", default)]
            at: Option<DateTime>,
            #[serde(with = "version_text", default)]
            skipped: Option<Version>,
        }
        let kept = Kept { at: Some(DateTime::from_millis(1_800_000_000_000)), skipped: Some(v(0, 1, 6)) };
        let json = serde_json::to_string(&kept).unwrap();
        assert_eq!(json, r#"{"at":"2027-01-15T08:00:00Z","skipped":"0.1.6"}"#);
        assert_eq!(serde_json::from_str::<Kept>(&json).unwrap(), kept);
        let none = Kept { at: None, skipped: None };
        assert_eq!(serde_json::from_str::<Kept>(r#"{"at": "yesterday", "skipped": "v0.1.6"}"#).unwrap(), none);
        assert_eq!(serde_json::from_str::<Kept>(r#"{"at": null, "skipped": null}"#).unwrap(), none);
        assert_eq!(serde_json::from_str::<Kept>("{}").unwrap(), none);
    }
}
