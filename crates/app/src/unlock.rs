//! The launch dialog: the master password, asked before anything else in the
//! window can be used. **Set** on the first launch (no vault), **Unlock** when
//! the vault exists, and on macOS **Move** on the first launch of a version
//! with a vault by someone whose older rongo kept secrets in the Keychain:
//! Move sets the master password and moves those secrets into the vault,
//! deleting them from the Keychain only once the vault is on disk. Argon2
//! takes about half a second, so it runs on a thread of its own while the
//! dialog shows a spinner and takes no input. **Forgot?** (Unlock), and
//! **Reset…** after a move that failed, hand the app its Reset question
//! (`UnlockResult::Reset`); the dialog itself deletes nothing. A failed move
//! also offers **Try Again**, unless what failed was a damaged history key in
//! the Keychain.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use egui::{Align, Context, Id, Key, Layout, Modal, RichText, TextEdit};
use egui_phosphor::regular as icon;
use rongo_core::history::{HistoryKey, Store};
use rongo_core::profile::{ConnectionProfile, SecretBackend};
use rongo_core::vault::{self, KdfParams, Vault};
use rongo_core::{CoreError, CoreResult};

use crate::job::Job;
use crate::theme;
use crate::vault_ui::{REMOVED, sentence};

pub enum UnlockResult {
    /// The vault is open. `keychain_left`: after a move out of the Keychain,
    /// why rongo's items there could not all be deleted (they stay there).
    Unlocked { vault: Arc<Vault>, keychain_left: Option<CoreError> },
    Quit,
    /// Forgot? (or Reset… after a failed move): the app asks whether to
    /// reset, then shows Set.
    Reset,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// No vault yet: choose a master password.
    Set,
    /// Open the vault.
    Unlock,
    /// No vault yet, but an older rongo may have kept secrets in the
    /// Keychain: choose a master password and move them into the vault.
    Move,
}

/// Argon2's cost for a new vault. Tests never wait for the real one.
const PARAMS: KdfParams = if cfg!(test) { KdfParams::TEST } else { KdfParams::DEFAULT };

const WRONG: &str = "Wrong master password (or a damaged vault). Try again.";

/// Whether to offer moving secrets out of the Keychain: on macOS, when there
/// is no vault yet and an older rongo may have kept something there (a saved
/// connection that uses a password or SSH, or Collection History, whose key
/// was there). `profiles` is None when the saved connections couldn't be
/// read: they may have, so the move is offered (an empty Keychain makes it
/// a plain Set); so is history that can't be measured. Looking reads no
/// Keychain item, so it asks macOS nothing. Never after a Reset (until a new
/// vault is made): what its clean-up of the Keychain could not delete must
/// not come back.
pub fn should_move(vault: &Path, profiles: Option<&[ConnectionProfile]>, history_root: &Path) -> bool {
    let secrets = || profiles.is_none_or(|ps| ps.iter().any(|p| p.uses_password() || p.needs_ssh_secret()));
    // Measured by file names and sizes: the key is never used. Unless measured empty, there may be some.
    let history = || !Store::new(history_root.to_path_buf(), HistoryKey::new([0; 32])).usage().is_ok_and(|bytes| bytes == 0);
    cfg!(target_os = "macos") && !Vault::exists(vault) && !Vault::was_reset(vault) && (secrets() || history())
}

/// The real Keychain, where there is one: Move reads it, Reset deletes
/// rongo's items left in it. Tests never touch it.
#[cfg(all(target_os = "macos", not(test)))]
pub(crate) fn keychain() -> Option<Arc<dyn SecretBackend>> {
    Some(Arc::new(vault::keychain::system()))
}

#[cfg(any(not(target_os = "macos"), test))]
pub(crate) fn keychain() -> Option<Arc<dyn SecretBackend>> {
    None
}

/// What Move moves from: the Keychain, and the saved profiles whose items
/// from before the Keychain vault it looks for.
struct MoveFrom {
    keychain: Arc<dyn SecretBackend>,
    profiles: Vec<ConnectionProfile>,
}

/// The vault, and (after a move) why the Keychain kept rongo's items.
type Opened = (Vault, Option<CoreError>);

type Work = Box<dyn FnOnce() -> CoreResult<Opened> + Send>;

