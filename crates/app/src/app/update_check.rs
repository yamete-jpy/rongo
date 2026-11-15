//! The update check: asking GitHub at launch, taking the answer in, the
//! banner under the toolbar, Open Download Page and Later.

use egui::Ui;
use rongo_core::bson::DateTime;

use crate::job::Job;
use crate::settings::Settings;
use crate::theme;
use crate::update::{self, BannerAction, Release, Version};

use super::RongoApp;

impl RongoApp {
    /// At launch, while the check is on in Settings: the banner for a newer
    /// version an earlier check found (no request needed), and, unless
    /// rongo asked within the last day or has no request to make (test
    /// apps), a request for the latest release on a thread of its own. That
    /// thread also saves when it asked and what it found (only for an answer
    /// that read: a failed check isn't today's), so no settings write holds
    /// up a frame.
    pub(super) fn start_update_check(&mut self) {
        let Some(running) = Version::running() else { return };
        if !self.settings.check_updates {
            return;
        }
        let skipped = self.settings.skipped_version;
        self.update.found = self.settings.latest_version.map(Release::remembered).filter(|r| update::offer(r, running, skipped));
        let Some(fetch) = self.update.hooks.fetch.clone() else { return };
        let now = DateTime::now();
        if !update::due(self.settings.last_update_check, now) {
            return;
        }
        let (path, fallback) = (self.settings_path.clone(), self.settings.clone());
        self.update.job = Some(Job::start(&self.ctx, "update-check", move |_| {
            let release = fetch().ok().and_then(|body| update::parse_release(&body));
            // Only an answer that read (newer or not) is today's check: a
            // failed one (offline at launch, say) saves nothing, so the next
            // launch asks again. Into the file as it is on disk; a failure
            // to save is said nowhere: at worst rongo asks again sooner.
            if let (Some(path), Some(release)) = (path, &release) {
                let _ = Settings::change_on_disk(&path, &fallback, |s| {
                    s.last_update_check = Some(now);
                    s.latest_version = Some(release.version);
                });
            }
            release
        }));
    }

    /// Takes the answer in, also while the launch dialog shows (it was
    /// saved on its thread): the newest version is remembered, and the
    /// banner offers it if it is newer than this one and not hidden by
    /// Later. A failed check changes nothing and says nothing. Then a page
    /// opening that failed is said.
    pub(super) fn poll_update_check(&mut self) {
        if let Some(done) = self.update.job.as_ref().and_then(|j| j.poll()) {
            self.update.job = None;
            if let Ok(Some(release)) = done {
                // As saved: in memory only now, so a Save in Settings during a
                // check that then fails writes no check time.
                self.settings.last_update_check = Some(DateTime::now());
                self.settings.latest_version = Some(release.version);
                let skipped = self.settings.skipped_version;
                self.update.found = Some(release).filter(|r| Version::running().is_some_and(|running| update::offer(r, running, skipped)));
            }
        }
        if let Some(done) = self.update.opening.as_ref().and_then(|j| j.poll()) {
            self.update.opening = None;
            match done {
                Ok(Ok(())) => {}
                Ok(Err(e)) => self.toast(e, true),
                Err(e) => self.toast(format!("Couldn't open the download page: {e}"), true),
            }
        }
    }

    /// The banner under the toolbar, while there is a newer release to
    /// offer and the check is on in Settings (only in the main window: never
    /// over the launch dialog).
    pub(super) fn update_banner(&mut self, ui: &mut Ui) {
        if !self.settings.check_updates {
            return;
        }
        let Some(release) = self.update.found.clone() else { return };
        let action = egui::Panel::top("update-banner")
            .frame(egui::Frame::new().fill(theme::RAISED).inner_margin(egui::Margin::symmetric(10, 4)))
            .show(ui, |ui| update::banner(ui, &release))
            .inner;
        match action {
            Some(BannerAction::Open) => {
                // Off the UI thread: macOS's `open` waits for the browser to take it.
                let (open, url) = (self.update.hooks.open.clone(), release.url);
                self.update.opening = Some(Job::start(&self.ctx, "open-download-page", move |_| open(&url).map_err(|e| format!("Couldn't open the download page ({e}). It is at {url}"))));
            }
            Some(BannerAction::Later) => {
                self.update.found = None;
                self.settings.skipped_version = Some(release.version);
                self.save_skipped();
            }
            None => {}
        }
    }

    /// Later's version, saved into settings.json as it is on disk now (as
    /// the launch check saves its own), so it never undoes Settings another
    /// window saved. On a click, like a Save in Settings. A file that can't
    /// be read is left alone, and a failure is said nowhere: at worst the
    /// banner shows again at the next launch.
    fn save_skipped(&self) {
        let Some(path) = &self.settings_path else { return };
        let skipped = self.settings.skipped_version;
        let _ = Settings::change_on_disk(path, &self.settings, |s| s.skipped_version = skipped);
    }
}
