//! The vault: unlocking, locking again when another window changed it, and Reset.

use std::path::Path;
use std::sync::Arc;

use rongo_core::profile::{ConnectionProfile, SecretKind};
use rongo_core::vault::{Vault, vault_key};
use rongo_core::CoreError;

use crate::job::Job;
use crate::dialogs::{ConfirmResult, Confirm, Pending};
use crate::tabs::TabKind;
use crate::unlock::{UnlockDialog, UnlockResult};
use crate::vault_ui::{self, CatchUp};

use super::RongoApp;
use super::history::fit_offer;

impl RongoApp {
    /// The launch dialog answered: the vault is open, or the app quits.
    pub(super) fn unlock_answer(&mut self, ctx: &egui::Context, answer: UnlockResult) {
        match answer {
            UnlockResult::Unlocked { vault, keychain_left } => {
                self.vault = Some(vault);
                self.unlock = None;
                // The move worked; only the Keychain's old copy stays. Said, and nothing else changes.
                if let Some(e) = keychain_left {
                    self.toast(format!("Your passwords were moved into rongo's vault, but rongo's older items in the Keychain could not all be deleted ({e}). Keychain Access lists them under dev.rongo.connections."), true);
                }
                if std::mem::take(&mut self.demo_on_unlock) {
                    self.open_demo();
                }
            }
            UnlockResult::Quit => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            UnlockResult::Reset => self.ask_reset(),
        }
    }

    /// Asks whether to reset the master password (Settings → Reset…, or
    /// Forgot? at launch), naming what is lost, and offering to delete all
    /// Collection History too (ticked); the offer says how much once that is
    /// measured, off the UI thread. Not while Collection History is being
    /// written (`RESET_WAITS`).
    pub(super) fn ask_reset(&mut self) {
        if self.history_busy(None, None) {
            return self.toast(RESET_WAITS, true);
        }
        let keychain = if self.keychain.is_some() { " rongo's items left in the Keychain are deleted too." } else { "" };
        let message = format!(
            "Reset the master password? Every saved connection password and SSH secret is deleted: the connections stay, and their passwords must be typed again. Collection History saved so far can no longer be read.{keychain} This can't be undone."
        );
        self.reset_ask = Some(Confirm::new("Reset Master Password", message, "Reset", Pending::ResetVault).danger().checkbox(DELETE_ALL_HISTORY, true));
        self.history_files.rescan(&self.ctx);
    }

    /// The Reset question, once the history is measured: how much there is,
    /// or (none) no offer to delete it.
    pub(super) fn measure_reset_offer(&mut self) {
        let Some(c) = &mut self.reset_ask else { return };
        if c.checkbox.as_ref().is_none_or(|(label, _)| label != DELETE_ALL_HISTORY) || !self.history_files.idle() {
            return;
        }
        // Not measured: still offered, without a size.
        let Some(Ok(s)) = self.history_files.scan() else { return };
        fit_offer(&mut c.checkbox, DELETE_ALL_HISTORY, s.bytes());
    }

    /// Reset, once confirmed: the vault is deleted, and with it every saved
    /// password and SSH secret and the Collection History key. rongo's items
    /// left in the Keychain go too (macOS), so a later launch can't move them
    /// back. Saved connections stay. With `history`, every Collection History
    /// file recorded up to now is deleted, off the UI thread; else they stay,
    /// listed in Settings, but can't be read. The window is then locked and
    /// Set asks for a new master password. Refused while Collection History
    /// is being written (a write may have begun since the question).
    pub(super) fn reset_vault(&mut self, history: bool) {
        let Some(path) = self.vault_path.clone() else { return self.toast("There is no data folder to keep rongo's vault in", true) };
        // A write may have begun since the question.
        if self.history_busy(None, None) {
            return self.toast(RESET_WAITS, true);
        }
        if let Err(e) = Vault::reset(&path) {
            return self.toast(format!("Couldn't reset the master password: {e}"), true);
        }
        // macOS may ask before each delete: off the UI thread, and a refusal is only said.
        if let Some(keychain) = self.keychain.clone() {
            let keys: Vec<String> = self.store.iter().flat_map(|s| s.profiles()).flat_map(|p| [vault_key(p.id, SecretKind::Password), vault_key(p.id, SecretKind::Ssh)]).collect();
            self.keychain_forget = Some(Job::start(&self.ctx, "keychain-forget", move |_| rongo_core::vault::keychain::forget(keychain.as_ref(), &keys)));
        }
        if history && self.history_root.is_some() {
            // Starts once nothing is being recorded (`poll_history_files`), maybe after
            // entries were recorded under the new master password: those stay.
            let cutoff = rongo_core::bson::DateTime::now().timestamp_millis();
            self.history_files.queue("Deleting all Collection History…", None, None, move |s| s.clear_before(cutoff));
        }
        self.vault = None;
        // Nothing open goes on with the old secrets or history key.
        self.settings_dialog = None;
        self.change_password = None;
        self.connect_dialog = None;
        self.export_conn = None;
        self.restore_ask = None;
        self.close_history_tabs();
        self.unlock = Some(unlock_dialog(Some(&path), false, Vec::new()).with_message(RESET_DONE));
    }