pub struct UnlockDialog {
    path: PathBuf,
    mode: Mode,
    params: KdfParams,
    from: Option<MoveFrom>,
    password: String,
    again: String,
    /// What went wrong last; trying again may help.
    error: Option<String>,
    /// No password can be asked: why, with Quit offered (and Reset… when a
    /// move out of the Keychain failed).
    stuck: Option<String>,
    /// The failed move may work if run again (anything but a damaged
    /// history key in the Keychain): Try Again is offered too.
    retry: bool,
    /// The derivation (and the file work after it) running on its thread.
    working: Option<Job<CoreResult<Opened>>>,
    /// Which field to focus on the next frame.
    focus: Option<usize>,
    /// Derivations started (tests count them).
    #[cfg(test)]
    started: usize,
    /// While a test holds this, a derivation waits before it starts.
    #[cfg(test)]
    hold: Option<Arc<std::sync::Mutex<()>>>,
}

impl UnlockDialog {
    /// The dialog for the vault at `path`: Unlock when it exists, else Move
    /// when `keychain_has_items` (`should_move`; macOS only) and the vault
    /// was not reset since, else Set.
    /// `profiles` are the saved connections, for Move.
    pub fn new(path: PathBuf, keychain_has_items: bool, profiles: Vec<ConnectionProfile>) -> Self {
        let from = keychain().filter(|_| keychain_has_items).map(|keychain| MoveFrom { keychain, profiles });
        Self::build(path, PARAMS, from)
    }

    /// For tests: Argon2's cost, and a Keychain to move from (Move mode when
    /// there is no vault).
    #[cfg(test)]
    pub fn for_test(path: PathBuf, params: KdfParams, keychain: Option<(Arc<dyn SecretBackend>, Vec<ConnectionProfile>)>) -> Self {
        Self::build(path, params, keychain.map(|(keychain, profiles)| MoveFrom { keychain, profiles }))
    }

    fn build(path: PathBuf, params: KdfParams, from: Option<MoveFrom>) -> Self {
        // After a Reset, nothing comes from the Keychain: Set, never Move.
        let from = from.filter(|_| !Vault::was_reset(&path));
        let mode = if Vault::exists(&path) {
            Mode::Unlock
        } else if from.is_some() {
            Mode::Move
        } else {
            Mode::Set
        };
        Self {
            path,
            mode,
            params,
            from,
            password: String::new(),
            again: String::new(),
            error: None,
            stuck: None,
            retry: false,
            working: None,
            focus: Some(0),
            #[cfg(test)]
            started: 0,
            #[cfg(test)]
            hold: None,
        }
    }

    /// With nowhere to keep a vault (no home folder): says so, offering Quit.
    pub fn nowhere() -> Self {
        let mut d = Self::build(PathBuf::new(), PARAMS, None);
        d.stuck = Some("There is no data folder to keep rongo's vault in.".into());
        d
    }

