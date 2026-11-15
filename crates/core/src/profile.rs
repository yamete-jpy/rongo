//! Saved connection profiles. Profiles live in a JSON file in the user's config
//! directory; passwords live in the vault (`crate::vault`), never in the file.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::ConnId;
use crate::error::{CoreError, CoreResult};
#[cfg(target_os = "macos")]
pub use crate::vault::keychain::Keychain;
pub use crate::vault::keychain::SecretBackend;
use crate::vault::Change;
pub use crate::vault::{SecretKind, Vault, vault_key};
pub use crate::uri::{export_uri, has_tls_files, redact_uri, split_uri_password};
pub(crate) use crate::uri::{redact_uri_secrets, uri_userinfo};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConnectionProfile {
    pub id: Uuid,
    pub name: String,
    pub target: Target,
    pub auth: Auth,
    #[serde(default)]
    pub tls: TlsConfig,
    #[serde(default)]
    pub ssh: Option<SshConfig>,
    #[serde(default)]
    pub default_db: Option<String>,
    /// A colour name ("red", "blue", …) to tell connections apart, e.g. to
    /// mark production. A name, not RGB, so the palette can follow the theme;
    /// a name this build doesn't know shows no colour.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Refuse every write on this connection (enforced in core, so dialogs
    /// and the shell are covered alike).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
    /// Collection History on this connection: `None` follows Settings,
    /// `Some(on)` overrides them (on even when Settings turn history off).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<bool>,
}

impl ConnectionProfile {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            target: Target::Manual {
                hosts: vec!["localhost:27017".into()],
                replica_set: None,
                direct: false,
            },
            auth: Auth::None,
            tls: TlsConfig::default(),
            ssh: None,
            default_db: None,
            color: None,
            read_only: false,
            history: None,
        }
    }

    /// A profile that opens the in-memory demo server instead of a real one.
    pub fn mock() -> Self {
        let mut p = Self::new("Demo (mock)");
        p.id = Uuid::from_u128(0x6d6f_636b);
        p.target = Target::Uri { uri: MOCK_URI.into() };
        p
    }

    pub fn conn_id(&self) -> ConnId {
        ConnId(self.id)
    }

    pub fn is_mock(&self) -> bool {
        matches!(&self.target, Target::Uri { uri } if uri.starts_with(MOCK_URI))
    }

    /// Whether connecting needs a password from the vault.
    pub fn needs_password(&self) -> bool {
        matches!(self.auth, Auth::Password { .. })
    }

    /// Whether a password may be stored for this profile: password auth, or a
    /// URI with a username (its password was moved to the vault).
    pub fn uses_password(&self) -> bool {
        self.needs_password() || matches!(&self.target, Target::Uri { uri } if uri_userinfo(uri).is_some())
    }

    pub fn needs_ssh_secret(&self) -> bool {
        self.ssh.is_some()
    }

    /// Short description for the connection list, e.g. `localhost:27017`.
    pub fn summary(&self) -> String {
        match &self.target {
            Target::Uri { uri } => redact_uri(uri),
            Target::Manual { hosts, replica_set, .. } => match replica_set {
                Some(rs) if !rs.is_empty() => format!("{} ({rs})", hosts.join(",")),
                _ => hosts.join(","),
            },
        }
    }
}