    /// History tabs hold a store sealed with this window's key: once that
    /// key is let go (a Reset, or the vault changed in another window) they
    /// close.
    fn close_history_tabs(&mut self) {
        self.close_tabs_where(|k| matches!(k, TabKind::History(_)));
    }

    /// Reset's Keychain clean-up finished: a refusal is said, and changes nothing else.
    pub(super) fn poll_keychain_forget(&mut self) {
        let Some(done) = self.keychain_forget.as_ref().and_then(|j| j.poll()) else { return };
        self.keychain_forget = None;
        let why = match done {
            Ok(Ok(())) => return,
            Ok(Err(e)) => e.to_string(),
            Err(_) => "the clean-up stopped unexpectedly".into(),
        };
        self.toast(format!("The master password was reset, but rongo's older items in the Keychain could not all be deleted ({why}). Keychain Access lists them under dev.rongo.connections."), true);
    }

    /// The Reset question: over Settings, or (locked) in place of the launch dialog.
    pub(super) fn reset_dialog(&mut self, ctx: &egui::Context) {
        let Some(c) = &mut self.reset_ask else { return };
        match c.show(ctx) {
            Some(ConfirmResult::Yes) => {
                if let Some(c) = self.reset_ask.take() {
                    self.confirmed(c);
                }
            }
            Some(ConfirmResult::No) => self.reset_ask = None,
            None => {}
        }
    }

    /// Another rongo window changed the vault (its password, or reset it):
    /// this one's copy is dropped and the master password asked again (Set,
    /// if the vault is gone). Nothing is written under the old key.
    pub(super) fn lock_again(&mut self) {
        self.vault = None;
        // They hold the vault, or the history key, that is gone.
        self.change_password = None;
        self.close_history_tabs();
        let dialog = unlock_dialog(self.vault_path.as_deref(), false, Vec::new());
        let why = if Vault::exists(dialog.path()) {
            "The vault was changed in another rongo window. Unlock it with its master password."
        } else {
            vault_ui::REMOVED
        };
        self.unlock = Some(dialog.with_message(why));
    }

    /// A vault write failed. Another window having changed the vault locks
    /// this one again (nothing was written under the old key) and says so in
    /// a toast, `then` being what to do once unlocked; true. Any other
    /// error is left to the caller.
    pub(super) fn vault_write_failed(&mut self, e: &CoreError, then: &str) -> bool {
        if *e != CoreError::VaultChanged {
            return false;
        }
        self.lock_again();
        self.toast(vault_ui::changed_message(then), true);
        true
    }

    /// The vault, caught up with what another rongo window saved since this
    /// one last read it, for reading secrets. None (and the master password
    /// asked again) when it was changed there.
    pub(super) fn fresh_vault(&mut self) -> Option<Arc<Vault>> {
        let vault = self.vault.clone()?;
        match vault_ui::catch_up(&vault) {
            CatchUp::Fresh => {}
            CatchUp::Changed => {
                self.lock_again();
                return None;
            }
            // Busy or unreadable just now: this window's copy still serves.
            CatchUp::Failed(e) => self.toast(vault_ui::unreadable(&e), true),
        }
        Some(vault)
    }

    /// Before Collection History is written or read with the key this window
    /// holds: the vault caught up with the other rongo windows, so nothing is
    /// sealed under a key another window's Reset left behind (an entry
    /// already being written keeps its key). False, said in a toast, when it
    /// can't be: changed there (the master password asked again) or busy or
    /// unreadable just now. The caller then sends nothing.
    pub(super) fn history_key_current(&mut self) -> bool {
        // Locked: `history_key` says so.
        let Some(vault) = self.vault.clone() else { return true };
        match vault_ui::catch_up(&vault) {
            CatchUp::Fresh => true,
            CatchUp::Changed => {
                self.lock_again();
                self.toast(vault_ui::changed_message("try again"), true);
                false
            }
            CatchUp::Failed(e) => {
                self.toast(vault_ui::unreadable(&e), true);
                false
            }
        }
    }
}

/// The launch dialog for the vault at `path`, or, with nowhere to keep one,
/// a dialog that says so and offers Quit.
pub(super) fn unlock_dialog(path: Option<&Path>, moving: bool, profiles: Vec<ConnectionProfile>) -> UnlockDialog {
    match path {
        Some(p) => UnlockDialog::new(p.to_path_buf(), moving, profiles),
        None => UnlockDialog::nowhere(),
    }
}

/// The Reset question's offer, before the history is measured.
pub(super) const DELETE_ALL_HISTORY: &str = "Also delete all Collection History";

/// Why Reset waits, as Rename does: what is being written now would be
/// sealed with the key Reset throws away.
const RESET_WAITS: &str = "Wait for the delete, update, paste or restore that is writing Collection History to finish before resetting the master password";

/// What the Set dialog says after a Reset.
const RESET_DONE: &str = "The master password was reset. Set a new one. Saved connections stay; type their passwords again when you connect.";