    /// With a message saying why it is asked (again).
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.error = Some(message.into());
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Draws the dialog; the answer once the vault is open, or Quit.
    pub fn show(&mut self, ctx: &Context) -> Option<UnlockResult> {
        if let Some((vault, keychain_left)) = self.poll() {
            return Some(UnlockResult::Unlocked { vault: Arc::new(vault), keychain_left });
        }
        let busy = self.working.is_some();
        let setting = self.mode != Mode::Unlock;
        let mut submit = false;
        let mut quit = false;
        let mut reset = false;
        let mut again = false;
        let screen = ctx.content_rect().size();
        Modal::new(Id::new("unlock")).show(ctx, |ui| {
            ui.set_width(440.0f32.min(screen.x - 40.0).max(240.0));
            let (title, intro) = match self.mode {
                Mode::Set => ("Set a Master Password", "rongo keeps saved connection passwords, SSH secrets and the Collection History key in one encrypted file, opened with a master password you type each time rongo starts."),
                Mode::Move => ("Set a Master Password", "rongo now keeps saved passwords under a master password instead of in the Keychain. Choose one, and your saved connection passwords, SSH secrets and Collection History key move from the Keychain into rongo's encrypted file. macOS may ask once to let rongo read them."),
                Mode::Unlock => ("Unlock rongo", "Type your master password to open your saved connections."),
            };
            ui.heading(format!("{}  {title}", icon::LOCK_KEY));
            ui.add_space(4.0);
            ui.label(intro);
            ui.add_space(8.0);
            if let Some(why) = &self.stuck {
                ui.label(RichText::new(why).color(theme::ERROR));
                // A failed move would stop every launch here: Reset… starts over without what the Keychain holds.
                let can_reset = self.mode == Mode::Move;
                if can_reset {
                    ui.add_space(4.0);
                    ui.label("Reset… starts over: it deletes rongo's items in the Keychain and asks for a new master password. Saved connections stay; their passwords must be typed again.");
                }
                ui.add_space(8.0);
                theme::dialog_buttons(ui, |ui| {
                    if can_reset && self.retry {
                        again = ui.add(theme::primary_button("Try Again", false)).clicked();
                    }
                    quit = ui.button("Quit").clicked();
                    if can_reset {
                        reset = ui.add(egui::Button::new(RichText::new("Reset…").color(theme::ERROR))).clicked();
                    }
                });
                return;
            }
            ui.label("Master password");
            let first = ui.add_enabled(!busy, TextEdit::singleline(&mut self.password).password(true).desired_width(f32::INFINITY));
            let second = setting.then(|| {
                ui.label("Type it again");
                ui.add_enabled(!busy, TextEdit::singleline(&mut self.again).password(true).desired_width(f32::INFINITY))
            });
            match (self.focus.take(), &second) {
                (Some(1), Some(s)) => s.request_focus(),
                (Some(_), _) => first.request_focus(),
                (None, _) => {}
            }
            if setting {
                ui.add_space(4.0);
                ui.label(RichText::new(format!("{}  A forgotten master password can't be recovered: you would have to reset it, losing every saved password and all Collection History.", icon::WARNING)).color(theme::WARN));
            }
            if let Some(e) = &self.error {
                ui.add_space(4.0);
                ui.label(RichText::new(e).color(theme::ERROR));
            }
            if busy {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(match self.mode {
                        Mode::Unlock => "Unlocking…",
                        Mode::Set => "Setting the master password…",
                        Mode::Move => "Moving your passwords out of the Keychain…",
                    });
                });
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if !setting {
                    let forgot = ui.add_enabled(!busy, egui::Button::new("Forgot?").frame(false));
                    reset = forgot.on_hover_text("A forgotten master password can't be recovered: reset it, losing every saved password; asks first").clicked();
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let label = if setting { "Set" } else { "Unlock" };
                    submit |= ui.add_enabled(!busy, theme::primary_button(label, false)).clicked();
                    quit = ui.add_enabled(!busy, egui::Button::new("Quit")).clicked();
                });
            });
            // Enter submits; from the first of two fields, the second still empty, it moves on to it.
            if !busy && ui.input(|i| i.key_pressed(Key::Enter)) {
                if setting && first.lost_focus() && self.again.is_empty() && !self.password.is_empty() {
                    self.focus = Some(1);
                } else {
                    submit = true;
                }
            }
        });
        if quit {
            return Some(UnlockResult::Quit);
        }
        if reset {
            return Some(UnlockResult::Reset);
        }
        // The move again, with what was typed (kept when it failed).
        if again {
            self.stuck = None;
            submit = true;
        }
        if submit {
            self.submit(ctx);
        }
        None
    }

    /// The finished derivation, if one finished: the vault, or what went
    /// wrong, shown on the dialog.
    fn poll(&mut self) -> Option<Opened> {
        let done = match self.working.as_ref()?.poll()? {
            Ok(done) => done,
            Err(_) => Err(CoreError::Other("the key derivation stopped".into())),
        };
        self.working = None;
        match done {
            Ok(v) => {
                self.password.clear();
                self.again.clear();
                return Some(v);
            }
            // Another window reset the vault meanwhile: say so and offer to
            // set a password, rather than repeating "Couldn't unlock".
            Err(_) if self.mode == Mode::Unlock && !Vault::exists(&self.path) => {
                self.mode = Mode::Set;
                self.error = Some(REMOVED.into());
                self.password.clear();
                self.again.clear();
                self.focus = Some(0);
            }
            Err(CoreError::WrongPassword) if self.mode == Mode::Unlock => {
                self.error = Some(WRONG.into());
                self.password.clear();
                self.focus = Some(0);
            }
            // Another rongo window set one first: never written over, unlocked instead.
            Err(CoreError::VaultExists) => {
                self.mode = Mode::Unlock;
                self.error = Some("A master password was just set in another rongo window. Unlock with that one.".into());
                self.password.clear();
                self.again.clear();
                self.focus = Some(0);
            }
            Err(e) if self.mode == Mode::Move => {
                // A damaged history key stays damaged; anything else (a refused read, a busy or full disk) may pass.
                self.retry = !matches!(e, CoreError::History(_));
                self.stuck = Some(format!("Couldn't move your passwords out of the Keychain: {e}. Nothing was deleted from the Keychain."));
            }
            Err(e) if self.mode == Mode::Set => self.error = Some(format!("Couldn't set the master password: {e}")),
            Err(e) => self.error = Some(format!("Couldn't unlock: {e}")),
        }
        None
    }

    /// Starts the derivation for what was typed, unless it is refused first.
    fn submit(&mut self, ctx: &Context) {
        if self.working.is_some() || self.stuck.is_some() {
            return;
        }
        let password = self.password.clone();
        let (path, params) = (self.path.clone(), self.params);
        let job: Work = match self.mode {
            Mode::Unlock => {
                if password.is_empty() {
                    self.error = Some("Type your master password.".into());
                    return;
                }
                Box::new(move || Vault::unlock(path, &password).map(|v| (v, None)))
            }
            Mode::Set | Mode::Move => {
                if let Err(e) = vault::check_new_password(&password) {
                    self.error = Some(sentence(&e));
                    return;
                }
                if password != self.again {
                    self.error = Some("The two passwords don't match.".into());
                    return;
                }
                match (&self.from, self.mode) {
                    (Some(from), Mode::Move) => {
                        let (keychain, profiles) = (from.keychain.clone(), from.profiles.clone());
                        Box::new(move || vault::keychain::move_out(keychain.as_ref(), &profiles, path, &password, params).map(|m| (m.vault, m.left)))
                    }
                    _ => Box::new(move || Vault::create(path, &password, params, BTreeMap::new()).map(|v| (v, None))),
                }
            }
        };
        #[cfg(test)]
        let job: Work = {
            self.started += 1;
            let hold = self.hold.clone();
            Box::new(move || {
                let _wait = hold.as_ref().map(|h| h.lock());
                job()
            })
        };
        self.error = None;
        match Job::try_start(ctx, "vault-key", move |_| job()) {
            Ok(j) => self.working = Some(j),
            Err(e) => self.error = Some(format!("Couldn't start: {e}")),
        }
    }
}

