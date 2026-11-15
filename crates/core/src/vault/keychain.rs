//! The macOS Keychain, which older versions kept every secret in. It is only
//! read now, to move what is there into the vault (`import`, then `forget`
//! once the vault is written; `move_out` does both in that order).
//!
//! Layout older versions wrote: one item, account `"vault"`, holding a JSON map
//! of secrets (names from `vault_key`), and, from versions before that, one
//! item per secret under the account `"<profile id>:<suffix>"` (that same name).
//!
//! Only the real Keychain (`Keychain`, `system()`) is macOS-only; the rest is
//! the same everywhere and tested with a backend in memory.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::error::{CoreError, CoreResult};
use crate::profile::ConnectionProfile;
use crate::vault::{KdfParams, SecretKind, Vault, vault_key};

#[cfg(target_os = "macos")]
pub(crate) const KEYRING_SERVICE: &str = "dev.rongo.connections";
/// The account of the one item that holds a JSON map of every secret.
pub(crate) const VAULT_ACCOUNT: &str = "vault";

/// Where secrets are stored. The real one is the macOS Keychain; tests use
/// memory. Only read and deleted now: nothing is written there any more.
pub trait SecretBackend: Send + Sync {
    fn read(&self, account: &str) -> CoreResult<Option<String>>;
    fn delete(&self, account: &str) -> CoreResult<()>;
}

#[cfg(target_os = "macos")]
pub struct Keychain;

#[cfg(target_os = "macos")]
impl Keychain {
    fn entry(account: &str) -> CoreResult<keyring::Entry> {
        keyring::Entry::new(KEYRING_SERVICE, account).map_err(|e| CoreError::Other(format!("keychain: {e}")))
    }
}

#[cfg(target_os = "macos")]
impl SecretBackend for Keychain {
    fn read(&self, account: &str) -> CoreResult<Option<String>> {
        match Self::entry(account)?.get_password() {
            Ok(s) => Ok(Some(s)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(CoreError::Other(format!("keychain: {e}"))),
        }
    }

    fn delete(&self, account: &str) -> CoreResult<()> {
        match Self::entry(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(CoreError::Other(format!("keychain: {e}"))),
        }
    }
}

/// The real Keychain.
#[cfg(target_os = "macos")]
pub fn system() -> impl SecretBackend {
    Keychain
}

/// What the Keychain holds of rongo's, for the vault to start from: the
/// `"vault"` item's map, plus each older per-connection item for a profile that
/// uses a password or SSH and whose entry that map lacks. `None` when there is
/// no such item at all. Only reads.
pub fn import(backend: &(impl SecretBackend + ?Sized), profiles: &[ConnectionProfile]) -> CoreResult<Option<BTreeMap<String, String>>> {
    let item = backend.read(VAULT_ACCOUNT)?;
    let mut found = item.is_some();
    // serde's message may quote what it could not read, and this text is secrets: only its category is reported.
    let mut map: BTreeMap<String, String> = match item {
        Some(json) => serde_json::from_str(&json).map_err(|e| CoreError::Other(format!("keychain vault: not a map of secrets ({:?})", e.classify())))?,
        None => BTreeMap::new(),
    };
    for p in profiles {
        let wanted = [(p.uses_password(), SecretKind::Password), (p.needs_ssh_secret(), SecretKind::Ssh)];
        for key in wanted.into_iter().filter(|(uses, _)| *uses).map(|(_, kind)| vault_key(p.id, kind)) {
            if map.contains_key(&key) {
                continue;
            }
            if let Some(old) = backend.read(&key)? {
                map.insert(key, old);
                found = true;
            }
        }
    }
    Ok(found.then_some(map))
}

/// Deletes the `"vault"` item and the older per-connection items named by
/// `keys` (names from `vault_key`). An item already gone is not an error.
/// Stops at the first refusal (macOS asked and the user said no: asking again
/// for each item would only repeat it).
pub fn forget(backend: &(impl SecretBackend + ?Sized), keys: &[String]) -> CoreResult<()> {
    backend.delete(VAULT_ACCOUNT)?;
    keys.iter().try_for_each(|key| backend.delete(key))
}

/// The vault `move_out` made, and what became of the Keychain's copy.
pub struct Moved {
    pub vault: Vault,
    /// Why rongo's items in the Keychain could not all be deleted (macOS
    /// asked and the user said no): they stay there, for the user to be told.
    pub left: Option<CoreError>,
}

/// Makes the vault at `path` from what the Keychain holds (or empty), and only
/// once it is written, `forget`s the Keychain's copy: a vault that could not be
/// made leaves the Keychain as it was.
pub fn move_out(backend: &(impl SecretBackend + ?Sized), profiles: &[ConnectionProfile], path: PathBuf, password: &str, params: KdfParams) -> CoreResult<Moved> {
    let Some(map) = import(backend, profiles)? else { return Ok(Moved { vault: Vault::create(path, password, params, BTreeMap::new())?, left: None }) };
    let keys: Vec<String> = map.keys().cloned().collect();
    let vault = Vault::create(path, password, params, map)?;
    // The vault is on disk: only now is the Keychain's copy let go. A Keychain that refuses
    // the delete (the user said No) leaves stale copies behind; that must not fail a move that worked, but is said.
    Ok(Moved { vault, left: forget(backend, &keys).err() })
}

/// An in-memory backend for tests, on every OS. Counts reads (each may be a
/// macOS prompt). With `refuse_deletes`, every delete fails, as when macOS
/// asked and the user said no.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct Memory {
    pub(crate) items: std::sync::Mutex<std::collections::HashMap<String, String>>,
    pub(crate) reads: std::sync::atomic::AtomicUsize,
    pub(crate) refuse_deletes: bool,
}

