//! Saved connection profiles. Profiles live in a JSON file in the user's config
//! directory; passwords live in the OS keychain, never in the file.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::api::ConnId;
use crate::error::{CoreError, CoreResult};

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

    /// Whether connecting needs a password from the keychain.
    pub fn needs_password(&self) -> bool {
        matches!(self.auth, Auth::Password { .. })
    }

    /// Whether a password may be stored for this profile: password auth, or a
    /// URI with a username (its password was moved to the keychain).
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
    /// the keychain when the profile is saved.
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

/// Replace the password in a URI with `****`.
pub fn redact_uri(uri: &str) -> String {
    match uri_userinfo(uri) {
        Some((scheme, user, Some(_), rest)) => format!("{scheme}://{user}:****{rest}"),
        _ => uri.to_string(),
    }
}

/// Split `scheme://user:pass@rest` into (`scheme://user@rest`, `pass`).
/// The password comes back decoded (`s3cr%40t` → `s3cr@t`): it leaves the
/// URI, and is given to the driver as is. Returns `None` when the URI
/// carries no password.
pub fn split_uri_password(uri: &str) -> Option<(String, String)> {
    match uri_userinfo(uri)? {
        (scheme, user, Some(pass), rest) => Some((format!("{scheme}://{user}{rest}"), percent_decode(pass))),
        _ => None,
    }
}

/// Decode `%XX` escapes. Text that isn't valid escaping is kept as typed.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        match (b[i], b.get(i + 1).copied().and_then(hex), b.get(i + 2).copied().and_then(hex)) {
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (c, ..) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

/// `(scheme, user, password, "@hosts/…")` of a URI with userinfo.
fn uri_userinfo(uri: &str) -> Option<(&str, &str, Option<&str>, &str)> {
    let (scheme, rest) = uri.split_once("://")?;
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let at = rest[..authority_end].rfind('@')?;
    let userinfo = &rest[..at];
    Some(match userinfo.split_once(':') {
        Some((user, pass)) => (scheme, user, Some(pass), &rest[at..]),
        None => (scheme, userinfo, None, &rest[at..]),
    })
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ProfileFile {
    profiles: Vec<ConnectionProfile>,
}

/// Loads and saves profiles and their secrets.
pub struct ProfileStore {
    path: PathBuf,
    profiles: Vec<ConnectionProfile>,
}

const KEYRING_SERVICE: &str = "dev.rongo.connections";

impl ProfileStore {
    pub fn default_path() -> Option<PathBuf> {
        directories::ProjectDirs::from("dev", "rongo", "rongo").map(|d| d.config_dir().join("profiles.json"))
    }

    pub fn load(path: PathBuf) -> CoreResult<Self> {
        let profiles = match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice::<ProfileFile>(&bytes)
                    .map_err(|e| CoreError::Other(format!("{}: {e}", path.display())))?
                    .profiles
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(CoreError::Other(format!("{}: {e}", path.display()))),
        };
        Ok(Self { path, profiles })
    }

    pub fn profiles(&self) -> &[ConnectionProfile] {
        &self.profiles
    }

    pub fn get(&self, id: Uuid) -> Option<&ConnectionProfile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    /// Insert or update a profile. A password embedded in a URI target is moved
    /// to the keychain, as is `password` when given.
    pub fn upsert(&mut self, mut profile: ConnectionProfile, password: Option<String>) -> CoreResult<()> {
        let mut password = password;
        if let Target::Uri { uri } = &mut profile.target
            && let Some((clean, pass)) = split_uri_password(uri)
        {
            *uri = clean;
            password = Some(pass);
        }
        if let Some(pw) = password {
            set_secret(profile.id, SecretKind::Password, &pw)?;
        }
        match self.profiles.iter_mut().find(|p| p.id == profile.id) {
            Some(existing) => *existing = profile,
            None => self.profiles.push(profile),
        }
        self.save()
    }

    /// Move a profile to just before `before`, or to the end. The order is
    /// the order connections are listed in.
    pub fn move_before(&mut self, id: Uuid, before: Option<Uuid>) -> CoreResult<()> {
        let Some(from) = self.profiles.iter().position(|p| p.id == id) else { return Ok(()) };
        if before == Some(id) {
            return Ok(());
        }
        let p = self.profiles.remove(from);
        let to = before.and_then(|b| self.profiles.iter().position(|p| p.id == b)).unwrap_or(self.profiles.len());
        self.profiles.insert(to, p);
        self.save()
    }

    pub fn remove(&mut self, id: Uuid) -> CoreResult<()> {
        self.profiles.retain(|p| p.id != id);
        let _ = delete_secret(id, SecretKind::Password);
        let _ = delete_secret(id, SecretKind::Ssh);
        self.save()
    }

    fn save(&self) -> CoreResult<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| CoreError::Other(e.to_string()))?;
        }
        let file = ProfileFile { profiles: self.profiles.clone() };
        let json = serde_json::to_vec_pretty(&file).map_err(|e| CoreError::Other(e.to_string()))?;
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, json).map_err(|e| CoreError::Other(e.to_string()))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| CoreError::Other(e.to_string()))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SecretKind {
    Password,
    Ssh,
}

