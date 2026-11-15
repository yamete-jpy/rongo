//! Settings: opening it, its actions, saving it, and the connection names its history list shows.

use std::collections::HashMap;

use rongo_core::profile::ConnectionProfile;
use rongo_core::ConnId;

use crate::dialogs::{Confirm, Pending};
use crate::result_view::thousands;
use crate::settings::{ChangePasswordDialog, HISTORY_ALL_IN_USE, HISTORY_IN_USE, HistorySettings, Settings, SettingsAction, SettingsDialog, mb};

use super::RongoApp;

impl RongoApp {
    /// Connection names by profile id, for the histories Settings lists:
    /// the saved connections, then open ones that aren't saved (the demo
    /// server, also when it isn't open). A history whose profile is in none
    /// of these belongs to a deleted profile.
    pub(super) fn profile_names(&mut self) -> &HashMap<uuid::Uuid, String> {
        self.refresh_names();
        &self.names
    }

    /// Brings `names` (see `profile_names`) up to date: made again only when
    /// the saved or open connections change, not each frame Settings shows.
    pub(super) fn refresh_names(&mut self) {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for p in self.store.iter().flat_map(|s| s.profiles()) {
            (p.id, &p.name).hash(&mut h);
        }
        h.write_u8(0xff);
        for c in self.conns.values() {
            (c.profile.id, &c.profile.name).hash(&mut h);
        }
        let key = h.finish();
        if self.names_from == Some(key) {
            return;
        }
        let mut names: HashMap<uuid::Uuid, String> = self.store.iter().flat_map(|s| s.profiles()).map(|p| (p.id, p.name.clone())).collect();
        for c in self.conns.values() {
            names.entry(c.profile.id).or_insert_with(|| c.profile.name.clone());
        }
        let demo = ConnectionProfile::mock();
        names.entry(demo.id).or_insert(demo.name);
        self.names = names;
        self.names_from = Some(key);
    }

    /// Opens Settings and measures the history for its Collection History section.
    pub(super) fn open_settings(&mut self) {
        self.settings_dialog = Some(SettingsDialog::new(&self.settings));
        self.history_files.rescan(&self.ctx);
    }

    /// What saving these limits removes from Collection History at once, or
    /// None when nothing: entries older than fewer days, and the oldest
    /// entries over a smaller size (how much, from the latest measure).
    fn limits_remove(&self, new: &HistorySettings) -> Option<String> {
        let old = &self.settings.history;
        let mut what = Vec::new();
        if new.keep_days < old.keep_days {
            what.push(format!("entries older than {} day{}", thousands(u64::from(new.keep_days)), if new.keep_days == 1 { "" } else { "s" }));
        }
        if new.max_mb < old.max_mb {
            match self.history_files.scan() {
                Some(Ok(s)) if s.bytes() <= new.max_bytes() => {}
                Some(Ok(s)) => what.push(format!("about {} of the oldest entries", mb(s.bytes() - new.max_bytes()))),
                _ => what.push("the oldest entries over the new size limit".into()),
            }
        }
        (!what.is_empty()).then(|| what.join(" and "))
    }

    pub(super) fn settings_action(&mut self, a: SettingsAction) {
        match a {
            SettingsAction::Close => self.settings_dialog = None,
            // Lower limits remove history at once: asked first, Settings
            // staying open behind the question.
            SettingsAction::Save(s) => match self.limits_remove(&s.history) {
                Some(what) => {
                    let msg = format!(
                        "Keep Collection History for {} days and at most {} MB? Saving removes {what} now. Their documents can't be restored afterwards.",
                        thousands(u64::from(s.history.keep_days)),
                        thousands(s.history.max_mb)
                    );
                    self.confirm = Some(Confirm::new("Lower History Limits", msg, "Save", Pending::SaveSettings(Box::new(s))).danger());
                }
                None => self.save_settings(s),
            },
            // Opens even for a dropped collection, or a connection that
            // isn't open or was deleted: listing and preview read the files;
            // restoring waits for the connection.
            SettingsAction::OpenHistory(profile, ns) => {
                self.settings_dialog = None;
                self.open_history(ConnId(profile), ns);
            }
            SettingsAction::DeleteHistory(profile, ns) if self.history_busy(Some(profile), Some(&ns)) => self.toast(HISTORY_IN_USE, true),
            SettingsAction::DeleteHistory(profile, ns) => {
                let found = match self.history_files.scan() {
                    Some(Ok(s)) => s.colls.iter().find(|c| c.0 == profile && c.1 == ns).map(|c| (c.2, c.3)),
                    _ => None,
                };
                let what = match found {
                    Some((1, bytes)) => format!(" (1 entry, {})", mb(bytes)),
                    Some((n, bytes)) => format!(" ({} entries, {})", thousands(n), mb(bytes)),
                    None => String::new(),
                };
                let on = self.profile_names().get(&profile).cloned().unwrap_or_else(|| crate::settings::DELETED_PROFILE.into());
                let msg = format!("Delete the Collection History of {ns} on {on}{what}? Its saved documents can't be restored afterwards.");
                let before = rongo_core::bson::DateTime::now().timestamp_millis();
                self.confirm = Some(Confirm::new("Delete History", msg, "Delete", Pending::DeleteHistory { profile, ns, before }).danger());
            }
            SettingsAction::ClearHistory if self.history_busy(None, None) => self.toast(HISTORY_ALL_IN_USE, true),
            SettingsAction::ClearHistory => {
                let size = match self.history_files.scan() {
                    Some(Ok(s)) => format!(" ({})", mb(s.bytes())),
                    _ => String::new(),
                };
                let msg = format!("Delete all Collection History{size}, of every connection? None of the documents it saved can be restored afterwards.");
                self.confirm = Some(Confirm::new("Clear All History", msg, "Delete All", Pending::ClearHistory).danger());
            }
            // Over Settings, which stays open behind it.
            SettingsAction::ChangeMasterPassword => {
                if let Some(vault) = self.vault.clone() {
                    self.change_password = Some(ChangePasswordDialog::new(vault));
                }
            }
            SettingsAction::ResetVault => self.ask_reset(),
        }
    }

    /// Saves and applies Settings (closing the dialog). New history limits
    /// apply now, not only at the next recorded write.
    pub(super) fn save_settings(&mut self, mut s: Settings) {
        self.settings_dialog = None;
        // The update check's, as it keeps them now: the dialog doesn't edit
        // them, and a Later since it opened stays.
        s.last_update_check = self.settings.last_update_check;
        s.skipped_version = self.settings.skipped_version;
        s.latest_version = self.settings.latest_version;
        s.apply();
        let saved = match &self.settings_path {
            Some(path) => s.save(path),
            None => Err("there is no configuration folder".into()),
        };
        // Trimmed once nothing is being recorded (`poll_history_files`).
        let (old, new) = (&self.settings.history, &s.history);
        if (old.keep_days, old.max_mb) != (new.keep_days, new.max_mb) {
            let (keep_days, max_bytes) = (new.keep_days, new.max_bytes());
            self.history_files.queue("Trimming to the new limits…", None, None, move |s| s.purge(keep_days, max_bytes, &[]).map(|_| ()));
        }
        self.settings = s;
        // Cached JSON text holds dates in the old zone.
        for t in &mut self.tabs {
            for v in t.views_mut() {
                v.relayout();
            }
        }
        match saved {
            Ok(()) => self.toast("Settings saved", false),
            Err(e) => self.toast(format!("Settings applied but not saved: {e}"), true),
        }
    }
}