/// Folders and vaults of a test's own.
#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use egui::accesskit::Role;
    use egui_kittest::Harness;
    use egui_kittest::kittest::{NodeT, Queryable};
    use rongo_core::Namespace;
    use rongo_core::history::{HistoryKey, Kind, Store};
    use rongo_core::profile::{Auth, ConnectionProfile, SecretBackend, SecretKind, SshAuth, SshConfig};
    use rongo_core::vault::{KdfParams, Vault, vault_key};
    use uuid::Uuid;

    use crate::test_support::*;
    use super::*;

    struct Probe {
        dialog: UnlockDialog,
        result: Option<UnlockResult>,
    }

    fn run(dialog: UnlockDialog) -> Harness<'static, Probe> {
        crate::test_support::harness(egui::vec2(800.0, 600.0), 3, Probe { dialog, result: None }, |ctx, p| {
            if let Some(r) = p.dialog.show(ctx) {
                p.result = Some(r);
            }
        })
    }

    /// Steps frames until the derivation (on its own thread) is done and shown.
    fn settle(h: &mut Harness<'static, Probe>) {
        let end = Instant::now() + Duration::from_secs(30);
        h.run_steps(1);
        while h.state().dialog.working.is_some() && Instant::now() < end {
            h.step();
            std::thread::sleep(Duration::from_millis(5));
        }
        h.run_steps(2);
    }

    /// Types `text` into the `n`th password field, as a person would.
    fn type_in(h: &mut Harness<'static, Probe>, n: usize, text: &str) {
        h.get_all_by_role(Role::PasswordInput).nth(n).expect("a password field").focus();
        h.run_steps(1);
        h.get_all_by_role(Role::PasswordInput).nth(n).expect("a password field").type_text(text);
        h.run_steps(1);
    }

    fn fields(h: &Harness<'static, Probe>) -> usize {
        h.query_all_by_role(Role::PasswordInput).count()
    }

    fn has(h: &Harness<'static, Probe>, text: &str) -> bool {
        h.query_by_label_contains(text).is_some()
    }

    fn click(h: &mut Harness<'static, Probe>, label: &str) {
        h.get_by_label(label).click();
        h.run_steps(1);
    }

    fn unlocked(h: &mut Harness<'static, Probe>) -> Arc<Vault> {
        match h.state_mut().result.take() {
            Some(UnlockResult::Unlocked { vault, .. }) => vault,
            Some(UnlockResult::Quit) => panic!("quit instead"),
            Some(UnlockResult::Reset) => panic!("asked to reset instead"),
            None => panic!("no answer yet"),
        }
    }

    fn with_password(name: &str) -> ConnectionProfile {
        let mut p = ConnectionProfile::new(name);
        p.auth = Auth::Password { user: "u".into(), auth_db: "admin".into(), mechanism: None };
        p
    }

    /// No vault: Set asks for the password twice and refuses a mismatch or
    /// one too short (no vault is made, nothing is derived); a good one makes
    /// the vault, which then opens with exactly what was typed.
    #[test]
    fn first_launch_sets_a_password() {
        let t = TempDir::new();
        let mut h = run(UnlockDialog::for_test(t.vault(), KdfParams::TEST, None));
        assert_eq!(h.state().dialog.mode, Mode::Set);
        assert_eq!(fields(&h), 2);
        assert!(has(&h, "can't be recovered"));

        type_in(&mut h, 0, "a good password");
        type_in(&mut h, 1, "a good passw0rd");
        click(&mut h, "Set");
        settle(&mut h);
        assert!(has(&h, "don't match"));
        assert!(!t.vault().exists());

        h.state_mut().dialog.password = "1234567".into();
        h.state_mut().dialog.again = "1234567".into();
        click(&mut h, "Set");
        settle(&mut h);
        assert!(has(&h, "at least 8"));
        assert!(!t.vault().exists());
        assert_eq!(h.state().dialog.started, 0, "nothing derived for a refused password");
        assert!(h.state().result.is_none());

        // Thai and spaces, kept as typed.
        let pw = " รหัสผ่าน ดี ";
        h.state_mut().dialog.password = pw.into();
        h.state_mut().dialog.again = pw.into();
        click(&mut h, "Set");
        settle(&mut h);
        let v = unlocked(&mut h);
        assert_eq!(v.path(), t.vault());
        assert!(t.vault().exists());
        assert!(Vault::unlock(t.vault(), pw).is_ok());
        assert!(Vault::unlock(t.vault(), pw.trim()).is_err());
    }

    /// A wrong password says so, clears the field and stays in Unlock;
    /// the right one (Enter or the button) unlocks.
    #[test]
    fn unlock_says_a_wrong_password_and_lets_you_retry() {
        let (t, _) = temp_vault();
        let mut h = run(UnlockDialog::for_test(t.vault(), KdfParams::TEST, None));
        assert_eq!(h.state().dialog.mode, Mode::Unlock);
        assert_eq!(fields(&h), 1);
        assert!(!has(&h, "can't be recovered") && has(&h, "Forgot?"));

        type_in(&mut h, 0, "not the password");
        h.key_press(egui::Key::Enter);
        settle(&mut h);
        assert!(has(&h, "Wrong master password"));
        assert_eq!(h.state().dialog.mode, Mode::Unlock);
        assert!(h.state().dialog.password.is_empty(), "the field is cleared");
        assert!(h.state().result.is_none());

        type_in(&mut h, 0, TEST_PASSWORD);
        click(&mut h, "Unlock");
        settle(&mut h);
        let v = unlocked(&mut h);
        assert_eq!(v.path(), t.vault());
        assert_eq!(h.state().dialog.started, 2);
    }

    /// The vault was removed by another window: trying to unlock says it is
    /// gone and offers to set a password, instead of repeating "Couldn't
    /// unlock". A new password makes a new vault.
    #[test]
    fn unlock_switches_to_set_when_the_vault_is_gone() {
        let (t, _) = temp_vault();
        let mut h = run(UnlockDialog::for_test(t.vault(), KdfParams::TEST, None));
        assert_eq!(h.state().dialog.mode, Mode::Unlock);
        std::fs::remove_file(t.vault()).unwrap();
        type_in(&mut h, 0, TEST_PASSWORD);
        h.key_press(egui::Key::Enter);
        settle(&mut h);
        assert_eq!(h.state().dialog.mode, Mode::Set);
        assert!(has(&h, "removed in another rongo window"));
        assert!(!has(&h, "Couldn't unlock"));
        assert_eq!(fields(&h), 2);
        assert!(h.state().dialog.password.is_empty() && h.state().result.is_none());

        type_in(&mut h, 0, "a brand new password");
        type_in(&mut h, 1, "a brand new password");
        click(&mut h, "Set");
        settle(&mut h);
        let v = unlocked(&mut h);
        assert!(Vault::unlock(v.path().to_path_buf(), "a brand new password").is_ok());
    }

    /// Forgot? asks the app for the Reset question; it opens nothing and
    /// derives nothing itself. Setting a password offers no Forgot?.
    #[test]
    fn forgot_asks_for_a_reset() {
        let (t, _) = temp_vault();
        let mut h = run(UnlockDialog::for_test(t.vault(), KdfParams::TEST, None));
        click(&mut h, "Forgot?");
        assert!(matches!(h.state().result, Some(UnlockResult::Reset)));
        assert_eq!(h.state().dialog.started, 0);
        assert!(t.vault().exists(), "the dialog deletes nothing itself");

        let t = TempDir::new();
        let h = run(UnlockDialog::for_test(t.vault(), KdfParams::TEST, None));
        assert_eq!(h.state().dialog.mode, Mode::Set);
        assert!(!has(&h, "Forgot?") && h.query_by_label("Reset…").is_none());
    }

    #[test]
    fn quit_quits() {
        let t = TempDir::new();
        let mut h = run(UnlockDialog::for_test(t.vault(), KdfParams::TEST, None));
        click(&mut h, "Quit");
        assert!(matches!(h.state().result, Some(UnlockResult::Quit)));
        assert!(!t.vault().exists(), "Quit makes nothing");

        let (t, _) = temp_vault();
        let mut h = run(UnlockDialog::for_test(t.vault(), KdfParams::TEST, None));
        click(&mut h, "Quit");
        assert!(matches!(h.state().result, Some(UnlockResult::Quit)));
    }

    /// While the key is derived the dialog shows a spinner and takes no
    /// input: another Enter or click starts no second derivation.
    #[test]
    fn the_ui_waits_while_the_key_is_derived() {
        let (t, _) = temp_vault();
        let mut d = UnlockDialog::for_test(t.vault(), KdfParams::TEST, None);
        let gate = Arc::new(Mutex::new(()));
        let held = gate.lock().unwrap();
        d.hold = Some(gate.clone());
        let mut h = run(d);
        type_in(&mut h, 0, TEST_PASSWORD);
        click(&mut h, "Unlock");
        h.run_steps(2);
        assert_eq!(h.state().dialog.started, 1);
        assert!(has(&h, "Unlocking"));
        let spinner = h.output().shapes.iter().any(|s| matches!(&s.shape, egui::Shape::Path(_)));
        assert!(spinner, "a spinner is drawn");
        assert!(h.get_by_label("Unlock").accesskit_node().is_disabled());

        h.key_press(egui::Key::Enter);
        h.run_steps(2);
        h.get_by_label("Unlock").click();
        h.run_steps(2);
        assert_eq!(h.state().dialog.started, 1, "no second derivation");
        assert!(h.state().result.is_none());

        drop(held);
        settle(&mut h);
        unlocked(&mut h);
        assert_eq!(h.state().dialog.started, 1);
    }

    /// Moving out of the Keychain: the dialog says so, and Set makes the
    /// vault from what the Keychain holds, then deletes rongo's items there.
    #[test]
    fn move_brings_the_keychain_into_the_vault() {
        let t = TempDir::new();
        let p = with_password("old");
        let hk = HistoryKey::new([3; 32]);
        let mem = Arc::new(Mem::default());
        let item = BTreeMap::from([(vault_key(p.id, SecretKind::Password), "pw".to_string()), (vault_key(Uuid::nil(), SecretKind::HistoryKey), "03".repeat(32))]);
        mem.put(KEYCHAIN_VAULT, &serde_json::to_string(&item).unwrap());
        mem.put("something else", "x");
        let keychain: Arc<dyn SecretBackend> = mem.clone();
        let mut h = run(UnlockDialog::for_test(t.vault(), KdfParams::TEST, Some((keychain, vec![p.clone()]))));
        assert_eq!(h.state().dialog.mode, Mode::Move);
        assert!(has(&h, "Keychain") && has(&h, "can't be recovered"));
        assert_eq!(fields(&h), 2);

        type_in(&mut h, 0, "a good password");
        type_in(&mut h, 1, "a good password");
        click(&mut h, "Set");
        settle(&mut h);
        let v = unlocked(&mut h);
        assert_eq!(v.get(p.id, SecretKind::Password).as_deref(), Some("pw"));
        assert_eq!(v.history_key(), hk);
        assert_eq!(mem.accounts(), ["something else"]);
        assert_eq!(Vault::unlock(t.vault(), "a good password").unwrap().get(p.id, SecretKind::Password).as_deref(), Some("pw"));
    }

    /// A move that fails (a damaged history key in the Keychain) says why
    /// and offers Reset… (so no one is stuck there) and Quit; nothing is
    /// made or deleted until one is chosen. With nowhere to keep a vault
    /// there is nothing to reset: only Quit.
    #[test]
    fn a_failed_move_says_why_and_offers_reset_or_quit() {
        let failed = || {
            let t = TempDir::new();
            let mem = Arc::new(Mem::default());
            mem.put(KEYCHAIN_VAULT, &serde_json::to_string(&BTreeMap::from([(vault_key(Uuid::nil(), SecretKind::HistoryKey), "damaged")])).unwrap());
            let keychain: Arc<dyn SecretBackend> = mem.clone();
            let mut h = run(UnlockDialog::for_test(t.vault(), KdfParams::TEST, Some((keychain, Vec::new()))));
            h.state_mut().dialog.password = "a good password".into();
            h.state_mut().dialog.again = "a good password".into();
            click(&mut h, "Set");
            settle(&mut h);
            (t, mem, h)
        };
        let (t, mem, mut h) = failed();
        assert!(has(&h, "Couldn't move") && has(&h, "damaged"), "says why");
        assert!(h.query_by_label("Set").is_none() && fields(&h) == 0, "no password asked");
        assert!(h.query_by_label("Reset…").is_some() && h.query_by_label("Quit").is_some());
        assert!(h.query_by_label("Try Again").is_none(), "a damaged key stays damaged: no Try Again");
        assert!(h.state().result.is_none());
        assert!(!t.vault().exists());
        assert_eq!(mem.accounts(), [KEYCHAIN_VAULT], "nothing deleted");
        click(&mut h, "Quit");
        assert!(matches!(h.state().result, Some(UnlockResult::Quit)));

        let (_t, mem, mut h) = failed();
        click(&mut h, "Reset…");
        assert!(matches!(h.state().result, Some(UnlockResult::Reset)));
        assert_eq!(mem.accounts(), [KEYCHAIN_VAULT], "the app asks first; the dialog deletes nothing");

        let h = run(UnlockDialog::nowhere());
        assert!(has(&h, "no data folder"));
        assert!(h.query_by_label("Reset…").is_none() && !has(&h, "Forgot?") && h.query_by_label("Quit").is_some());
    }

    /// A move that failed for a reason that may pass (here the vault's
    /// folder was a file) offers Try Again besides Reset… and Quit; it runs
    /// the move again with what was typed, and once it works the Keychain's
    /// secrets are in the vault and gone from the Keychain.
    #[test]
    fn a_failed_move_can_be_tried_again() {
        let t = TempDir::new();
        let blocker = t.0.join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let p = with_password("old");
        let mem = Arc::new(Mem::default());
        mem.put(&vault_key(p.id, SecretKind::Password), "pw");
        let keychain: Arc<dyn SecretBackend> = mem.clone();
        let mut h = run(UnlockDialog::for_test(blocker.join("vault"), KdfParams::TEST, Some((keychain, vec![p.clone()]))));
        type_in(&mut h, 0, "a good password");
        type_in(&mut h, 1, "a good password");
        click(&mut h, "Set");
        settle(&mut h);
        assert!(has(&h, "Couldn't move"), "the move failed");
        assert!(h.query_by_label("Try Again").is_some() && h.query_by_label("Reset…").is_some() && h.query_by_label("Quit").is_some());
        assert_eq!(mem.accounts().len(), 1, "nothing deleted");

        std::fs::remove_file(&blocker).unwrap();
        click(&mut h, "Try Again");
        settle(&mut h);
        let v = unlocked(&mut h);
        assert_eq!(v.get(p.id, SecretKind::Password).as_deref(), Some("pw"));
        assert_eq!(h.state().dialog.started, 2, "the move ran again");
        assert!(mem.accounts().is_empty(), "moved, then deleted from the Keychain");
    }

    /// A vault set in another window while this one was setting: this one
    /// doesn't overwrite it but asks to unlock it.
    #[test]
    fn a_vault_set_meanwhile_is_unlocked_not_replaced() {
        let t = TempDir::new();
        let mut h = run(UnlockDialog::for_test(t.vault(), KdfParams::TEST, None));
        Vault::create(t.vault(), TEST_PASSWORD, KdfParams::TEST, Default::default()).unwrap();
        h.state_mut().dialog.password = "another password".into();
        h.state_mut().dialog.again = "another password".into();
        click(&mut h, "Set");
        settle(&mut h);
        assert_eq!(h.state().dialog.mode, Mode::Unlock);
        assert!(has(&h, "another rongo window"));
        assert_eq!(fields(&h), 1);
        assert!(Vault::unlock(t.vault(), TEST_PASSWORD).is_ok(), "the other window's vault stays");
    }

    /// Moving is offered (on macOS) only when there is no vault and older
    /// versions may have kept something in the Keychain: a profile with a
    /// password or SSH, or Collection History (its key was there). Looking
    /// reads no Keychain.
    #[test]
    fn moving_is_offered_only_when_older_secrets_may_be_in_the_keychain() {
        let t = TempDir::new();
        let history = t.0.join("history");
        let mac = cfg!(target_os = "macos");
        let plain = ConnectionProfile::new("plain");
        assert!(!should_move(&t.vault(), Some(std::slice::from_ref(&plain)), &history));
        assert_eq!(should_move(&t.vault(), Some(&[plain.clone(), with_password("pw")]), &history), mac);
        let mut ssh = ConnectionProfile::new("ssh");
        ssh.ssh = Some(SshConfig { host: "h".into(), port: 22, user: "u".into(), auth: SshAuth::Password });
        assert_eq!(should_move(&t.vault(), Some(std::slice::from_ref(&ssh)), &history), mac);

        let ns = Namespace::new("d", "c");
        let mut w = Store::new(history.clone(), HistoryKey::new([1; 32])).begin(plain.id, "plain", &ns, Kind::Delete, Default::default()).unwrap();
        w.save(&[rongo_core::bson::RawDocumentBuf::try_from(&rongo_core::bson::doc! { "_id": 1 }).unwrap()]).unwrap();
        w.finish(false).unwrap();
        assert_eq!(should_move(&t.vault(), Some(std::slice::from_ref(&plain)), &history), mac);

        Vault::create(t.vault(), TEST_PASSWORD, KdfParams::TEST, Default::default()).unwrap();
        assert!(!should_move(&t.vault(), Some(&[with_password("pw"), ssh]), &history), "a vault is there: nothing to move");
    }

    /// Collection History that can't be measured (here a file where its
    /// folder should be) may be there, its key in the Keychain: moving is
    /// offered (on macOS), as for connections that can't be read.
    #[test]
    fn unmeasurable_history_still_offers_moving() {
        let t = TempDir::new();
        let history = t.0.join("history");
        std::fs::write(&history, b"not a folder").unwrap();
        assert!(Store::new(history.clone(), HistoryKey::new([0; 32])).usage().is_err(), "it can't be measured");
        let plain = ConnectionProfile::new("plain");
        assert_eq!(should_move(&t.vault(), Some(std::slice::from_ref(&plain)), &history), cfg!(target_os = "macos"));
    }

    /// Saved connections that couldn't be read (a damaged profiles.json)
    /// may have kept passwords in the Keychain, so moving is offered (on
    /// macOS) all the same, with no history either; not after a Reset, and
    /// not once a vault is there.
    #[test]
    fn unreadable_connections_still_offer_moving() {
        let t = TempDir::new();
        let history = t.0.join("history");
        let mac = cfg!(target_os = "macos");
        assert_eq!(should_move(&t.vault(), None, &history), mac);
        Vault::reset(&t.vault()).unwrap();
        assert!(!should_move(&t.vault(), None, &history), "not after a Reset");
        Vault::create(t.vault(), TEST_PASSWORD, KdfParams::TEST, Default::default()).unwrap();
        assert!(!Vault::was_reset(&t.vault()));
        assert!(!should_move(&t.vault(), None, &history), "a vault is there: nothing to move");
    }
}