impl SecretKind {
    fn suffix(self) -> &'static str {
        match self {
            SecretKind::Password => "password",
            SecretKind::Ssh => "ssh",
        }
    }
}

/// Where secrets are stored. The real one is the macOS Keychain; tests use
/// memory.
pub trait SecretBackend: Send + Sync {
    fn read(&self, account: &str) -> CoreResult<Option<String>>;
    fn write(&self, account: &str, value: &str) -> CoreResult<()>;
    fn delete(&self, account: &str) -> CoreResult<()>;
}

pub struct Keychain;

impl Keychain {
    fn entry(account: &str) -> CoreResult<keyring::Entry> {
        keyring::Entry::new(KEYRING_SERVICE, account).map_err(|e| CoreError::Other(format!("keychain: {e}")))
    }
}

impl SecretBackend for Keychain {
    fn read(&self, account: &str) -> CoreResult<Option<String>> {
        match Self::entry(account)?.get_password() {
            Ok(s) => Ok(Some(s)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(CoreError::Other(format!("keychain: {e}"))),
        }
    }

    fn write(&self, account: &str, value: &str) -> CoreResult<()> {
        Self::entry(account)?.set_password(value).map_err(|e| CoreError::Other(format!("keychain: {e}")))
    }

    fn delete(&self, account: &str) -> CoreResult<()> {
        match Self::entry(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(CoreError::Other(format!("keychain: {e}"))),
        }
    }
}

/// Every connection's secrets in one keychain item, so macOS asks once per
/// launch rather than once per connection. Secrets saved by older versions
/// (one item per connection) are moved in the first time they are read.
pub struct Vault<B: SecretBackend> {
    backend: B,
    state: std::sync::Mutex<VaultState>,
}

#[derive(Default)]
struct VaultState {
    /// The vault's contents, once read.
    secrets: Option<std::collections::BTreeMap<String, String>>,
    /// Old per-connection items already looked for.
    legacy_checked: std::collections::HashSet<String>,
}

const VAULT_ACCOUNT: &str = "vault";

fn vault_key(id: Uuid, kind: SecretKind) -> String {
    format!("{id}:{}", kind.suffix())
}

impl<B: SecretBackend> Vault<B> {
    pub fn new(backend: B) -> Self {
        Self { backend, state: Default::default() }
    }

    fn with_state<T>(&self, f: impl FnOnce(&B, &mut VaultState) -> CoreResult<T>) -> CoreResult<T> {
        let mut st = self.state.lock().map_err(|_| CoreError::Other("keychain state poisoned".into()))?;
        if st.secrets.is_none() {
            let loaded = match self.backend.read(VAULT_ACCOUNT)? {
                Some(json) => serde_json::from_str(&json).map_err(|e| CoreError::Other(format!("keychain vault: {e}")))?,
                None => Default::default(),
            };
            st.secrets = Some(loaded);
        }
        f(&self.backend, &mut st)
    }

    fn save(backend: &B, st: &VaultState) -> CoreResult<()> {
        let json = serde_json::to_string(st.secrets.as_ref().expect("loaded")).map_err(|e| CoreError::Other(e.to_string()))?;
        backend.write(VAULT_ACCOUNT, &json)
    }