pub const MOCK_URI: &str = "mock://";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Target {
    /// A `mongodb://` or `mongodb+srv://` URI. Any password in it is moved to
    /// the vault when the profile is saved.
    Uri { uri: String },
    Manual {
        /// `host:port` entries.
        hosts: Vec<String>,
        replica_set: Option<String>,
        /// Talk to the listed host only, without replica set discovery.
        direct: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Auth {
    None,
    Password {
        user: String,
        auth_db: String,
        /// `SCRAM-SHA-256`, `SCRAM-SHA-1`, or empty to let the server choose.
        mechanism: Option<String>,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TlsConfig {
    pub enabled: bool,
    pub allow_invalid_certs: bool,
    pub ca_file: Option<PathBuf>,
    pub cert_key_file: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub auth: SshAuth,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SshAuth {
    /// The secret is the password.
    Password,
    /// The secret, if any, is the key's passphrase.
    Key { path: PathBuf },
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ProfileFile {
    profiles: Vec<ConnectionProfile>,
}

/// What `ProfileStore::update` says of a profile no longer in the file.
pub const DELETED_ELSEWHERE: &str = "this connection was deleted in another rongo window";

/// The profiles saved at `path`; none when there is no file yet.
fn read_profiles(path: &Path) -> CoreResult<Vec<ConnectionProfile>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice::<ProfileFile>(&bytes).map_err(|e| CoreError::Other(format!("{}: {e}", path.display())))?.profiles),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(CoreError::Other(format!("{}: {e}", path.display()))),
    }
}

/// Loads and saves profiles and their secrets.
///
/// The list in memory is a copy of `profiles.json`; changes are never made
/// from it. Each one, holding an OS lock on `profiles.lock` beside the file
/// (as the vault does: tried for about 4 s, then "busy"), re-reads the file,
/// applies its one change by profile id, writes the file whole (through a
/// temporary file of this process's own, synced, then renamed over it; 0600
/// on Unix), and only then takes the result as the copy. So two rongo windows
/// keep each other's connections, a failed write changes nothing in memory or
/// on disk, and a file that can't be read is never written over.
pub struct ProfileStore {
    path: PathBuf,
    profiles: Vec<ConnectionProfile>,
}

impl ProfileStore {
    pub fn default_path() -> Option<PathBuf> {
        directories::ProjectDirs::from("dev", "rongo", "rongo").map(|d| d.config_dir().join("profiles.json"))
    }

    pub fn load(path: PathBuf) -> CoreResult<Self> {
        let profiles = read_profiles(&path)?;
        Ok(Self { path, profiles })
    }

    /// Reads the file again, for what another rongo window saved since. A
    /// file that can't be read leaves the list as it was.
    pub fn reload(&mut self) -> CoreResult<()> {
        self.profiles = read_profiles(&self.path)?;
        Ok(())
    }

    pub fn profiles(&self) -> &[ConnectionProfile] {
        &self.profiles
    }

    pub fn get(&self, id: Uuid) -> Option<&ConnectionProfile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    /// Edits one saved profile as it is on disk now, under the lock, for a
    /// change that touches no secret (its colour, its lock): another
    /// window's edits to it stay. One deleted in another rongo window is not
    /// brought back: nothing is written, it leaves this window's list too,
    /// and the error (`DELETED_ELSEWHERE`) says so.
    pub fn update(&mut self, id: Uuid, edit: impl FnOnce(&mut ConnectionProfile)) -> CoreResult<()> {
        let mut found = false;
        self.change(|saved| {
            if let Some(p) = saved.iter_mut().find(|p| p.id == id) {
                edit(p);
                found = true;
            }
            Ok(())
        })?;
        if !found {
            return Err(CoreError::Other(DELETED_ELSEWHERE.into()));
        }
        Ok(())
    }

    /// Insert or update a profile (the connection form's Save), with its
    /// secrets in one vault write: a password embedded in a URI target
    /// (moved out of it) or else `password`, and `ssh_secret`, each kept only
    /// when the profile uses it. A secret it no longer uses (password
    /// authentication or SSH turned off) is deleted in that same write. A
    /// profile deleted in another window since comes back, with the secrets
    /// typed or else those this window held. A lock saved on disk is never
    /// lifted here (that takes Allow Writes, then `update`): true when the
    /// profile was given unlocked and so was saved locked. A vault that
    /// refuses (changed in another window, say) leaves the profile as it
    /// was; so does a failed write of the file, though the vault was written
    /// by then.
    pub fn upsert(&mut self, mut profile: ConnectionProfile, password: Option<String>, ssh_secret: Option<String>, vault: &Vault) -> CoreResult<bool> {
        let mut password = password;
        if let Target::Uri { uri } = &mut profile.target
            && let Some((clean, pass)) = split_uri_password(uri)
        {
            *uri = clean;
            password = Some(pass);
        }
        let id = profile.id;
        let mut kept_lock = false;
        self.change(|saved| {
            let uses = |p: &ConnectionProfile, kind| if kind == SecretKind::Password { p.uses_password() } else { p.needs_ssh_secret() };
            let before = saved.iter().find(|p| p.id == id);
            if before.is_some_and(|p| p.read_only) && !profile.read_only {
                profile.read_only = true;
                kept_lock = true;
            }
            // Not on disk (new, or deleted elsewhere since): what this window holds for it, for what wasn't typed.
            let held = |kind| before.is_none().then(|| vault.get(id, kind)).flatten();
            let (held_password, held_ssh) = (held(SecretKind::Password), held(SecretKind::Ssh));
            let mut changes = Vec::new();
            for (kind, typed, held) in [(SecretKind::Password, password.as_deref(), held_password.as_deref()), (SecretKind::Ssh, ssh_secret.as_deref(), held_ssh.as_deref())] {
                if uses(&profile, kind) {
                    changes.extend(typed.or(held).map(|s| Change::Set(id, kind, s)));
                } else if vault.get(id, kind).is_some() || before.is_some_and(|p| uses(p, kind)) {
                    // Turned off (or left from before): it goes, and one typed for it isn't kept.
                    changes.push(Change::Delete(id, kind));
                }
            }
            vault.apply(&changes)?;
            match saved.iter_mut().find(|p| p.id == id) {
                Some(existing) => *existing = profile,
                None => saved.push(profile),
            }
            Ok(())
        })?;
        Ok(kept_lock)
    }

    /// Move a profile to just before `before`, or to the end. The order is
    /// the order connections are listed in.
    pub fn move_before(&mut self, id: Uuid, before: Option<Uuid>) -> CoreResult<()> {
        if before == Some(id) {
            return Ok(());
        }
        self.change(|saved| {
            let Some(from) = saved.iter().position(|p| p.id == id) else { return Ok(()) };
            let p = saved.remove(from);
            let to = before.and_then(|b| saved.iter().position(|p| p.id == b)).unwrap_or(saved.len());
            saved.insert(to, p);
            Ok(())
        })
    }

    /// Takes a profile off the list, then its secrets out of the vault. A
    /// vault changed in another window is found first, and then nothing is
    /// removed. The secrets go only once the profile is gone from the file,
    /// so a failed write keeps both; should the vault refuse after that, the
    /// profile is gone (`get` finds nothing) and the error says why its
    /// secrets stay.
    pub fn remove(&mut self, id: Uuid, vault: &Vault) -> CoreResult<()> {
        vault.reload_if_changed()?;
        self.change(|saved| {
            saved.retain(|p| p.id != id);
            Ok(())
        })?;
        vault.apply(&[Change::Delete(id, SecretKind::Password), Change::Delete(id, SecretKind::Ssh)])
    }

    /// Applies `edit` to profiles.json as it is on disk now, under
    /// `profiles.lock`, and writes the result (nothing when it is the same);
    /// only then is it the list in memory. `edit` failing, a file that can't
    /// be read, or a failed write changes nothing.
    fn change(&mut self, edit: impl FnOnce(&mut Vec<ConnectionProfile>) -> CoreResult<()>) -> CoreResult<()> {
        let at = |e: &dyn std::fmt::Display| CoreError::Other(format!("{}: {e}", self.path.display()));
        if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            crate::fsx::create_private_dir(dir).map_err(|e| at(&e))?;
        }
        let _lock = match crate::vault::lock(&self.path) {
            Ok(Some(f)) => f,
            Ok(None) => return Err(CoreError::Other("the saved connections are busy in another rongo window; try again".into())),
            Err(e) => return Err(at(&e)),
        };
        // No other process writes profiles.json while the lock is held: any temporary file of theirs is a crash's.
        crate::vault::remove_stale_temps(&self.path, std::time::Duration::ZERO);
        let saved = read_profiles(&self.path)?;
        let mut next = saved.clone();
        edit(&mut next)?;
        if next != saved {
            let json = serde_json::to_vec_pretty(&ProfileFile { profiles: next.clone() }).map_err(|e| at(&e))?;
            crate::vault::write_atomically(&self.path, &json).map_err(|e| at(&e))?;
        }
        self.profiles = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::KdfParams;

    const PW: &str = "correct horse battery";

    /// A folder of its own holding `profiles.json` and a vault (removed on drop).
    struct Temp {
        dir: PathBuf,
        vault: Vault,
    }

    impl Temp {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("rongo-profiles-{}", Uuid::new_v4()));
            let vault = Vault::create(dir.join("vault"), PW, KdfParams::TEST, Default::default()).unwrap();
            Temp { dir, vault }
        }

        fn path(&self) -> PathBuf {
            self.dir.join("profiles.json")
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// `remove` takes a profile off the saved list (and the file) and its
    /// secrets out of the vault; a vault changed elsewhere, found before
    /// anything is removed, leaves it listed.
    #[test]
    fn remove_deletes_the_profile_and_its_secrets() {
        let t = Temp::new();
        let mut store = ProfileStore::load(t.path()).unwrap();
        let [a, b] = ["a", "b"].map(|name| ConnectionProfile { auth: Auth::Password { user: "u".into(), auth_db: "admin".into(), mechanism: None }, ..ConnectionProfile::new(name) });
        for p in [&a, &b] {
            store.upsert(p.clone(), Some(format!("pw {}", p.name)), None, &t.vault).unwrap();
        }
        t.vault.set(a.id, SecretKind::Ssh, "ssh a").unwrap();
        store.remove(a.id, &t.vault).unwrap();
        assert!(store.get(a.id).is_none());
        assert_eq!(t.vault.get(a.id, SecretKind::Password), None);
        assert_eq!(t.vault.get(a.id, SecretKind::Ssh), None);
        assert_eq!(t.vault.get(b.id, SecretKind::Password).as_deref(), Some("pw b"));
        let reloaded = ProfileStore::load(t.path()).unwrap();
        assert_eq!(reloaded.profiles().iter().map(|p| p.id).collect::<Vec<_>>(), [b.id]);
        let on_disk = Vault::unlock(t.vault.path().to_path_buf(), PW).unwrap();
        assert_eq!(on_disk.get(a.id, SecretKind::Password), None);
        assert_eq!(on_disk.get(b.id, SecretKind::Password).as_deref(), Some("pw b"));

        // The vault was changed in another window: nothing is removed.
        Vault::unlock(t.vault.path().to_path_buf(), PW).unwrap().change_password(PW, "another password").unwrap();
        assert_eq!(store.remove(b.id, &t.vault).err(), Some(CoreError::VaultChanged));
        assert!(store.get(b.id).is_some());
        assert_eq!(ProfileStore::load(t.path()).unwrap().profiles().len(), 1);
    }

    /// A password typed into a URI goes to the vault, decoded, and the URI
    /// is saved without it. A password typed later replaces it; none keeps it.
    #[test]
    fn upsert_moves_a_uri_password_into_the_vault() {
        let t = Temp::new();
        let mut store = ProfileStore::load(t.path()).unwrap();
        let mut p = ConnectionProfile::new("uri");
        p.target = Target::Uri { uri: "mongodb://u:p%40ss@h".into() };
        store.upsert(p.clone(), None, None, &t.vault).unwrap();
        let saved = store.get(p.id).unwrap().clone();
        assert_eq!(saved.target, Target::Uri { uri: "mongodb://u@h".into() });
        assert_eq!(t.vault.get(p.id, SecretKind::Password).as_deref(), Some("p@ss"));
        let json = std::fs::read_to_string(t.path()).unwrap();
        assert!(!json.contains("p%40ss") && !json.contains("p@ss"), "{json}");
        assert_eq!(Vault::unlock(t.vault.path().to_path_buf(), PW).unwrap().get(p.id, SecretKind::Password).as_deref(), Some("p@ss"));

        store.upsert(saved.clone(), Some("typed".into()), None, &t.vault).unwrap();
        assert_eq!(t.vault.get(p.id, SecretKind::Password).as_deref(), Some("typed"));
        store.upsert(saved, None, None, &t.vault).unwrap();
        assert_eq!(t.vault.get(p.id, SecretKind::Password).as_deref(), Some("typed"));
    }

    /// A password and an SSH secret saved together are one vault write.
    /// Turning password authentication or SSH off deletes that secret in the
    /// same write, and a secret given for a method that is off is not kept.
    #[test]
    fn a_profiles_secrets_are_one_write_and_go_with_their_method() {
        let t = Temp::new();
        let mut store = ProfileStore::load(t.path()).unwrap();
        let mut p = ConnectionProfile::new("p");
        p.auth = Auth::Password { user: "u".into(), auth_db: "admin".into(), mechanism: None };
        p.ssh = Some(SshConfig { host: "bastion".into(), port: 22, user: "ec2".into(), auth: SshAuth::Password });
        let writes = t.vault.writes();
        store.upsert(p.clone(), Some("pw".into()), Some("ssh pw".into()), &t.vault).unwrap();
        assert_eq!(t.vault.writes(), writes + 1, "one write for both");
        assert_eq!(t.vault.get(p.id, SecretKind::Password).as_deref(), Some("pw"));
        assert_eq!(t.vault.get(p.id, SecretKind::Ssh).as_deref(), Some("ssh pw"));

        // Saved again with nothing typed: both stay, and nothing is written.
        store.upsert(p.clone(), None, None, &t.vault).unwrap();
        assert_eq!(t.vault.writes(), writes + 1);
        assert_eq!(t.vault.get(p.id, SecretKind::Ssh).as_deref(), Some("ssh pw"));

        // SSH off: its secret goes (a new one typed is not kept); the password stays.
        let no_ssh = ConnectionProfile { ssh: None, ..p.clone() };
        store.upsert(no_ssh.clone(), None, Some("typed while off".into()), &t.vault).unwrap();
        assert_eq!(t.vault.writes(), writes + 2);
        assert_eq!(t.vault.get(p.id, SecretKind::Ssh), None);
        assert_eq!(t.vault.get(p.id, SecretKind::Password).as_deref(), Some("pw"));
        // Authentication off too: the password goes; one typed is not kept.
        store.upsert(ConnectionProfile { auth: Auth::None, ..no_ssh }, Some("typed while off".into()), None, &t.vault).unwrap();
        assert_eq!(t.vault.get(p.id, SecretKind::Password), None);
        let on_disk = Vault::unlock(t.vault.path().to_path_buf(), PW).unwrap();
        assert_eq!((on_disk.get(p.id, SecretKind::Password), on_disk.get(p.id, SecretKind::Ssh)), (None, None));

        // A profile that never had secrets doesn't touch the vault at all.
        let writes = t.vault.writes();
        store.upsert(ConnectionProfile::new("plain"), None, None, &t.vault).unwrap();
        assert_eq!(t.vault.writes(), writes);
    }

    /// Every change is made to profiles.json as it is on disk now, under
    /// `profiles.lock`: two windows (two stores on one file) keep each
    /// other's connections, deletes and order, and `reload` shows what the
    /// other saved.
    #[test]
    fn two_windows_keep_each_others_connections() {
        let t = Temp::new();
        let mut a = ProfileStore::load(t.path()).unwrap();
        let mut b = ProfileStore::load(t.path()).unwrap();
        let [p1, p2, x] = ["p1", "p2", "x"].map(ConnectionProfile::new);
        let p1 = ConnectionProfile { auth: Auth::Password { user: "u".into(), auth_db: "admin".into(), mechanism: None }, ..p1 };
        a.upsert(x.clone(), None, None, &t.vault).unwrap();
        a.upsert(p1.clone(), Some("pw1".into()), None, &t.vault).unwrap();
        // B started before A saved: its save keeps A's.
        b.upsert(p2.clone(), None, None, &t.vault).unwrap();
        let names = |s: &ProfileStore| s.profiles().iter().map(|p| p.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(&ProfileStore::load(t.path()).unwrap()), ["x", "p1", "p2"]);
        assert_eq!(names(&b), ["x", "p1", "p2"], "B's list is the file's");

        // A edits X; B, which never saw that, deletes X: it stays deleted, not reverted.
        a.upsert(ConnectionProfile { name: "x edited".into(), ..x.clone() }, None, None, &t.vault).unwrap();
        b.remove(x.id, &t.vault).unwrap();
        a.upsert(ConnectionProfile { color: Some("red".into()), ..p1.clone() }, None, None, &t.vault).unwrap();
        assert_eq!(names(&ProfileStore::load(t.path()).unwrap()), ["p1", "p2"], "not brought back");
        // Moves go by id on what is there now.
        b.move_before(p2.id, Some(p1.id)).unwrap();
        assert_eq!(names(&b), ["p2", "p1"]);
        assert_eq!(b.get(p1.id).unwrap().color.as_deref(), Some("red"), "A's colour kept");
        assert_eq!(names(&a), ["p1", "p2"], "A hasn't looked yet");
        a.reload().unwrap();
        assert_eq!(names(&a), ["p2", "p1"]);
        assert_eq!(t.vault.get(p1.id, SecretKind::Password).as_deref(), Some("pw1"));
    }

    /// `update` edits the profile as it is on disk now: another window's
    /// edits to it stay. One deleted in another window is not brought back:
    /// nothing is written, it leaves this window's list, and the error says so.
    #[test]
    fn update_edits_the_profile_as_it_is_on_disk() {
        let t = Temp::new();
        let mut a = ProfileStore::load(t.path()).unwrap();
        let mut b = ProfileStore::load(t.path()).unwrap();
        let p = ConnectionProfile::new("p");
        a.upsert(p.clone(), None, None, &t.vault).unwrap();
        b.reload().unwrap();
        b.upsert(ConnectionProfile { name: "renamed in B".into(), read_only: true, ..p.clone() }, None, None, &t.vault).unwrap();
        // A still holds the old copy.
        a.update(p.id, |q| q.color = Some("red".into())).unwrap();
        let on_disk = ProfileStore::load(t.path()).unwrap();
        let q = on_disk.get(p.id).unwrap();
        assert_eq!((q.name.as_str(), q.read_only, q.color.as_deref()), ("renamed in B", true, Some("red")), "B's edits kept");
        assert_eq!(a.get(p.id).unwrap().name, "renamed in B", "A's list is the file's");

        b.remove(p.id, &t.vault).unwrap();
        let before = std::fs::read(t.path()).unwrap();
        let err = a.update(p.id, |q| q.read_only = false).unwrap_err();
        assert!(err.to_string().contains("deleted in another rongo window"), "{err}");
        assert_eq!(std::fs::read(t.path()).unwrap(), before, "not brought back");
        assert!(a.get(p.id).is_none(), "and gone from A's list");
    }

    /// A Save never lifts a lock saved on disk (by this window or another):
    /// the profile is written with the lock kept, and `upsert` says so, for
    /// the app to ask "Allow Writes" first. Unlocked on disk, it saves as given.
    #[test]
    fn upsert_keeps_a_lock_saved_on_disk() {
        let t = Temp::new();
        let mut a = ProfileStore::load(t.path()).unwrap();
        let mut b = ProfileStore::load(t.path()).unwrap();
        let p = ConnectionProfile::new("p");
        assert!(!a.upsert(p.clone(), None, None, &t.vault).unwrap());
        b.reload().unwrap();
        b.update(p.id, |q| q.read_only = true).unwrap();
        assert!(ProfileStore::load(t.path()).unwrap().get(p.id).unwrap().read_only, "locked in B");
        // A's copy says unlocked; its form saves a new name, unlocked.
        let kept = a.upsert(ConnectionProfile { name: "new name".into(), ..p.clone() }, None, None, &t.vault).unwrap();
        assert!(kept, "the lock is kept, and said");
        let q = ProfileStore::load(t.path()).unwrap().get(p.id).unwrap().clone();
        assert_eq!((q.name.as_str(), q.read_only), ("new name", true));
        assert!(a.get(p.id).unwrap().read_only);
        // Lifted the one way there is (Allow Writes, then `update`): a Save then goes as given.
        a.update(p.id, |q| q.read_only = false).unwrap();
        assert!(!a.upsert(ConnectionProfile { read_only: false, ..q }, None, None, &t.vault).unwrap(), "unlocked on disk: nothing kept");
        assert!(!ProfileStore::load(t.path()).unwrap().get(p.id).unwrap().read_only);
    }

    /// Saving from the form a profile deleted in another window brings it
    /// back (the user is saving it), with the secrets typed, or else kept as
    /// this window held them (the form said they were saved).
    #[test]
    fn a_profile_deleted_elsewhere_comes_back_when_saved() {
        let t = Temp::new();
        let other = Vault::unlock(t.vault.path().to_path_buf(), PW).unwrap();
        let mut a = ProfileStore::load(t.path()).unwrap();
        let mut b = ProfileStore::load(t.path()).unwrap();
        let p = ConnectionProfile { auth: Auth::Password { user: "u".into(), auth_db: "admin".into(), mechanism: None }, ..ConnectionProfile::new("p") };
        a.upsert(p.clone(), Some("pw".into()), None, &t.vault).unwrap();
        b.reload().unwrap();
        b.remove(p.id, &other).unwrap();
        assert!(ProfileStore::load(t.path()).unwrap().get(p.id).is_none());
        a.upsert(ConnectionProfile { name: "saved again".into(), ..p.clone() }, None, None, &t.vault).unwrap();
        assert_eq!(ProfileStore::load(t.path()).unwrap().get(p.id).map(|q| q.name.as_str()), Some("saved again"));
        let on_disk = Vault::unlock(t.vault.path().to_path_buf(), PW).unwrap();
        assert_eq!(on_disk.get(p.id, SecretKind::Password).as_deref(), Some("pw"), "kept as the form said");
    }

    /// Temporary files of other processes left by a crash
    /// (`profiles.json.<pid>.tmp`) are swept while `profiles.lock` is held;
    /// other files are left alone.
    #[test]
    fn stale_temporary_files_are_swept() {
        let t = Temp::new();
        let mut store = ProfileStore::load(t.path()).unwrap();
        store.upsert(ConnectionProfile::new("p"), None, None, &t.vault).unwrap();
        let stale = t.dir.join("profiles.json.4000000000.tmp");
        std::fs::write(&stale, b"left by a crash").unwrap();
        for keep in ["profiles.json.tmp", "profiles.json.abc.tmp", "settings.json.4000000000.tmp"] {
            std::fs::write(t.dir.join(keep), b"not ours to sweep").unwrap();
        }
        store.upsert(ConnectionProfile::new("q"), None, None, &t.vault).unwrap();
        assert!(!stale.exists(), "swept");
        for keep in ["profiles.json.tmp", "profiles.json.abc.tmp", "settings.json.4000000000.tmp"] {
            assert!(t.dir.join(keep).exists(), "{keep} left alone");
        }
    }

    /// A write that fails (here a read-only folder) changes nothing: not
    /// the file, not the list in memory, and `remove` deletes no secret
    /// before the profile is gone from the file. A damaged file on disk is
    /// never written over.
    #[cfg(unix)]
    #[test]
    fn a_failed_write_changes_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let t = Temp::new();
        let mut store = ProfileStore::load(t.path()).unwrap();
        let mut p = ConnectionProfile::new("p");
        p.auth = Auth::Password { user: "u".into(), auth_db: "admin".into(), mechanism: None };
        store.upsert(p.clone(), Some("pw".into()), None, &t.vault).unwrap();
        let before = std::fs::read(t.path()).unwrap();
        std::fs::set_permissions(&t.dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let failed = store.upsert(ConnectionProfile { name: "renamed".into(), ..p.clone() }, None, None, &t.vault);
        let removed = store.remove(p.id, &t.vault);
        let moved = store.upsert(ConnectionProfile::new("new"), None, None, &t.vault);
        std::fs::set_permissions(&t.dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(failed.is_err() && removed.is_err() && moved.is_err(), "{failed:?} {removed:?} {moved:?}");
        assert_eq!(store.profiles().iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["p"], "memory unchanged");
        assert_eq!(std::fs::read(t.path()).unwrap(), before, "file unchanged");
        assert_eq!(t.vault.get(p.id, SecretKind::Password).as_deref(), Some("pw"), "its secret stays while the profile does");

        std::fs::write(t.path(), b"{ damaged").unwrap();
        assert!(store.upsert(ConnectionProfile::new("new"), None, None, &t.vault).is_err());
        assert_eq!(std::fs::read(t.path()).unwrap(), b"{ damaged", "never written over");
        assert_eq!(store.profiles().len(), 1);
        assert!(store.reload().is_err(), "reload says so");
        assert_eq!(store.profiles().len(), 1, "and keeps what it had");
    }

    /// A vault that refuses once the profile is gone from the file (here its
    /// folder turned read-only): the profile stays removed, and the error
    /// says why its secrets stay.
    #[cfg(unix)]
    #[test]
    fn remove_says_when_the_secrets_stay() {
        use std::os::unix::fs::PermissionsExt;
        let t = Temp::new();
        let other = Temp::new();
        let mut store = ProfileStore::load(other.path()).unwrap();
        let p = ConnectionProfile { auth: Auth::Password { user: "u".into(), auth_db: "admin".into(), mechanism: None }, ..ConnectionProfile::new("p") };
        store.upsert(p.clone(), Some("pw".into()), None, &t.vault).unwrap();
        std::fs::set_permissions(&t.dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let removed = store.remove(p.id, &t.vault);
        std::fs::set_permissions(&t.dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(removed.is_err());
        assert!(store.get(p.id).is_none() && ProfileStore::load(other.path()).unwrap().profiles().is_empty(), "gone");
        assert_eq!(t.vault.get(p.id, SecretKind::Password).as_deref(), Some("pw"), "the secret stayed");
    }

    /// profiles.json is written through a temporary file of this process's
    /// own, readable only by the user, and none is left behind.
    #[cfg(unix)]
    #[test]
    fn the_file_is_private_and_written_whole() {
        use std::os::unix::fs::PermissionsExt;
        let t = Temp::new();
        let mut store = ProfileStore::load(t.path()).unwrap();
        // What an older rongo, or another window, writes through: left alone.
        std::fs::write(t.dir.join("profiles.json.tmp"), b"theirs").unwrap();
        store.upsert(ConnectionProfile::new("p"), None, None, &t.vault).unwrap();
        assert_eq!(std::fs::metadata(t.path()).unwrap().permissions().mode() & 0o777, 0o600);
        let mut names: Vec<String> = std::fs::read_dir(&t.dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names, ["profiles.json", "profiles.json.tmp", "profiles.lock", "vault", "vault.lock"]);
        assert_eq!(std::fs::read(t.dir.join("profiles.json.tmp")).unwrap(), b"theirs");
    }

    /// A profile save that makes rongo's folder (before the vault does) makes
    /// it private, like the vault would.
    #[cfg(unix)]
    #[test]
    fn a_save_makes_a_new_folder_private() {
        use std::os::unix::fs::PermissionsExt;
        let other = std::env::temp_dir().join(format!("rongo-profiles-{}", Uuid::new_v4()));
        let path = other.join("config").join("profiles.json");
        let t = Temp::new();
        ProfileStore::load(path.clone()).unwrap().upsert(ConnectionProfile::new("p"), None, None, &t.vault).unwrap();
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let (config, top) = (mode(path.parent().unwrap()), mode(&other));
        let _ = std::fs::remove_dir_all(&other);
        assert_eq!((config, top), (0o700, 0o700));
    }

    /// While another window holds `profiles.lock`, a change waits (the
    /// vault's lock wait, shortened for the test), then says the other window
    /// is busy and changes nothing.
    #[test]
    fn a_held_lock_fails_after_a_while() {
        let _short = crate::vault::short_lock_wait();
        let t = Temp::new();
        let mut store = ProfileStore::load(t.path()).unwrap();
        store.upsert(ConnectionProfile::new("p"), None, None, &t.vault).unwrap();
        let held = std::fs::File::options().read(true).write(true).open(t.dir.join("profiles.lock")).unwrap();
        held.lock().unwrap();
        let start = std::time::Instant::now();
        let err = store.upsert(ConnectionProfile::new("q"), None, None, &t.vault).unwrap_err();
        let waited = start.elapsed();
        drop(held);
        assert!(err.to_string().contains("busy in another rongo window"), "{err}");
        assert!(waited >= crate::vault::SHORT_LOCK_WAIT, "waited {waited:?}");
        assert_eq!(store.profiles().len(), 1);
        assert_eq!(ProfileStore::load(t.path()).unwrap().profiles().len(), 1);
        store.upsert(ConnectionProfile::new("q"), None, None, &t.vault).unwrap();
        assert_eq!(ProfileStore::load(t.path()).unwrap().profiles().len(), 2);
    }

    /// Order and colour are saved, and a file from before colours loads.
    #[test]
    fn order_and_colour_are_saved() {
        let t = Temp::new();
        let path = t.path();
        let mut store = ProfileStore::load(path.clone()).unwrap();
        let [a, b, c] = ["a", "b", "c"].map(ConnectionProfile::new);
        for p in [&a, &b, &c] {
            store.upsert(p.clone(), None, None, &t.vault).unwrap();
        }
        let names = |s: &ProfileStore| s.profiles().iter().map(|p| p.name.clone()).collect::<Vec<_>>();
        store.move_before(c.id, Some(a.id)).unwrap();
        assert_eq!(names(&store), ["c", "a", "b"]);
        store.move_before(c.id, None).unwrap();
        assert_eq!(names(&store), ["a", "b", "c"]);
        store.move_before(a.id, Some(c.id)).unwrap();
        assert_eq!(names(&store), ["b", "a", "c"]);
        store.move_before(a.id, Some(a.id)).unwrap();
        assert_eq!(names(&store), ["b", "a", "c"], "onto itself: no change");
        store.upsert(ConnectionProfile { color: Some("red".into()), ..b.clone() }, None, None, &t.vault).unwrap();

        let reloaded = ProfileStore::load(path.clone()).unwrap();
        assert_eq!(names(&reloaded), ["b", "a", "c"]);
        assert_eq!(reloaded.profiles()[0].color.as_deref(), Some("red"));
        let json = std::fs::read_to_string(&path).unwrap();
        assert_eq!(json.matches("\"color\"").count(), 1, "no colour, no key: {json}");

        std::fs::write(&path, r#"{"profiles":[{"id":"00000000-0000-0000-0000-000000000001","name":"old","target":{"kind":"uri","uri":"mongodb://h"},"auth":{"kind":"none"}}]}"#).unwrap();
        let old = ProfileStore::load(path).unwrap();
        assert_eq!(old.profiles()[0].color, None);
    }

    #[test]
    fn only_profiles_with_credentials_use_the_vault() {
        let mut p = ConnectionProfile::new("x");
        assert!(!p.uses_password());
        p.target = Target::Uri { uri: "mongodb+srv://bob@c.example.net/".into() };
        assert!(p.uses_password());
        p.target = Target::Uri { uri: "mongodb://h:27017/".into() };
        assert!(!p.uses_password());
        p.auth = Auth::Password { user: "u".into(), auth_db: "admin".into(), mechanism: None };
        assert!(p.uses_password());
    }

    #[test]
    fn profile_keeps_read_only_and_omits_it_when_off() {
        let mut p = ConnectionProfile::new("ro");
        p.read_only = true;
        let on = serde_json::to_string(&p).unwrap();
        assert!(on.contains("\"read_only\":true"), "{on}");
        let back: ConnectionProfile = serde_json::from_str(&on).unwrap();
        assert!(back.read_only);
        p.read_only = false;
        let off = serde_json::to_string(&p).unwrap();
        assert!(!off.contains("read_only"), "{off}");
        let old = r#"{"id":"00000000-0000-0000-0000-000000000001","name":"old","target":{"kind":"uri","uri":"mongodb://h"},"auth":{"kind":"none"}}"#;
        assert!(!serde_json::from_str::<ConnectionProfile>(old).unwrap().read_only);
    }

    /// A connection's Collection History override (use Settings / on / off)
    /// is saved when set and left out when it follows Settings, so files
    /// from before it load as "use Settings".
    #[test]
    fn profiles_keep_the_history_override() {
        let mut p = ConnectionProfile::new("h");
        assert_eq!(p.history, None, "new profiles follow Settings");
        for on in [true, false] {
            p.history = Some(on);
            let json = serde_json::to_string(&p).unwrap();
            assert!(json.contains(&format!("\"history\":{on}")), "{json}");
            assert_eq!(serde_json::from_str::<ConnectionProfile>(&json).unwrap(), p);
        }
        p.history = None;
        let json = serde_json::to_string(&p).unwrap();
        assert!(!json.contains("history"), "{json}");
        let old = r#"{"id":"00000000-0000-0000-0000-000000000001","name":"old","target":{"kind":"uri","uri":"mongodb://h"},"auth":{"kind":"none"}}"#;
        assert_eq!(serde_json::from_str::<ConnectionProfile>(old).unwrap().history, None);
    }

    #[test]
    fn profile_round_trips_through_json() {
        let mut p = ConnectionProfile::new("x");
        p.auth = Auth::Password { user: "u".into(), auth_db: "admin".into(), mechanism: None };
        p.ssh = Some(SshConfig { host: "bastion".into(), port: 22, user: "ec2".into(), auth: SshAuth::Password });
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<ConnectionProfile>(&json).unwrap(), p);
    }
}
