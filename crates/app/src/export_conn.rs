//! Export Connection: one saved connection as a `mongodb://` string to give
//! to someone else, who pastes it into New Connection (rongo or Studio 3T).

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use egui::{Context, Id, Modal, RichText};
use egui_phosphor::regular as icon;
use rongo_core::fsx;
use rongo_core::profile::{self, ConnectionProfile, SecretKind};
use rongo_core::vault::Vault;
use rongo_core::{CoreError, CoreResult};
use uuid::Uuid;

use crate::theme;
use crate::vault_ui::{CatchUp, catch_up};

/// Reads a connection's saved password (the vault; tests pass their own).
type ReadSecret = Box<dyn Fn(Uuid) -> CoreResult<Option<String>>>;

/// Reads from `vault`, caught up first with what another rongo window saved
/// since. A vault changed elsewhere is `VaultChanged` (it must be unlocked
/// again); one busy or unreadable just now is that error, which the dialog
/// shows where the password would be (it has no toast of its own).
fn from_vault(vault: Arc<Vault>) -> ReadSecret {
    Box::new(move |id| match catch_up(&vault) {
        CatchUp::Fresh => Ok(vault.get(id, SecretKind::Password)),
        CatchUp::Changed => Err(CoreError::VaultChanged),
        CatchUp::Failed(e) => Err(e),
    })
}

pub struct ExportConnDialog {
    profile: ConnectionProfile,
    /// The profile has a username (password auth, or one in its URI).
    has_user: bool,
    include_user: bool,
    include_password: bool,
    /// The saved password, read the first time Include password is ticked
    /// (again after a failed read, once it is unticked and ticked).
    secret: Option<CoreResult<Option<String>>>,
    read_secret: ReadSecret,
    /// What Copy or Save did (`true`: it failed).
    status: Option<(String, bool)>,
}

impl ExportConnDialog {
    pub fn new(profile: ConnectionProfile, vault: Arc<Vault>) -> Self {
        Self::with_secret(profile, from_vault(vault))
    }

    fn with_secret(profile: ConnectionProfile, read_secret: impl Fn(Uuid) -> CoreResult<Option<String>> + 'static) -> Self {
        let has_user = profile.uses_password();
        Self { profile, has_user, include_user: has_user, include_password: false, secret: None, read_secret: Box::new(read_secret), status: None }
    }

    /// Reading the password found the vault changed in another rongo
    /// window: it must be unlocked again.
    pub fn vault_changed(&self) -> bool {
        matches!(self.secret, Some(Err(CoreError::VaultChanged)))
    }

    fn password(&self) -> Option<&str> {
        if !(self.has_user && self.include_user && self.include_password) {
            return None;
        }
        self.secret.as_ref()?.as_ref().ok()?.as_deref()
    }

    /// The connection string as the boxes say.
    pub fn uri(&self) -> String {
        profile::export_uri(&self.profile, self.has_user && self.include_user, self.password())
    }