    pub fn get(&self, id: Uuid, kind: SecretKind) -> CoreResult<Option<String>> {
        let key = vault_key(id, kind);
        self.with_state(|backend, st| {
            if let Some(v) = st.secrets.as_ref().and_then(|m| m.get(&key)) {
                return Ok(Some(v.clone()));
            }
            if !st.legacy_checked.insert(key.clone()) {
                return Ok(None);
            }
            // Saved by an older version in its own item: move it in.
            let Some(old) = backend.read(&key)? else { return Ok(None) };
            st.secrets.as_mut().expect("loaded").insert(key.clone(), old.clone());
            Self::save(backend, st)?;
            let _ = backend.delete(&key);
            Ok(Some(old))
        })
    }

    pub fn set(&self, id: Uuid, kind: SecretKind, secret: &str) -> CoreResult<()> {
        let key = vault_key(id, kind);
        self.with_state(|backend, st| {
            let map = st.secrets.as_mut().expect("loaded");
            if map.get(&key).map(String::as_str) == Some(secret) {
                return Ok(());
            }
            map.insert(key.clone(), secret.to_string());
            st.legacy_checked.insert(key);
            Self::save(backend, st)
        })
    }

    pub fn delete(&self, id: Uuid, kind: SecretKind) -> CoreResult<()> {
        let key = vault_key(id, kind);
        self.with_state(|backend, st| {
            let removed = st.secrets.as_mut().expect("loaded").remove(&key).is_some();
            st.legacy_checked.insert(key.clone());
            if removed {
                Self::save(backend, st)?;
            }
            backend.delete(&key)
        })
    }
}

static VAULT: std::sync::LazyLock<Vault<Keychain>> = std::sync::LazyLock::new(|| Vault::new(Keychain));

pub fn get_secret(id: Uuid, kind: SecretKind) -> CoreResult<Option<String>> {
    VAULT.get(id, kind)
}

pub fn set_secret(id: Uuid, kind: SecretKind, secret: &str) -> CoreResult<()> {
    VAULT.set(id, kind, secret)
}

pub fn delete_secret(id: Uuid, kind: SecretKind) -> CoreResult<()> {
    VAULT.delete(id, kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Order and colour are saved, and a file from before colours loads.
    #[test]
    fn order_and_colour_are_saved() {
        let dir = std::env::temp_dir().join(format!("rongo-profiles-{}", Uuid::new_v4()));
        let path = dir.join("profiles.json");
        let mut store = ProfileStore::load(path.clone()).unwrap();
        let [a, b, c] = ["a", "b", "c"].map(ConnectionProfile::new);
        for p in [&a, &b, &c] {
            store.upsert(p.clone(), None).unwrap();
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
        store.upsert(ConnectionProfile { color: Some("red".into()), ..b.clone() }, None).unwrap();

        let reloaded = ProfileStore::load(path.clone()).unwrap();
        assert_eq!(names(&reloaded), ["b", "a", "c"]);
        assert_eq!(reloaded.profiles()[0].color.as_deref(), Some("red"));
        let json = std::fs::read_to_string(&path).unwrap();
        assert_eq!(json.matches("\"color\"").count(), 1, "no colour, no key: {json}");

        std::fs::write(&path, r#"{"profiles":[{"id":"00000000-0000-0000-0000-000000000001","name":"old","target":{"kind":"uri","uri":"mongodb://h"},"auth":{"kind":"none"}}]}"#).unwrap();
        let old = ProfileStore::load(path).unwrap();
        assert_eq!(old.profiles()[0].color, None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn splits_password_out_of_uri() {
        let (clean, pass) = split_uri_password("mongodb://alice:s3cr%40t@db1:27017/?replicaSet=rs0").unwrap();
        assert_eq!(clean, "mongodb://alice@db1:27017/?replicaSet=rs0");
        assert_eq!(pass, "s3cr@t", "decoded, as the driver would read it");
        let (_, pass) = split_uri_password("mongodb://u:a%3Ab%2Fc%25d%E0%B8%81@h/").unwrap();
        assert_eq!(pass, "a:b/c%dก");
        let (_, pass) = split_uri_password("mongodb://u:100%sure@h/").unwrap();
        assert_eq!(pass, "100%sure", "not an escape: kept as typed");
        assert!(split_uri_password("mongodb://db1:27017").is_none());
        assert!(split_uri_password("mongodb://alice@db1:27017").is_none());
    }

    #[test]
    fn only_profiles_with_credentials_use_the_keychain() {
        let mut p = ConnectionProfile::new("x");
        assert!(!p.uses_password());
        p.target = Target::Uri { uri: "mongodb+srv://bob@c.example.net/".into() };
        assert!(p.uses_password());
        p.target = Target::Uri { uri: "mongodb://h:27017/".into() };
        assert!(!p.uses_password());
        p.auth = Auth::Password { user: "u".into(), auth_db: "admin".into(), mechanism: None };
        assert!(p.uses_password());
    }

    /// Counts keychain reads (each may be a macOS prompt) and holds items.
    #[derive(Default)]
    struct Memory {
        items: std::sync::Mutex<std::collections::HashMap<String, String>>,
        reads: std::sync::atomic::AtomicUsize,
    }

    impl SecretBackend for &Memory {
        fn read(&self, account: &str) -> CoreResult<Option<String>> {
            self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self.items.lock().unwrap().get(account).cloned())
        }
        fn write(&self, account: &str, value: &str) -> CoreResult<()> {
            self.items.lock().unwrap().insert(account.into(), value.into());
            Ok(())
        }
        fn delete(&self, account: &str) -> CoreResult<()> {
            self.items.lock().unwrap().remove(account);
            Ok(())
        }
    }

    #[test]
    fn many_connections_share_one_keychain_item() {
        let mem = Memory::default();
        let ids: Vec<Uuid> = (0..5).map(|_| Uuid::new_v4()).collect();
        {
            let v = Vault::new(&mem);
            for (i, id) in ids.iter().enumerate() {
                v.set(*id, SecretKind::Password, &format!("pw{i}")).unwrap();
            }
        }
        assert_eq!(mem.items.lock().unwrap().len(), 1, "one item for all connections");
        // A new launch reads the keychain once for all five connections.
        mem.reads.store(0, std::sync::atomic::Ordering::SeqCst);
        let v = Vault::new(&mem);
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(v.get(*id, SecretKind::Password).unwrap().as_deref(), Some(format!("pw{i}").as_str()));
            assert_eq!(v.get(*id, SecretKind::Password).unwrap().as_deref(), Some(format!("pw{i}").as_str()));
        }
        assert_eq!(mem.reads.load(std::sync::atomic::Ordering::SeqCst), 1);
        v.delete(ids[0], SecretKind::Password).unwrap();
        assert_eq!(v.get(ids[0], SecretKind::Password).unwrap(), None);
    }

    #[test]
    fn secrets_saved_per_connection_by_older_versions_move_into_the_vault() {
        let mem = Memory::default();
        let id = Uuid::new_v4();
        mem.items.lock().unwrap().insert(format!("{id}:password"), "old".into());
        let v = Vault::new(&mem);
        assert_eq!(v.get(id, SecretKind::Password).unwrap().as_deref(), Some("old"));
        assert!(!mem.items.lock().unwrap().contains_key(&format!("{id}:password")), "old item removed");
        // Next launch: found in the vault, one read.
        mem.reads.store(0, std::sync::atomic::Ordering::SeqCst);
        let v = Vault::new(&mem);
        assert_eq!(v.get(id, SecretKind::Password).unwrap().as_deref(), Some("old"));
        assert_eq!(mem.reads.load(std::sync::atomic::Ordering::SeqCst), 1);
        // A connection without a password looks for an old item only once.
        let none = Uuid::new_v4();
        assert_eq!(v.get(none, SecretKind::Password).unwrap(), None);
        assert_eq!(v.get(none, SecretKind::Password).unwrap(), None);
        assert_eq!(mem.reads.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn redacts_uri() {
        assert_eq!(redact_uri("mongodb+srv://bob:pw@c.example.net/x"), "mongodb+srv://bob:****@c.example.net/x");
        assert_eq!(redact_uri("mongodb://h:1"), "mongodb://h:1");
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
