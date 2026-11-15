//! What the app says and does about the vault outside the launch dialog:
//! catching up with other windows' writes, and the texts for a vault that
//! changed or could not be read.

use rongo_core::CoreError;
use rongo_core::vault::Vault;

/// Shown when the vault is gone (another window reset it) and a password is
/// asked for a new one.
pub const REMOVED: &str = "The vault was removed in another rongo window. Set a master password to go on.";

/// What catching a vault up with the other rongo windows' writes found.
#[derive(Debug, PartialEq)]
pub enum CatchUp {
    /// Caught up (or nothing new): this window's copy is current.
    Fresh,
    /// Another window changed the master password or reset the vault, or the
    /// file is gone: it must be unlocked again, and nothing written before.
    Changed,
    /// Busy or unreadable just now: this window's copy still serves, but
    /// the user is told why (the error's text, never a secret).
    Failed(CoreError),
}

/// `vault.reload_if_changed()` as the one place that decides what each
/// answer means; every reader of secrets goes through it.
pub fn catch_up(vault: &Vault) -> CatchUp {
    match vault.reload_if_changed() {
        Ok(()) => CatchUp::Fresh,
        Err(CoreError::VaultChanged) => CatchUp::Changed,
        Err(e) => CatchUp::Failed(e),
    }
}

/// The text for a vault failure that is not a change, as a toast.
pub fn unreadable(e: &CoreError) -> String {
    format!("Couldn't read the vault just now: {e}")
}

/// What to tell the user when a vault write or read found it changed;
/// `then` says what to do once unlocked ("Save", "try again").
pub fn changed_message(then: &str) -> String {
    format!("The vault was changed in another rongo window: unlock again, then {then}")
}

/// `"the master password needs …"` as a sentence: capital first, full stop.
pub(crate) fn sentence(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(first) => format!("{}{}.", first.to_uppercase(), c.as_str().trim_end_matches('.')),
        None => String::new(),
    }
}