    /// Draws the dialog; `true` when it should close.
    pub fn show(&mut self, ctx: &Context) -> bool {
        let mut close = false;
        let screen = ctx.content_rect().size();
        let modal = Modal::new(Id::new("export-connection")).show(ctx, |ui| {
            ui.set_width(560.0f32.min(screen.x - 40.0).max(240.0));
            ui.heading(format!("{}  Export Connection", icon::EXPORT));
            ui.add_space(2.0);
            let name = RichText::new(&self.profile.name).strong();
            ui.label(match theme::conn_color(self.profile.color.as_deref()) {
                Some(c) => name.color(c),
                None => name,
            });
            ui.add_space(6.0);
            let no_user = "This connection has no username";
            let user = ui.add_enabled(self.has_user, egui::Checkbox::new(&mut self.include_user, "Include username")).on_disabled_hover_text(no_user);
            let pass = ui
                .add_enabled(self.has_user && self.include_user, egui::Checkbox::new(&mut self.include_password, "Include password"))
                .on_disabled_hover_text(if self.has_user { "Include the username first" } else { no_user });
            if user.changed() || pass.changed() {
                self.status = None;
            }
            // A read that failed (busy just now) is tried again on the next tick.
            if pass.changed() && !self.include_password && matches!(self.secret, Some(Err(_))) {
                self.secret = None;
            }
            if self.include_password && self.has_user && self.include_user {
                let id = self.profile.id;
                match self.secret.get_or_insert_with(|| (self.read_secret)(id)) {
                    Ok(Some(_)) => {
                        ui.label(RichText::new(format!("{}  The password is in the string as readable text: anyone who sees it can sign in. Send it only where you trust.", icon::WARNING)).color(theme::WARN));
                    }
                    Ok(None) => _ = ui.weak("No password is saved for this connection."),
                    Err(e) => _ = ui.label(RichText::new(format!("Couldn't read the password from the vault: {e}")).color(theme::ERROR)),
                }
            }
            ui.add_space(6.0);
            let mut uri = self.uri();
            ui.add(egui::TextEdit::multiline(&mut uri.as_str()).code_editor().desired_rows(3).desired_width(f32::INFINITY));
            if self.profile.ssh.is_some() {
                ui.label(RichText::new(format!("{}  SSH tunnel settings aren't included: whoever uses this string sets the tunnel up in the SSH tab.", icon::INFO)).color(theme::WARN));
            }
            if profile::has_tls_files(&self.profile) {
                ui.label(RichText::new(format!("{}  TLS certificate files aren't included: they are files on this computer.", icon::INFO)).color(theme::WARN));
            }
            ui.weak("Paste it into New Connection (URI) in rongo or Studio 3T.");
            ui.add_space(8.0);
            theme::dialog_buttons(ui, |ui| {
                if ui.add(theme::primary_button(format!("{}  Copy", icon::COPY), false)).clicked() {
                    ui.ctx().copy_text(std::mem::take(&mut uri));
                    self.status = Some(("Copied".into(), false));
                }
                if ui.button(format!("{}  Save to File…", icon::FLOPPY_DISK)).clicked() {
                    let file = suggested_file_name(&self.profile.name);
                    if let Some(path) = rfd::FileDialog::new().set_file_name(file).save_file() {
                        self.status = Some(match save_text(&path, &self.uri(), self.password().is_some()) {
                            Ok(()) => (format!("Saved to {}", path.display()), false),
                            Err(e) => (format!("Couldn't save: {e}"), true),
                        });
                    }
                }
                if ui.button("Close").clicked() {
                    close = true;
                }
                if let Some((text, failed)) = &self.status {
                    theme::footer_note(ui, RichText::new(text).color(if *failed { theme::ERROR } else { theme::OK }));
                }
            });
        });
        close || modal.should_close()
    }
}

/// The file name the save dialog starts with: the connection's name and
/// `.txt`, with every character a file system refuses (`/ \ : * ? " < > |`
/// and control characters, on every OS alike) written as `-`.
fn suggested_file_name(profile_name: &str) -> String {
    let name: String = profile_name.chars().map(|c| if c.is_control() || "/\\:*?\"<>|".contains(c) { '-' } else { c }).collect();
    format!("{name}.txt")
}

/// Writes `text` and a newline to `path`, replacing any file there (a
/// symlink there is replaced, not followed); with `private` (a password is
/// in it) the file is readable only by you. The file is written whole
/// beside `path` and renamed over it, so a save that fails leaves what was
/// there as it was, and no other file.
pub fn save_text(path: &Path, text: &str, private: bool) -> std::io::Result<()> {
    use std::io::Write;
    replace_through(path, &temp_beside(path)?, private, &STEPS, |f| f.write_all(format!("{text}\n").as_bytes()))
}

/// What a refused `set_private` is told to do about it.
const PRIVATE_HINT: &str = "Save it on a local drive, or export without the password.";

/// The steps `replace_through` takes on the disk, apart so the tests can
/// make each fail.
struct Steps {
    /// Makes the temporary file, which must not exist (`create_new`), private with `bool`.
    create: fn(&Path, bool) -> std::io::Result<File>,
    /// Makes an open file private (strict: see `fsx::set_private`).
    narrow: fn(&File) -> std::io::Result<()>,
    rename: fn(&Path, &Path) -> std::io::Result<()>,
}

const STEPS: Steps = Steps { create: create_new, narrow: fsx::set_private, rename: fsx::rename };

fn create_new(tmp: &Path, private: bool) -> std::io::Result<File> {
    let mut opts = if private { fsx::private_options() } else { OpenOptions::new() };
    opts.write(true).create_new(true).open(tmp)
}