#[cfg(test)]
impl SecretBackend for Memory {
    fn read(&self, account: &str) -> CoreResult<Option<String>> {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self.items.lock().unwrap().get(account).cloned())
    }
    fn delete(&self, account: &str) -> CoreResult<()> {
        if self.refuse_deletes {
            return Err(CoreError::Other("keychain: the user refused".into()));
        }
        self.items.lock().unwrap().remove(account);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{Auth, SshAuth, SshConfig};
    use crate::testkit::TempDir;
    use uuid::Uuid;

    const PW: &str = "correct horse battery";

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn with_password(n: u128) -> ConnectionProfile {
        let mut p = ConnectionProfile::new(format!("p{n}"));
        p.id = id(n);
        p.auth = Auth::Password { user: "u".into(), auth_db: "admin".into(), mechanism: None };
        p
    }

    fn put(mem: &Memory, account: &str, value: &str) {
        mem.items.lock().unwrap().insert(account.into(), value.into());
    }

    fn account_names(mem: &Memory) -> Vec<String> {
        let mut names: Vec<String> = mem.items.lock().unwrap().keys().cloned().collect();
        names.sort();
        names
    }

    #[test]
    fn import_reads_the_vault_item_and_legacy_items() {
        let mem = Memory::default();
        let (id1, id2, id3) = (id(1), id(2), id(3));
        put(&mem, VAULT_ACCOUNT, &serde_json::to_string(&BTreeMap::from([(vault_key(id1, SecretKind::Password), "pw1".to_string())])).unwrap());
        put(&mem, &vault_key(id2, SecretKind::Password), "pw2");
        // Not read: a profile with no auth, whatever is stored for it.
        put(&mem, &vault_key(id3, SecretKind::Password), "stray");
        let mut ssh = ConnectionProfile::new("p4");
        ssh.id = id(4);
        ssh.ssh = Some(SshConfig { host: "h".into(), port: 22, user: "u".into(), auth: SshAuth::Password });
        put(&mem, &vault_key(ssh.id, SecretKind::Ssh), "ssh4");
        let mut p3 = ConnectionProfile::new("p3");
        p3.id = id3;
        // A legacy item the vault item already has an entry for: the vault item's copy wins.
        put(&mem, &vault_key(id1, SecretKind::Password), "older pw1");

        let map = import(&mem, &[with_password(1), with_password(2), p3, ssh.clone()]).unwrap().unwrap();
        assert_eq!(map.get(&vault_key(id1, SecretKind::Password)).map(String::as_str), Some("pw1"));
        assert_eq!(map.get(&vault_key(id2, SecretKind::Password)).map(String::as_str), Some("pw2"));
        assert_eq!(map.get(&vault_key(ssh.id, SecretKind::Ssh)).map(String::as_str), Some("ssh4"));
        assert!(!map.contains_key(&vault_key(id3, SecretKind::Password)));
        assert_eq!(map.len(), 3);
        // Only reads.
        assert_eq!(mem.items.lock().unwrap().len(), 5);
    }

    #[test]
    fn import_finds_nothing_on_a_clean_keychain() {
        let mem = Memory::default();
        assert_eq!(import(&mem, &[with_password(1)]).unwrap(), None);
        assert_eq!(import(&mem, &[]).unwrap(), None);
        // Items of other accounts are none of its business.
        put(&mem, "something else", "x");
        assert_eq!(import(&mem, &[with_password(1)]).unwrap(), None);
        // A vault item that holds nothing is still rongo's: it is moved (and forgotten).
        put(&mem, VAULT_ACCOUNT, "{}");
        assert_eq!(import(&mem, &[]).unwrap(), Some(BTreeMap::new()));
    }

    #[test]
    fn a_damaged_vault_item_is_an_error_not_nothing() {
        let mem = Memory::default();
        put(&mem, VAULT_ACCOUNT, "not json hunter2");
        let err = import(&mem, &[]).unwrap_err();
        assert!(!err.to_string().contains("hunter2"), "{err}");
        // Valid JSON of the wrong shape: serde would quote the value.
        put(&mem, VAULT_ACCOUNT, r#"{"a":"b","c":"hunter2" 5}"#);
        assert!(!import(&mem, &[]).unwrap_err().to_string().contains("hunter2"));
        put(&mem, VAULT_ACCOUNT, r#"["hunter2"]"#);
        assert!(!import(&mem, &[]).unwrap_err().to_string().contains("hunter2"));
    }

    #[test]
    fn forget_deletes_only_after_the_vault_is_written() {
        let mem = Memory::default();
        let hk = crate::history::HistoryKey::new([5; 32]);
        let item = BTreeMap::from([(vault_key(id(1), SecretKind::Password), "pw1".to_string()), (vault_key(Uuid::nil(), SecretKind::HistoryKey), hk.to_hex())]);
        put(&mem, VAULT_ACCOUNT, &serde_json::to_string(&item).unwrap());
        put(&mem, &vault_key(id(2), SecretKind::Password), "pw2");
        put(&mem, "something else", "x");
        let profiles = [with_password(1), with_password(2)];
        let before = account_names(&mem);

        // The vault cannot be made (its folder is a file): nothing is deleted.
        let t = TempDir::made();
        std::fs::write(t.0.join("blocker"), b"x").unwrap();
        assert!(move_out(&mem, &profiles, t.0.join("blocker").join("vault"), PW, KdfParams::TEST).is_err());
        assert_eq!(account_names(&mem), before);

        // Nor when the password is refused, or a history key in the item is damaged.
        assert!(matches!(move_out(&mem, &profiles, t.0.join("short"), "short", KdfParams::TEST), Err(CoreError::InvalidInput(_))));
        assert_eq!(account_names(&mem), before);
        let damaged = Memory::default();
        put(&damaged, VAULT_ACCOUNT, &serde_json::to_string(&BTreeMap::from([(vault_key(Uuid::nil(), SecretKind::HistoryKey), "bad".to_string())])).unwrap());
        assert!(matches!(move_out(&damaged, &[], t.0.join("v2"), PW, KdfParams::TEST), Err(CoreError::History(_))));
        assert_eq!(account_names(&damaged), [VAULT_ACCOUNT]);
        assert!(!t.0.join("v2").exists());

        // Made: everything is in the vault, and the vault item and the legacy item are gone.
        let path = t.0.join("vault");
        let v = move_out(&mem, &profiles, path.clone(), PW, KdfParams::TEST).unwrap().vault;
        assert_eq!(v.get(id(1), SecretKind::Password).as_deref(), Some("pw1"));
        assert_eq!(v.get(id(2), SecretKind::Password).as_deref(), Some("pw2"));
        assert_eq!(v.history_key(), hk);
        assert_eq!(account_names(&mem), ["something else"]);
        let again = Vault::unlock(path, PW).unwrap();
        assert_eq!(again.get(id(2), SecretKind::Password).as_deref(), Some("pw2"));
    }

    /// A Keychain that refuses the delete (macOS asked, the user said no)
    /// doesn't fail a move that worked: the vault is there with every
    /// secret, the items stay, and the refusal comes back to be said.
    #[test]
    fn a_refused_clean_up_is_reported_not_fatal() {
        let mem = Memory { refuse_deletes: true, ..Memory::default() };
        put(&mem, VAULT_ACCOUNT, &serde_json::to_string(&BTreeMap::from([(vault_key(id(1), SecretKind::Password), "pw1".to_string())])).unwrap());
        let t = TempDir::made();
        let moved = move_out(&mem, &[with_password(1)], t.0.join("vault"), PW, KdfParams::TEST).unwrap();
        assert_eq!(moved.vault.get(id(1), SecretKind::Password).as_deref(), Some("pw1"));
        let why = moved.left.expect("the refusal is reported").to_string();
        assert!(why.contains("refused") && !why.contains("pw1"), "{why}");
        assert_eq!(account_names(&mem), [VAULT_ACCOUNT]);
        assert_eq!(Vault::unlock(t.0.join("vault"), PW).unwrap().get(id(1), SecretKind::Password).as_deref(), Some("pw1"));

        // Deleted: nothing to report.
        let mem = Memory::default();
        put(&mem, VAULT_ACCOUNT, "{}");
        let t = TempDir::made();
        assert!(move_out(&mem, &[], t.0.join("vault"), PW, KdfParams::TEST).unwrap().left.is_none());
        assert!(account_names(&mem).is_empty());
    }

    #[test]
    fn a_clean_keychain_moves_out_to_an_empty_vault() {
        let mem = Memory::default();
        put(&mem, "something else", "x");
        let t = TempDir::made();
        let v = move_out(&mem, &[], t.0.join("vault"), PW, KdfParams::TEST).unwrap().vault;
        assert_eq!(v.get(id(1), SecretKind::Password), None);
        assert_eq!(account_names(&mem), ["something else"]);
    }

    #[test]
    fn forget_leaves_other_items_and_ignores_missing_ones() {
        let mem = Memory::default();
        put(&mem, VAULT_ACCOUNT, "{}");
        put(&mem, &vault_key(id(1), SecretKind::Password), "a");
        put(&mem, &vault_key(id(2), SecretKind::Password), "b");
        forget(&mem, &[vault_key(id(1), SecretKind::Password), vault_key(id(9), SecretKind::Ssh)]).unwrap();
        assert_eq!(account_names(&mem), [vault_key(id(2), SecretKind::Password)]);
    }
}