/// A name for this save's own temporary file in `path`'s folder: `.<file name>.<pid>.<n>.tmp`.
fn temp_beside(path: &Path) -> std::io::Result<PathBuf> {
    static SAVES: AtomicU32 = AtomicU32::new(0);
    let name = path.file_name().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no file name to save to"))?;
    Ok(path.with_file_name(format!(".{}.{}.{}.tmp", name.to_string_lossy(), std::process::id(), SAVES.fetch_add(1, Ordering::Relaxed))))
}

/// Makes a new file `tmp` (private with `private`), makes it private
/// *before* anything is written to it (strict: a refusal is the error, with
/// `PRIVATE_HINT`), lets `write` fill it, syncs it and renames it over
/// `path`: `path` (or the symlink there) is the old file until the rename
/// has worked. Any failure removes `tmp`, best effort, once it was made
/// (a file already at `tmp` isn't ours and is left alone), and is the answer.
fn replace_through(path: &Path, tmp: &Path, private: bool, steps: &Steps, write: impl FnOnce(&mut File) -> std::io::Result<()>) -> std::io::Result<()> {
    let mut f = (steps.create)(tmp, private)?;
    let written = (|| {
        if private {
            (steps.narrow)(&f).map_err(|e| std::io::Error::new(e.kind(), format!("{}. {PRIVATE_HINT}", e.to_string().trim_end_matches('.'))))?;
        }
        write(&mut f)?;
        f.sync_all()
    })();
    drop(f);
    match written.and_then(|()| (steps.rename)(tmp, path)) {
        Ok(()) => {
            if let Some(dir) = path.parent() {
                fsx::sync_dir(dir);
            }
            Ok(())
        }
        Err(e) => {
            let _ = fsx::remove_file(tmp);
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::Harness;
    use egui_kittest::kittest::Queryable;
    use rongo_core::profile::{Auth, SshAuth, SshConfig, Target};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::test_support::{TEST_PASSWORD, disabled, temp_vault};

    /// The suggested file name is a name every OS takes: what Windows (and
    /// macOS, for `/` and `:`) refuse in one is replaced, whatever OS this is.
    #[test]
    fn the_suggested_file_name_has_no_characters_a_file_system_refuses() {
        assert_eq!(suggested_file_name("sit"), "sit.txt");
        assert_eq!(suggested_file_name("prod: a/b"), "prod- a-b.txt");
        assert_eq!(suggested_file_name(r#"a\b*c?d"e<f>g|h"#), "a-b-c-d-e-f-g-h.txt");
        assert_eq!(suggested_file_name("tab\tnew\nline\u{7f}"), "tab-new-line-.txt");
        assert_eq!(suggested_file_name("ภาษาไทย (SIT) 2"), "ภาษาไทย (SIT) 2.txt");
    }

    fn profile() -> ConnectionProfile {
        let mut p = ConnectionProfile::new("sit");
        p.target = Target::Manual { hosts: vec!["h:27017".into()], replica_set: None, direct: false };
        p.auth = Auth::Password { user: "app".into(), auth_db: "admin".into(), mechanism: None };
        p
    }

    fn run(d: ExportConnDialog) -> Harness<'static, ExportConnDialog> {
        crate::test_support::harness(egui::vec2(800.0, 600.0), 4, d, |ctx, d| {
            _ = d.show(ctx);
        })
    }

    fn click(h: &mut Harness<'static, ExportConnDialog>, label: &str) {
        h.get_by_label(label).click();
        h.run_steps(3);
    }

    /// The username is in by default and the password only once ticked: the
    /// vault is read then (once), not before, and a warning says the
    /// password travels readable. Without the username the password goes too.
    #[test]
    fn the_password_is_read_and_added_only_when_ticked() {
        static READS: AtomicUsize = AtomicUsize::new(0);
        fn saved(_: Uuid) -> CoreResult<Option<String>> {
            READS.fetch_add(1, Ordering::SeqCst);
            Ok(Some("s3cr@t".into()))
        }
        let mut h = run(ExportConnDialog::with_secret(profile(), saved));
        assert_eq!(h.state().uri(), "mongodb://app@h:27017/?authSource=admin");
        assert_eq!(READS.load(Ordering::SeqCst), 0, "no vault read until asked");
        assert!(h.query_by_label_contains("readable").is_none());
        click(&mut h, "Include password");
        assert_eq!(h.state().uri(), "mongodb://app:s3cr%40t@h:27017/?authSource=admin");
        assert!(h.query_by_label_contains("readable").is_some(), "warns that the password travels readable");
        click(&mut h, "Include password");
        click(&mut h, "Include password");
        assert_eq!(READS.load(Ordering::SeqCst), 1, "read once");
        click(&mut h, "Include username");
        assert_eq!(h.state().uri(), "mongodb://h:27017", "no username: no password or auth options either");
        assert!(disabled(&h, "Include password"));
        assert!(h.query_by_label_contains("readable").is_none());
    }

    /// No saved password, or a vault that can't be read, is said in the
    /// dialog and leaves the password out.
    #[test]
    fn a_missing_or_unreadable_password_is_said() {
        fn none(_: Uuid) -> CoreResult<Option<String>> {
            Ok(None)
        }
        fn refused(_: Uuid) -> CoreResult<Option<String>> {
            Err(CoreError::Other("user denied access".into()))
        }
        let mut h = run(ExportConnDialog::with_secret(profile(), none));
        click(&mut h, "Include password");
        assert_eq!(h.state().uri(), "mongodb://app@h:27017/?authSource=admin");
        assert!(h.query_by_label_contains("No password is saved").is_some());
        let mut h = run(ExportConnDialog::with_secret(profile(), refused));
        click(&mut h, "Include password");
        assert_eq!(h.state().uri(), "mongodb://app@h:27017/?authSource=admin");
        assert!(h.query_by_label_contains("user denied access").is_some());
    }

    /// A connection without a username has nothing to include: both boxes
    /// are off and the vault is never read.
    #[test]
    fn a_connection_without_a_username_offers_no_credentials() {
        static READS: AtomicUsize = AtomicUsize::new(0);
        fn saved(_: Uuid) -> CoreResult<Option<String>> {
            READS.fetch_add(1, Ordering::SeqCst);
            Ok(Some("pw".into()))
        }
        let mut p = profile();
        p.auth = Auth::None;
        let mut h = run(ExportConnDialog::with_secret(p, saved));
        assert!(disabled(&h, "Include username") && disabled(&h, "Include password"));
        click(&mut h, "Include password");
        assert_eq!(h.state().uri(), "mongodb://h:27017");
        assert_eq!(READS.load(Ordering::SeqCst), 0);
    }

    /// The password comes from the vault, as another rongo window may have
    /// saved it since this one unlocked; a vault whose password was changed
    /// there is reported (to unlock again), and no password goes in.
    #[test]
    fn the_password_comes_from_the_vault_as_saved_elsewhere() {
        let (_dir, vault) = temp_vault();
        let p = profile();
        let mut h = run(ExportConnDialog::new(p.clone(), vault.clone()));
        let other = rongo_core::vault::Vault::unlock(vault.path().to_path_buf(), TEST_PASSWORD).unwrap();
        other.set(p.id, SecretKind::Password, "s3cr@t").unwrap();
        click(&mut h, "Include password");
        assert_eq!(h.state().uri(), "mongodb://app:s3cr%40t@h:27017/?authSource=admin");
        assert!(!h.state().vault_changed());

        let mut h = run(ExportConnDialog::new(p, vault));
        other.change_password(TEST_PASSWORD, "another password").unwrap();
        click(&mut h, "Include password");
        assert!(h.state().vault_changed());
        assert_eq!(h.state().uri(), "mongodb://app@h:27017/?authSource=admin");
        assert!(h.query_by_label_contains("changed by another rongo window").is_some());
    }

    /// A vault another window holds locked is reported in the dialog (not
    /// taken for a change, not silent), and no password goes in. Unticking
    /// and ticking again reads it again.
    #[test]
    fn a_busy_vault_is_reported() {
        let (_dir, vault) = temp_vault();
        let p = profile();
        vault.set(p.id, SecretKind::Password, "s3cr@t").unwrap();
        let mut h = run(ExportConnDialog::new(p, vault.clone()));
        let held = std::fs::File::options().read(true).write(true).create(true).truncate(false).open(vault.path().with_extension("lock")).unwrap();
        held.lock().unwrap();
        click(&mut h, "Include password");
        drop(held);
        assert!(!h.state().vault_changed());
        assert!(h.query_by_label_contains("busy").is_some());
        assert_eq!(h.state().uri(), "mongodb://app@h:27017/?authSource=admin");

        click(&mut h, "Include password");
        click(&mut h, "Include password");
        assert!(h.query_by_label_contains("busy").is_none());
        assert_eq!(h.state().uri(), "mongodb://app:s3cr%40t@h:27017/?authSource=admin", "read again");
    }

    /// SSH and TLS certificate files can't travel in a connection string:
    /// the dialog says so.
    #[test]
    fn says_what_is_left_out() {
        fn none(_: Uuid) -> CoreResult<Option<String>> {
            Ok(None)
        }
        let h = run(ExportConnDialog::with_secret(profile(), none));
        assert!(h.query_by_label_contains("SSH").is_none() && h.query_by_label_contains("certificate").is_none());
        let mut p = profile();
        p.ssh = Some(SshConfig { host: "bastion".into(), port: 22, user: "ec2".into(), auth: SshAuth::Password });
        p.tls.enabled = true;
        p.tls.ca_file = Some("/Users/me/ca.pem".into());
        let h = run(ExportConnDialog::with_secret(p, none));
        assert!(h.query_by_label_contains("SSH tunnel").is_some());
        assert!(h.query_by_label_contains("certificate files").is_some());
        let mut typed = profile();
        typed.target = Target::Uri { uri: "mongodb://h:27017/?tls=true&tlsCAFile=/Users/me/ca.pem".into() };
        let h = run(ExportConnDialog::with_secret(typed, none));
        assert!(h.query_by_label_contains("certificate files").is_some(), "files named in the URI are left out too");
    }

    /// Copy says it copied.
    #[test]
    fn copy_says_so() {
        fn none(_: Uuid) -> CoreResult<Option<String>> {
            Ok(None)
        }
        let mut h = run(ExportConnDialog::with_secret(profile(), none));
        assert!(h.query_by_label_contains("Copied").is_none());
        h.get_by_label_contains("Copy").click();
        h.run_steps(3);
        assert!(h.query_by_label_contains("Copied").is_some());
    }

    fn export_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rongo-export-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut n: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        n.sort();
        n
    }

    fn fails(what: &str) -> std::io::Error {
        std::io::Error::other(what.to_string())
    }

    /// A save that fails at any step (creating the temporary file, making it
    /// private, writing it, renaming it over the target) leaves the file
    /// that was there whole, and no temporary file, for a private save and
    /// a plain one.
    #[test]
    fn a_failed_save_leaves_the_old_file_whole_and_no_temporary_file() {
        use std::io::Write;
        let dir = export_dir();
        let old = dir.join("old.txt");
        let steps: [(&str, Steps); 4] = [
            ("no room to create", Steps { create: |_, _| Err(fails("no room to create")), ..STEPS }),
            ("cannot narrow", Steps { narrow: |_| Err(fails("cannot narrow")), ..STEPS }),
            ("the disk is full", STEPS),
            ("held by antivirus", Steps { rename: |_, _| Err(fails("held by antivirus")), ..STEPS }),
        ];
        for private in [true, false] {
            for (said, steps) in &steps {
                // `narrow` is only asked of a private save.
                if *said == "cannot narrow" && !private {
                    continue;
                }
                std::fs::write(&old, "a connection string saved before").unwrap();
                let failed = replace_through(&old, &dir.join(".old.txt.tmp"), private, steps, |f| {
                    f.write_all(b"mongodb://app:p")?;
                    if *said == "the disk is full" { Err(fails(said)) } else { Ok(()) }
                });
                let e = failed.unwrap_err().to_string();
                assert!(e.starts_with(said), "{e}");
                assert_eq!(std::fs::read_to_string(&old).unwrap(), "a connection string saved before", "{said}, private {private}");
                assert_eq!(names(&dir), ["old.txt"], "{said}, private {private}: no temporary file left");
            }
        }
        // And with nothing there yet: no file appears.
        std::fs::remove_file(&old).unwrap();
        let failed = replace_through(&old, &dir.join(".old.txt.tmp"), true, &Steps { rename: |_, _| Err(fails("held")), ..STEPS }, |_| Ok(()));
        assert!(failed.is_err());
        assert!(names(&dir).is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A private file is made private before a byte is written to it, and a
    /// refusal says what to do. A plain save never asks.
    #[test]
    fn a_private_file_is_narrowed_before_anything_is_written_and_a_refusal_says_what_to_do() {
        use std::cell::Cell;
        let dir = export_dir();
        let target = dir.join("out.txt");
        let written = Cell::new(false);
        let refused = Steps { narrow: |_| Err(std::io::Error::from_raw_os_error(1)), ..STEPS };
        let e = replace_through(&target, &dir.join(".out.tmp"), true, &refused, |_| {
            written.set(true);
            Ok(())
        })
        .unwrap_err();
        assert!(!written.get(), "nothing written to a file that could not be made private");
        let said = e.to_string();
        assert!(said.ends_with(". Save it on a local drive, or export without the password."), "{said}");
        assert!(said.starts_with(&std::io::Error::from_raw_os_error(1).to_string().trim_end_matches('.').to_string()), "what the OS said comes first: {said}");
        assert!(names(&dir).is_empty());
        let never = Steps { narrow: |_| panic!("a plain save is not made private"), ..STEPS };
        replace_through(&target, &dir.join(".out.tmp"), false, &never, |f| std::io::Write::write_all(f, b"plain")).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "plain");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A file already at the temporary name isn't ours: the save fails and
    /// leaves it, and the old file, alone. So does a target that is a folder.
    #[test]
    fn a_file_that_is_not_ours_is_never_removed() {
        let dir = export_dir();
        let (old, tmp) = (dir.join("old.txt"), dir.join(".old.txt.tmp"));
        std::fs::write(&old, "old").unwrap();
        std::fs::write(&tmp, "somebody else's").unwrap();
        let e = replace_through(&old, &tmp, true, &STEPS, |_| Ok(())).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(&tmp).unwrap(), "somebody else's");
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "old");
        std::fs::remove_file(&tmp).unwrap();
        let folder = dir.join("folder");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(folder.join("in"), "x").unwrap();
        assert!(save_text(&folder, "mongodb://h:27017", true).is_err());
        assert_eq!(names(&folder), ["in"], "a folder is not replaced");
        assert_eq!(names(&dir), ["folder", "old.txt"], "and no temporary file is left");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Each save's temporary file has a name of its own, beside the target.
    #[test]
    fn the_temporary_file_is_beside_the_target_with_a_name_of_its_own() {
        let target = Path::new("/some/where/ก ไทย.txt");
        let (a, b) = (temp_beside(target).unwrap(), temp_beside(target).unwrap());
        assert_ne!(a, b);
        assert_eq!(a.parent(), target.parent());
        let name = a.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with(".ก ไทย.txt.") && name.ends_with(".tmp"), "{name}");
        assert!(name.contains(&format!(".{}.", std::process::id())));
        assert!(temp_beside(Path::new("/")).is_err());
    }

    /// A save that works leaves just the file.
    #[test]
    fn a_save_that_works_leaves_just_the_file() {
        let dir = export_dir();
        let path = dir.join("new.txt");
        for private in [true, false, true] {
            save_text(&path, "mongodb://h:27017", private).unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "mongodb://h:27017\n");
            assert_eq!(names(&dir), ["new.txt"], "private {private}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A file at a symlink's path is replaced by the new one; the file the
    /// link pointed to is not touched.
    #[cfg(unix)]
    #[test]
    fn a_symlink_at_the_path_is_replaced_and_its_target_left_alone() {
        let dir = std::env::temp_dir().join(format!("rongo-export-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let (real, link) = (dir.join("real.txt"), dir.join("link.txt"));
        std::fs::write(&real, "somebody else's file").unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        save_text(&link, "mongodb://app:pw@h:27017", true).unwrap();
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "somebody else's file");
        assert!(!std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "mongodb://app:pw@h:27017\n");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A saved string with a password in it is readable only by you.
    #[cfg(unix)]
    #[test]
    fn a_file_with_a_password_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("rongo-export-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let (open, private) = (dir.join("open.txt"), dir.join("private.txt"));
        save_text(&open, "mongodb://h:27017", false).unwrap();
        save_text(&private, "mongodb://app:pw@h:27017", true).unwrap();
        assert_eq!(std::fs::read_to_string(&private).unwrap(), "mongodb://app:pw@h:27017\n");
        assert_eq!(std::fs::metadata(&private).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(&open).unwrap(), "mongodb://h:27017\n");
        std::fs::write(&private, "old secret that is longer than the new text").unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o644)).unwrap();
        save_text(&private, "mongodb://app:pw@h:27017", true).unwrap();
        assert_eq!(std::fs::read_to_string(&private).unwrap(), "mongodb://app:pw@h:27017\n", "an existing file is replaced");
        assert_eq!(std::fs::metadata(&private).unwrap().permissions().mode() & 0o777, 0o600, "and made private");
        let _ = std::fs::remove_dir_all(dir);
    }
}
