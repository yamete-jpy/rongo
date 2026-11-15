//! The vault: every saved secret (connection passwords, SSH passwords and
//! passphrases, the Collection History key) in one encrypted file, opened with
//! the master password. The same code on every OS.
//!
//! Layout: the magic line `RONGOVAULT1`, a one-line JSON header
//! `{"v":1,"kdf":"argon2id","m":…,"t":…,"p":…,"salt":"<16 bytes hex>","id":"<uuid>"}`,
//! then a 24-byte nonce and the XChaCha20-Poly1305 ciphertext of the secrets
//! as a JSON map (names from `vault_key` → values). The key is Argon2id of the
//! password exactly as typed (no trimming, no normalisation) with the header's
//! cost and salt. The associated data is everything before the nonce, so an
//! edited header fails just as a wrong password does. `id` is new for every
//! vault made; the salt is new for every password.
//!
//! Writes never trust the copy in memory: holding an OS lock on `vault.lock`
//! beside the file, each one re-reads the vault, checks it is still the one
//! this handle unlocked (same header, so the same id and salt; otherwise
//! `VaultChanged`), applies its one change to what is there, and replaces the
//! file through `vault.tmp`. So two rongo windows keep each other's secrets,
//! and a window never writes over a vault whose password was changed, or which
//! was reset or deleted, elsewhere. Reads come from the copy in memory;
//! `reload_if_changed` catches it up with another window's writes.

pub mod keychain;

use std::collections::BTreeMap;
use std::fs::{self, File, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{CoreError, CoreResult};
use crate::fsx;
use crate::history::HistoryKey;

const MAGIC: &[u8] = b"RONGOVAULT1\n";
const VERSION: u32 = 1;
const KDF: &str = "argon2id";
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;
/// A header line is about 150 bytes; no newline within this many is a damaged file.
const MAX_HEADER: usize = 1024;
/// How long a write waits for another window to let go of `vault.lock`. One
/// writer can hold it for as long as its slowest steps take on Windows, where
/// an antivirus scan can hold a file: removing an old temporary file and
/// renaming over the vault each wait up to `fsx::HOLD_WAIT`. So twice that,
/// and two seconds more, so another window says "busy" only for a stuck holder.
const LOCK_WAIT: Duration = fsx::HOLD_WAIT.saturating_mul(2).saturating_add(Duration::from_secs(2));

/// What tests that hold the lock wait instead of `LOCK_WAIT`, so none of
/// them waits the full time (this thread only; see `short_lock_wait`).
#[cfg(test)]
pub(crate) const SHORT_LOCK_WAIT: Duration = Duration::from_millis(300);

#[cfg(test)]
thread_local! {
    static LOCK_WAIT_OVERRIDE: std::cell::Cell<Option<Duration>> = const { std::cell::Cell::new(None) };
}

/// While it lives, locks taken on this thread wait `SHORT_LOCK_WAIT`.
#[cfg(test)]
pub(crate) struct ShortLockWait;

#[cfg(test)]
pub(crate) fn short_lock_wait() -> ShortLockWait {
    LOCK_WAIT_OVERRIDE.with(|w| w.set(Some(SHORT_LOCK_WAIT)));
    ShortLockWait
}

#[cfg(test)]
impl Drop for ShortLockWait {
    fn drop(&mut self) {
        LOCK_WAIT_OVERRIDE.with(|w| w.set(None));
    }
}

fn lock_wait() -> Duration {
    #[cfg(test)]
    if let Some(short) = LOCK_WAIT_OVERRIDE.with(|w| w.get()) {
        return short;
    }
    LOCK_WAIT
}
/// The fewest characters (`char`s) a new master password may have.
pub const MIN_PASSWORD_CHARS: usize = 8;

type Key = [u8; 32];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SecretKind {
    Password,
    Ssh,
    /// Seals Collection History; one per install, kept under `Uuid::nil()`.
    HistoryKey,
}

impl SecretKind {
    fn suffix(self) -> &'static str {
        match self {
            SecretKind::Password => "password",
            SecretKind::Ssh => "ssh",
            SecretKind::HistoryKey => "history-key",
        }
    }
}

/// A secret's name in the vault (and the Keychain account name older versions
/// used): `"<profile id>:password"`, `"<profile id>:ssh"`, `"<nil uuid>:history-key"`.
pub fn vault_key(id: Uuid, kind: SecretKind) -> String {
    format!("{id}:{}", kind.suffix())
}

/// A new master password needs at least `MIN_PASSWORD_CHARS` characters,
/// counted as `char`s so Thai counts the same as Latin. Nothing else is
/// checked: the password is used exactly as typed.
pub fn check_new_password(p: &str) -> Result<(), String> {
    if p.chars().count() < MIN_PASSWORD_CHARS {
        return Err(format!("the master password needs at least {MIN_PASSWORD_CHARS} characters"));
    }
    Ok(())
}

/// Argon2id cost: memory in KiB, passes, lanes. Each vault's header holds its
/// own, so a vault keeps opening after the default changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KdfParams {
    pub m_kib: u32,
    pub t: u32,
    pub p: u32,
}

impl KdfParams {
    /// About 0.5 s for one derivation on an Apple-silicon Mac in a release
    /// build (`report_unlock_time`), using 256 MiB while it runs.
    pub const DEFAULT: KdfParams = KdfParams { m_kib: 256 * 1024, t: 5, p: 1 };
    /// Next to nothing, for tests.
    pub const TEST: KdfParams = KdfParams { m_kib: 8, t: 1, p: 1 };

    /// What a header may ask for: at most 1 GiB and 32 passes, so a damaged
    /// header can't make unlocking take all memory or hours.
    fn in_bounds(self) -> bool {
        (1..=16).contains(&self.p) && (8 * self.p..=1 << 20).contains(&self.m_kib) && (1..=32).contains(&self.t)
    }
}

fn derive(password: &str, params: KdfParams, salt: &[u8; SALT_LEN]) -> Result<Key, argon2::Error> {
    let params = Params::new(params.m_kib, params.t, params.p, Some(32))?;
    let mut key = [0u8; 32];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params).hash_password_into(password.as_bytes(), salt, &mut key)?;
    Ok(key)
}

fn kdf_err(e: argon2::Error) -> CoreError {
    CoreError::Other(format!("vault: key derivation failed: {e}"))
}

fn io_err(e: std::io::Error) -> CoreError {
    CoreError::Other(format!("vault: {e}"))
}

fn is_not_found(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::NotFound
}

fn same_key(a: &Key, b: &Key) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn cipher(key: &Key) -> XChaCha20Poly1305 {
    XChaCha20Poly1305::new(key.into())
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn unhex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != 2 * N || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; N];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Header {
    params: KdfParams,
    salt: [u8; SALT_LEN],
    id: Uuid,
}

/// The header line as stored; the field order is the file format.
#[derive(Serialize, Deserialize)]
struct HeaderLine {
    v: u32,
    kdf: String,
    m: u32,
    t: u32,
    p: u32,
    salt: String,
    id: Uuid,
}

#[derive(Deserialize)]
struct VersionOnly {
    v: u32,
}

impl Header {
    /// The magic line and the header line: how the file starts, and the
    /// associated data the body is sealed with.
    fn prefix(&self) -> Vec<u8> {
        let line = HeaderLine { v: VERSION, kdf: KDF.into(), m: self.params.m_kib, t: self.params.t, p: self.params.p, salt: hex(&self.salt), id: self.id };
        let mut out = MAGIC.to_vec();
        out.extend(serde_json::to_vec(&line).expect("a header always serialises"));
        out.push(b'\n');
        out
    }
}

/// A vault file cut into its parts; only the layout is checked.
struct Sealed<'a> {
    header: Header,
    aad: &'a [u8],
    nonce: &'a [u8],
    ct: &'a [u8],
}

enum Unreadable {
    Damaged,
    Version(u32),
}

impl From<Unreadable> for CoreError {
    fn from(u: Unreadable) -> Self {
        match u {
            Unreadable::Damaged => CoreError::WrongPassword,
            Unreadable::Version(v) => CoreError::Other(format!("the vault's format ({v}) is not one this version of rongo reads")),
        }
    }
}

fn parse(bytes: &[u8]) -> Result<Sealed<'_>, Unreadable> {
    let rest = bytes.strip_prefix(MAGIC).ok_or(Unreadable::Damaged)?;
    let end = rest.iter().take(MAX_HEADER).position(|&b| b == b'\n').ok_or(Unreadable::Damaged)?;
    let line = &rest[..end];
    // A vault from a newer rongo says so rather than looking damaged, which would invite a Reset.
    let v = serde_json::from_slice::<VersionOnly>(line).map_err(|_| Unreadable::Damaged)?.v;
    if v != VERSION {
        return Err(Unreadable::Version(v));
    }
    let line: HeaderLine = serde_json::from_slice(line).map_err(|_| Unreadable::Damaged)?;
    let params = KdfParams { m_kib: line.m, t: line.t, p: line.p };
    let salt = unhex(&line.salt).ok_or(Unreadable::Damaged)?;
    if line.kdf != KDF || !params.in_bounds() {
        return Err(Unreadable::Damaged);
    }
    let aad_len = MAGIC.len() + end + 1;
    let body = &bytes[aad_len..];
    if body.len() < NONCE_LEN + TAG_LEN {
        return Err(Unreadable::Damaged);
    }
    Ok(Sealed { header: Header { params, salt, id: line.id }, aad: &bytes[..aad_len], nonce: &body[..NONCE_LEN], ct: &body[NONCE_LEN..] })
}

/// The secrets, if `key` opens the body and the header is the one it was sealed with.
fn open(key: &Key, s: &Sealed) -> Option<BTreeMap<String, String>> {
    let plain = cipher(key).decrypt(XNonce::from_slice(s.nonce), Payload { msg: s.ct, aad: s.aad }).ok()?;
    serde_json::from_slice(&plain).ok()
}

fn seal(header: &Header, key: &Key, secrets: &BTreeMap<String, String>) -> CoreResult<Vec<u8>> {
    let mut out = header.prefix();
    let plain = serde_json::to_vec(secrets).map_err(|e| CoreError::Other(format!("vault: {e}")))?;
    let nonce: [u8; NONCE_LEN] = rand::random();
    let ct = cipher(key).encrypt(XNonce::from_slice(&nonce), Payload { msg: &plain, aad: &out }).map_err(|_| CoreError::Other("vault: could not encrypt".into()))?;
    out.extend_from_slice(&nonce);
    out.extend(ct);
    Ok(out)
}

/// Holds the lock on `<name>.lock` beside `path` (`vault.lock`,
/// `profiles.lock`) until dropped; `None` when another holder kept it for all
/// of `LOCK_WAIT` (a write takes milliseconds, so that holder is stuck). It is
/// an OS lock, so the OS lets go of it when the process ends, however it
/// ends: a crash leaves no stale lock. The file itself stays.
pub(crate) fn lock(path: &Path) -> std::io::Result<Option<File>> {
    let f = fsx::private_options().read(true).write(true).create(true).truncate(false).open(path.with_extension("lock"))?;
    let deadline = Instant::now() + lock_wait();
    loop {
        match f.try_lock() {
            Ok(()) => return Ok(Some(f)),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Error(e)) => return Err(e),
        }
    }
}

/// `vault.reset`, beside the vault: left by a Reset until a vault is made again.
fn reset_marker(path: &Path) -> PathBuf {
    path.with_extension("reset")
}

/// Writes `bytes` to `vault.tmp`, makes them durable and renames the file over
/// `path`, so the vault is always the old file or the new one, never part of
/// either. A vault held open (antivirus, on Windows) is waited for, briefly.
fn replace(path: &Path, bytes: &[u8]) -> CoreResult<()> {
    replace_through(path, &path.with_extension("tmp"), bytes).map_err(io_err)
}

/// How rongo's saved files (`profiles.json`, `settings.json`) are replaced:
/// through a temporary file beside `path` named for this process
/// (`profiles.json.<pid>.tmp`), synced before it is renamed over `path`, and
/// private (`fsx`). A crash or a full disk leaves the old file whole. The
/// folder must exist.
pub fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    replace_through(path, &path.with_file_name(format!("{name}.{}.tmp", std::process::id())), bytes)
}

/// Removes what `write_atomically` left beside `path` in other processes (a
/// crash between the write and the rename): `<name>.<pid>.tmp` with another
/// pid, last changed at least `older_than` ago. Best effort. With the file's
/// lock held no other process is writing one, so `Duration::ZERO`; without a
/// lock, an age a write could never take.
pub fn remove_stale_temps(path: &Path, older_than: Duration) {
    let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else { return };
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let Ok(entries) = fs::read_dir(dir) else { return };
    let own = std::process::id().to_string();
    for e in entries.flatten() {
        let file = e.file_name();
        let pid = file.to_str().and_then(|f| f.strip_prefix(name.as_str())).and_then(|f| f.strip_prefix('.')).and_then(|f| f.strip_suffix(".tmp"));
        if !pid.is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) && p != own) {
            continue;
        }
        // A time in the future (clock moved back) counts as just now.
        if e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().unwrap_or_default() >= older_than) {
            let _ = fs::remove_file(e.path());
        }
    }
}

/// Writes `bytes` to `tmp` (made afresh, private), makes them durable and
/// renames `tmp` over `path` (`fsx::rename`, which waits out a file held
/// open): `path` is always the old file or the new one, never part of
/// either. A failure removes `tmp`.
fn replace_through(path: &Path, tmp: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let written = (|| {
        // One left by a crash or a failed write: made afresh, so it is private whatever that one was.
        match fsx::remove_file(tmp) {
            Err(e) if !is_not_found(&e) => return Err(e),
            _ => {}
        }
        let mut f = fsx::private_options().write(true).create_new(true).open(tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        fsx::rename(tmp, path)
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(tmp);
        return Err(e);
    }
    // The rename itself, durable too (best effort): e.g. the Keychain copy is deleted once a new vault is written.
    if let Some(dir) = path.parent() {
        fsx::sync_dir(dir);
    }
    Ok(())
}

fn make_private_dir(dir: &Path) -> CoreResult<()> {
    fsx::create_private_dir(dir).map_err(io_err)
}

/// The history key is made with the vault and then never replaced: history
/// sealed with it could not be read again.
fn refuse_history_key(kind: SecretKind) -> CoreResult<()> {
    if kind == SecretKind::HistoryKey {
        return Err(CoreError::InvalidInput("the Collection History key is made with the vault and never replaced".into()));
    }
    Ok(())
}

/// One change in a batch (`Vault::apply`). No `Debug`: it may hold a secret.
#[derive(Clone, Copy)]
pub enum Change<'a> {
    Set(Uuid, SecretKind, &'a str),
    Delete(Uuid, SecretKind),
}

impl Change<'_> {
    fn kind(&self) -> SecretKind {
        match *self {
            Change::Set(_, kind, _) | Change::Delete(_, kind) => kind,
        }
    }
}

/// An unlocked vault. Reads come from memory; every write goes through the
/// file (see the module notes). Shared between threads behind an `Arc`.
pub struct Vault {
    path: PathBuf,
    state: Mutex<State>,
    /// Files written (tests count them).
    #[cfg(test)]
    writes: std::sync::atomic::AtomicUsize,
}

struct State {
    /// The header this handle unlocked (or last wrote), and its key.
    header: Header,
    key: Key,
    secrets: BTreeMap<String, String>,
    history: HistoryKey,
    /// The file as this handle last read or wrote it: other bytes mean
    /// another window wrote since (or the file changed some other way).
    seen: Vec<u8>,
}

/// A change of password: the header it started from, and the new header and key.
struct Rekey {
    from: Header,
    header: Header,
    key: Key,
}

impl std::fmt::Debug for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vault").field("path", &self.path).finish_non_exhaustive()
    }
}

impl Vault {
    fn new(path: PathBuf, state: State) -> Vault {
        Vault {
            path,
            state: Mutex::new(state),
            #[cfg(test)]
            writes: Default::default(),
        }
    }

    pub fn exists(path: &Path) -> bool {
        path.exists()
    }

    /// Makes a new vault at `path` holding `seed` (secrets moved from the
    /// Keychain, or nothing) and a new history key unless `seed` has one.
    /// Never replaces a file already there (`VaultExists`).
    pub fn create(path: PathBuf, password: &str, params: KdfParams, mut seed: BTreeMap<String, String>) -> CoreResult<Vault> {
        check_new_password(password).map_err(CoreError::InvalidInput)?;
        if !params.in_bounds() {
            return Err(CoreError::InvalidInput(format!("vault: Argon2 cost out of range: {params:?}")));
        }
        let name = vault_key(Uuid::nil(), SecretKind::HistoryKey);
        let history = match seed.get(&name) {
            Some(hex) => HistoryKey::from_hex(hex).ok_or_else(|| CoreError::History("the key being moved into the vault is damaged (expected 64 hex characters)".into()))?,
            None => {
                let fresh = HistoryKey::generate();
                seed.insert(name, fresh.to_hex());
                fresh
            }
        };
        // Refused before the slow derivation too; what counts is the check under the lock.
        if Vault::exists(&path) {
            return Err(CoreError::VaultExists);
        }
        let header = Header { params, salt: rand::random(), id: Uuid::new_v4() };
        let key = derive(password, params, &header.salt).map_err(kdf_err)?;
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            make_private_dir(dir)?;
        }
        let Some(_lock) = lock(&path).map_err(io_err)? else { return Err(CoreError::VaultBusy) };
        match fs::symlink_metadata(&path) {
            Ok(_) => return Err(CoreError::VaultExists),
            Err(e) if is_not_found(&e) => {}
            Err(e) => return Err(io_err(e)),
        }
        let seen = seal(&header, &key, &seed)?;
        replace(&path, &seen)?;
        // The vault is on disk: a Reset before it no longer needs remembering (best effort).
        let _ = fs::remove_file(reset_marker(&path));
        Ok(Vault::new(path, State { header, key, secrets: seed, history, seen }))
    }

    /// Opens the vault at `path`. Reads only; a wrong password and a damaged
    /// file are both `WrongPassword`.
    pub fn unlock(path: PathBuf, password: &str) -> CoreResult<Vault> {
        let bytes = fs::read(&path).map_err(io_err)?;
        let sealed = parse(&bytes)?;
        let key = derive(password, sealed.header.params, &sealed.header.salt).map_err(kdf_err)?;
        let secrets = open(&key, &sealed).ok_or(CoreError::WrongPassword)?;
        let history = secrets.get(&vault_key(Uuid::nil(), SecretKind::HistoryKey)).and_then(|h| HistoryKey::from_hex(h)).ok_or_else(|| CoreError::History("the key in the vault is missing or damaged".into()))?;
        let header = sealed.header;
        Ok(Vault::new(path, State { header, key, secrets, history, seen: bytes }))
    }

    /// Reset: deletes the vault at `path`, every secret in it and the history
    /// key too, first leaving the marker `was_reset` looks for. Under the same
    /// lock as writes, so a write from another window is not half way through;
    /// that window's handle writes nothing after (`VaultChanged`). A vault
    /// already gone is not an error; the marker is left all the same.
    pub fn reset(path: &Path) -> CoreResult<()> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            make_private_dir(dir)?;
        }
        let Some(_lock) = lock(path).map_err(io_err)? else { return Err(CoreError::VaultBusy) };
        // Before the vault goes: a Reset that stopped half way still keeps the Keychain's leftovers out.
        let at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        fsx::private_options().write(true).create(true).truncate(true).open(reset_marker(path)).and_then(|mut f| writeln!(f, "reset at {at} (seconds since 1970)")).map_err(io_err)?;
        match fsx::remove_file(path) {
            Err(e) if !is_not_found(&e) => Err(io_err(e)),
            _ => Ok(()),
        }
    }

    /// Whether the vault at `path` was reset and no new one made since
    /// (`vault.reset` beside it). Then nothing is moved from the Keychain:
    /// what a refused clean-up left there must not come back. Holds a time,
    /// never a secret.
    pub fn was_reset(path: &Path) -> bool {
        reset_marker(path).exists()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// As of this handle's last unlock, write or `reload_if_changed`.
    pub fn get(&self, id: Uuid, kind: SecretKind) -> Option<String> {
        self.state().secrets.get(&vault_key(id, kind)).cloned()
    }

    pub fn set(&self, id: Uuid, kind: SecretKind, secret: &str) -> CoreResult<()> {
        self.apply(&[Change::Set(id, kind, secret)])
    }

    pub fn delete(&self, id: Uuid, kind: SecretKind) -> CoreResult<()> {
        self.apply(&[Change::Delete(id, kind)])
    }

    /// Several changes in one write: one lock, one read of the file and one
    /// replace, so either all of them are written or (one refused, or the
    /// vault changed elsewhere) none. Nothing to change touches nothing.
    pub fn apply(&self, changes: &[Change]) -> CoreResult<()> {
        if changes.is_empty() {
            return Ok(());
        }
        changes.iter().try_for_each(|c| refuse_history_key(c.kind()))?;
        self.write(None, |m| {
            for c in changes {
                match *c {
                    Change::Set(id, kind, secret) => _ = m.insert(vault_key(id, kind), secret.to_string()),
                    Change::Delete(id, kind) => _ = m.remove(&vault_key(id, kind)),
                }
            }
        })
    }

    /// Files this handle wrote.
    #[cfg(test)]
    pub(crate) fn writes(&self) -> usize {
        self.writes.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn history_key(&self) -> HistoryKey {
        self.state().history.clone()
    }

    /// Seals the same secrets (history key included) under `new` and a new
    /// salt; the cost stays the vault's own.
    pub fn change_password(&self, old: &str, new: &str) -> CoreResult<()> {
        check_new_password(new).map_err(CoreError::InvalidInput)?;
        // The two derivations take about a second: done without holding the state, so reads go on meanwhile.
        let (from, key) = {
            let st = self.state();
            (st.header, st.key)
        };
        if !same_key(&derive(old, from.params, &from.salt).map_err(kdf_err)?, &key) {
            return Err(CoreError::WrongPassword);
        }
        let header = Header { salt: rand::random(), ..from };
        let key = derive(new, header.params, &header.salt).map_err(kdf_err)?;
        self.write(Some(Rekey { from, header, key }), |_| {})
    }

    /// Catches this handle's copy up with the file: when another window wrote
    /// the same vault (same header: same id, salt and password) since this
    /// handle last read or wrote it, its secrets replace the copy in memory.
    /// Writes nothing. `VaultChanged` when the file is no longer the vault
    /// this handle holds (its password changed, reset, deleted or damaged);
    /// the copy in memory then stays as it was.
    pub fn reload_if_changed(&self) -> CoreResult<()> {
        let mut st = self.state();
        let (_lock, bytes) = self.read_locked()?;
        if bytes == st.seen {
            return Ok(());
        }
        let Ok(sealed) = parse(&bytes) else { return Err(CoreError::VaultChanged) };
        if sealed.header != st.header {
            return Err(CoreError::VaultChanged);
        }
        let Some(secrets) = open(&st.key, &sealed) else { return Err(CoreError::VaultChanged) };
        st.secrets = secrets;
        st.seen = bytes;
        Ok(())
    }

    /// Takes `vault.lock` (held until the returned file is dropped) and reads
    /// the vault. A vault or folder that is gone is `VaultChanged`.
    fn read_locked(&self) -> CoreResult<(File, Vec<u8>)> {
        let lock = match lock(&self.path) {
            Ok(Some(f)) => f,
            Ok(None) => return Err(CoreError::VaultBusy),
            // The vault's folder is gone.
            Err(e) if is_not_found(&e) => return Err(CoreError::VaultChanged),
            Err(e) => return Err(io_err(e)),
        };
        match fs::read(&self.path) {
            Ok(b) => Ok((lock, b)),
            Err(e) if is_not_found(&e) => Err(CoreError::VaultChanged),
            Err(e) => Err(io_err(e)),
        }
    }

    /// Applies `change` to the vault as it is on disk now and keeps the result
    /// as this handle's copy; with `rekey`, seals it under the new header and
    /// key. Writes nothing when the file is not the vault this handle holds
    /// (changed, replaced, deleted or damaged): `VaultChanged`.
    fn write(&self, rekey: Option<Rekey>, change: impl FnOnce(&mut BTreeMap<String, String>)) -> CoreResult<()> {
        let mut st = self.state();
        // Another change of password through this handle got in first.
        if rekey.as_ref().is_some_and(|r| r.from != st.header) {
            return Err(CoreError::VaultChanged);
        }
        let (_lock, bytes) = self.read_locked()?;
        let Ok(sealed) = parse(&bytes) else { return Err(CoreError::VaultChanged) };
        if sealed.header != st.header {
            return Err(CoreError::VaultChanged);
        }
        let Some(mut secrets) = open(&st.key, &sealed) else { return Err(CoreError::VaultChanged) };
        let before = secrets.clone();
        change(&mut secrets);
        let seen = if let Some(r) = rekey {
            let sealed = seal(&r.header, &r.key, &secrets)?;
            self.replace(&sealed)?;
            st.header = r.header;
            st.key = r.key;
            sealed
        } else if secrets != before {
            let sealed = seal(&st.header, &st.key, &secrets)?;
            self.replace(&sealed)?;
            sealed
        } else {
            bytes
        };
        st.secrets = secrets;
        st.seen = seen;
        Ok(())
    }

    fn replace(&self, sealed: &[u8]) -> CoreResult<()> {
        replace(&self.path, sealed)?;
        #[cfg(test)]
        self.writes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::testkit::TempDir;

    impl TempDir {
        fn vault(&self) -> PathBuf {
            self.0.join("vault")
        }

        fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(&self.0).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
            names.sort();
            names
        }
    }

    const PW: &str = "correct horse battery";

    fn create(t: &TempDir) -> Vault {
        Vault::create(t.vault(), PW, KdfParams::TEST, BTreeMap::new()).unwrap()
    }

    fn unlock(t: &TempDir) -> Vault {
        Vault::unlock(t.vault(), PW).unwrap()
    }

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn create_then_unlock_reads_back_every_secret() {
        let t = TempDir::new();
        let v = create(&t);
        v.set(id(1), SecretKind::Password, "pw one").unwrap();
        v.set(id(1), SecretKind::Ssh, "ssh one").unwrap();
        v.set(id(2), SecretKind::Password, "pw two").unwrap();
        v.set(id(2), SecretKind::Ssh, "ssh two").unwrap();
        assert_eq!(v.get(id(1), SecretKind::Password).as_deref(), Some("pw one"));
        assert_eq!(v.path(), t.vault());
        let hk = v.history_key();
        assert!(!format!("{v:?}").contains("pw one"));
        drop(v);

        // Nothing readable on disk: not a secret, not the history key.
        let bytes = fs::read(t.vault()).unwrap();
        assert!(bytes.starts_with(MAGIC));
        for plain in ["pw one", "ssh two", &hk.to_hex()] {
            assert!(!contains(&bytes, plain.as_bytes()), "{plain} is on disk");
        }

        let v = unlock(&t);
        assert_eq!(v.get(id(1), SecretKind::Password).as_deref(), Some("pw one"));
        assert_eq!(v.get(id(1), SecretKind::Ssh).as_deref(), Some("ssh one"));
        assert_eq!(v.get(id(2), SecretKind::Password).as_deref(), Some("pw two"));
        assert_eq!(v.get(id(2), SecretKind::Ssh).as_deref(), Some("ssh two"));
        assert_eq!(v.get(id(3), SecretKind::Password), None);
        assert_eq!(v.history_key(), hk);

        v.delete(id(1), SecretKind::Ssh).unwrap();
        v.delete(id(3), SecretKind::Ssh).unwrap();
        assert_eq!(v.get(id(1), SecretKind::Ssh), None);
        let v = unlock(&t);
        assert_eq!(v.get(id(1), SecretKind::Ssh), None);
        assert_eq!(v.get(id(1), SecretKind::Password).as_deref(), Some("pw one"));
    }

    #[test]
    fn a_wrong_password_is_refused_and_changes_nothing() {
        let t = TempDir::new();
        create(&t).set(id(1), SecretKind::Password, "secret").unwrap();
        let before = fs::read(t.vault()).unwrap();
        let names = t.names();
        assert_eq!(Vault::unlock(t.vault(), "wrong").err(), Some(CoreError::WrongPassword));
        assert_eq!(Vault::unlock(t.vault(), "").err(), Some(CoreError::WrongPassword));
        assert_eq!(fs::read(t.vault()).unwrap(), before);
        assert_eq!(t.names(), names);
    }

    #[test]
    fn an_edited_header_or_body_is_refused() {
        let t = TempDir::new();
        create(&t).set(id(1), SecretKind::Password, "secret").unwrap();
        let good = fs::read(t.vault()).unwrap();
        let end = MAGIC.len() + good[MAGIC.len()..].iter().position(|&b| b == b'\n').unwrap();
        let header = std::str::from_utf8(&good[MAGIC.len()..end]).unwrap();
        // The first character of a field's value, changed to another digit so the header still parses.
        let value_at = |field: &str| MAGIC.len() + header.find(&format!("\"{field}\":")).unwrap() + field.len() + 3;
        let salt = value_at("salt") + 1;
        let vault_id = value_at("id") + 1;
        let passes = value_at("t");
        let body = good.len() - 1;
        for (i, at) in [salt, vault_id, passes, body].into_iter().enumerate() {
            let mut bad = good.clone();
            bad[at] = if at == body { bad[at] ^ 1 } else if bad[at] == b'1' { b'2' } else { b'1' };
            let copy = t.0.join(format!("edited-{i}"));
            fs::write(&copy, &bad).unwrap();
            assert_eq!(Vault::unlock(copy.clone(), PW).err(), Some(CoreError::WrongPassword), "edit {i}");
            assert_eq!(fs::read(&copy).unwrap(), bad);
        }
        let cut = t.0.join("cut");
        fs::write(&cut, &good[..good.len() - 20]).unwrap();
        assert_eq!(Vault::unlock(cut, PW).err(), Some(CoreError::WrongPassword));
        assert!(Vault::unlock(t.vault(), PW).is_ok());
    }

    #[test]
    fn create_refuses_an_existing_vault() {
        let t = TempDir::new();
        create(&t).set(id(1), SecretKind::Password, "kept").unwrap();
        assert_eq!(Vault::create(t.vault(), "another password", KdfParams::TEST, BTreeMap::new()).err(), Some(CoreError::VaultExists));
        assert_eq!(unlock(&t).get(id(1), SecretKind::Password).as_deref(), Some("kept"));
        assert_eq!(Vault::unlock(t.vault(), "another password").err(), Some(CoreError::WrongPassword));
    }

    #[test]
    fn create_keeps_a_seed_and_its_history_key() {
        let t = TempDir::new();
        let hk = HistoryKey::new([9; 32]);
        let seed = BTreeMap::from([(vault_key(id(1), SecretKind::Password), "moved".to_string()), (vault_key(Uuid::nil(), SecretKind::HistoryKey), hk.to_hex())]);
        let v = Vault::create(t.vault(), PW, KdfParams::TEST, seed).unwrap();
        assert_eq!(v.history_key(), hk);
        let v = unlock(&t);
        assert_eq!(v.history_key(), hk);
        assert_eq!(v.get(id(1), SecretKind::Password).as_deref(), Some("moved"));

        // A damaged key is never swapped for a new one: history sealed with the real one could not be read again.
        let t = TempDir::new();
        let seed = BTreeMap::from([(vault_key(Uuid::nil(), SecretKind::HistoryKey), "not a key".to_string())]);
        assert!(matches!(Vault::create(t.vault(), PW, KdfParams::TEST, seed), Err(CoreError::History(_))));
        assert!(!Vault::exists(&t.vault()));
    }

    #[test]
    fn the_history_key_cannot_be_replaced() {
        let t = TempDir::new();
        let v = create(&t);
        let hk = v.history_key();
        assert!(matches!(v.set(Uuid::nil(), SecretKind::HistoryKey, &HistoryKey::new([1; 32]).to_hex()), Err(CoreError::InvalidInput(_))));
        assert!(matches!(v.delete(Uuid::nil(), SecretKind::HistoryKey), Err(CoreError::InvalidInput(_))));
        assert_eq!(v.history_key(), hk);
        assert_eq!(unlock(&t).history_key(), hk);
    }

    #[test]
    fn two_handles_keep_each_others_writes() {
        let t = TempDir::new();
        drop(create(&t));
        let a = unlock(&t);
        let b = unlock(&t);
        a.set(id(1), SecretKind::Password, "from a").unwrap();
        b.set(id(2), SecretKind::Password, "from b").unwrap();
        // B's write started from the file, so B now knows A's secret too.
        assert_eq!(b.get(id(1), SecretKind::Password).as_deref(), Some("from a"));
        let c = unlock(&t);
        assert_eq!(c.get(id(1), SecretKind::Password).as_deref(), Some("from a"));
        assert_eq!(c.get(id(2), SecretKind::Password).as_deref(), Some("from b"));

        // A delete through B is not undone by A's next write.
        b.delete(id(1), SecretKind::Password).unwrap();
        a.set(id(3), SecretKind::Ssh, "a again").unwrap();
        let c = unlock(&t);
        assert_eq!(c.get(id(1), SecretKind::Password), None);
        assert_eq!(c.get(id(2), SecretKind::Password).as_deref(), Some("from b"));
        assert_eq!(c.get(id(3), SecretKind::Ssh).as_deref(), Some("a again"));
    }

    /// A batch is one write: several sets and deletes under one lock, one
    /// read and one replace of the file, so all of them are written or none.
    /// A change refused (the history key) or a vault changed elsewhere
    /// writes none of it; an empty batch touches nothing.
    #[test]
    fn a_batch_is_one_write_all_or_nothing() {
        let t = TempDir::new();
        let v = create(&t);
        v.set(id(1), SecretKind::Password, "old").unwrap();
        v.set(id(1), SecretKind::Ssh, "old ssh").unwrap();
        let writes = v.writes();
        v.apply(&[Change::Set(id(1), SecretKind::Password, "new"), Change::Delete(id(1), SecretKind::Ssh), Change::Set(id(2), SecretKind::Ssh, "two")]).unwrap();
        assert_eq!(v.writes(), writes + 1, "one write");
        assert_eq!(v.get(id(1), SecretKind::Password).as_deref(), Some("new"));
        assert_eq!(v.get(id(1), SecretKind::Ssh), None);
        let w = unlock(&t);
        assert_eq!(w.get(id(1), SecretKind::Password).as_deref(), Some("new"));
        assert_eq!(w.get(id(1), SecretKind::Ssh), None);
        assert_eq!(w.get(id(2), SecretKind::Ssh).as_deref(), Some("two"));

        let before = fs::read(t.vault()).unwrap();
        let refused = v.apply(&[Change::Set(id(3), SecretKind::Password, "lost"), Change::Delete(Uuid::nil(), SecretKind::HistoryKey)]);
        assert!(matches!(refused, Err(CoreError::InvalidInput(_))), "{refused:?}");
        assert_eq!(fs::read(t.vault()).unwrap(), before);
        assert_eq!(v.get(id(3), SecretKind::Password), None);

        unlock(&t).change_password(PW, "another password").unwrap();
        assert_eq!(v.apply(&[Change::Delete(id(1), SecretKind::Password), Change::Set(id(4), SecretKind::Password, "lost")]).err(), Some(CoreError::VaultChanged));
        let after = Vault::unlock(t.vault(), "another password").unwrap();
        assert_eq!(after.get(id(1), SecretKind::Password).as_deref(), Some("new"));
        assert_eq!(after.get(id(4), SecretKind::Password), None);
        assert_eq!(v.apply(&[]), Ok(()), "nothing to write: the file isn't even read");
    }

    #[test]
    fn writers_in_parallel_lose_nothing() {
        fn send_sync<T: Send + Sync>() {}
        send_sync::<Vault>();
        let t = TempDir::new();
        drop(create(&t));
        let shared = Arc::new(unlock(&t));
        let threads: Vec<_> = (0..8u128)
            .map(|n| {
                // Half the threads share one handle, half have their own (another window).
                let shared = shared.clone();
                let path = t.vault();
                std::thread::spawn(move || {
                    let own = (n % 2 == 1).then(|| Vault::unlock(path, PW).unwrap());
                    let v = own.as_ref().unwrap_or(&shared);
                    for i in 0..5 {
                        v.set(id(n * 10 + i), SecretKind::Password, &format!("{n}-{i}")).unwrap();
                    }
                })
            })
            .collect();
        for th in threads {
            th.join().unwrap();
        }
        let v = unlock(&t);
        for n in 0..8u128 {
            for i in 0..5 {
                assert_eq!(v.get(id(n * 10 + i), SecretKind::Password), Some(format!("{n}-{i}")));
            }
        }
    }

    /// One writer can hold `vault.lock` for as long as its slowest steps
    /// take on Windows: removing an old temporary file and renaming it over
    /// the vault, each waiting up to `HOLD_WAIT` for an antivirus scan to let
    /// go. Another window must outwait that before it says "busy".
    #[test]
    fn the_lock_wait_outlasts_a_writer_held_by_antivirus() {
        assert!(LOCK_WAIT >= 2 * fsx::HOLD_WAIT + Duration::from_secs(2), "{LOCK_WAIT:?}");
    }

    #[test]
    fn a_held_lock_fails_after_a_while() {
        let _short = short_lock_wait();
        let t = TempDir::new();
        let v = create(&t);
        let before = fs::read(t.vault()).unwrap();
        let held = File::options().read(true).write(true).open(t.vault().with_extension("lock")).unwrap();
        held.lock().unwrap();
        // Let go after 6 s whatever happens, so a write that waits forever fails this test instead of hanging it.
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            let _ = rx.recv_timeout(Duration::from_secs(6));
            drop(held);
        });
        let start = Instant::now();
        assert_eq!(v.set(id(1), SecretKind::Password, "waits").err(), Some(CoreError::VaultBusy));
        let waited = start.elapsed();
        assert!(waited >= SHORT_LOCK_WAIT && waited < Duration::from_secs(5), "waited {waited:?}");
        assert_eq!(fs::read(t.vault()).unwrap(), before);
        assert_eq!(v.get(id(1), SecretKind::Password), None);

        tx.send(()).unwrap();
        holder.join().unwrap();
        v.set(id(1), SecretKind::Password, "after").unwrap();
        assert_eq!(unlock(&t).get(id(1), SecretKind::Password).as_deref(), Some("after"));
    }

    #[test]
    fn reload_if_changed_picks_up_another_handles_writes() {
        let t = TempDir::new();
        drop(create(&t));
        let a = unlock(&t);
        let b = unlock(&t);
        a.set(id(1), SecretKind::Password, "from a").unwrap();
        b.set(id(2), SecretKind::Password, "from b").unwrap();
        // A doesn't see B's write until it catches up.
        assert_eq!(a.get(id(2), SecretKind::Password), None);
        a.reload_if_changed().unwrap();
        assert_eq!(a.get(id(2), SecretKind::Password).as_deref(), Some("from b"));
        assert_eq!(a.get(id(1), SecretKind::Password).as_deref(), Some("from a"));
        // Nothing new: nothing changes, nothing is written.
        let before = fs::read(t.vault()).unwrap();
        a.reload_if_changed().unwrap();
        b.reload_if_changed().unwrap();
        assert_eq!(fs::read(t.vault()).unwrap(), before);
        assert_eq!(b.get(id(1), SecretKind::Password).as_deref(), Some("from a"));
        // A delete elsewhere shows too, and the history key stays.
        let hk = a.history_key();
        b.delete(id(1), SecretKind::Password).unwrap();
        a.reload_if_changed().unwrap();
        assert_eq!(a.get(id(1), SecretKind::Password), None);
        assert_eq!(a.history_key(), hk);
        assert_eq!(a.path(), t.vault());
    }

    #[test]
    fn reload_if_changed_refuses_another_vault() {
        let t = TempDir::new();
        drop(create(&t));
        let a = unlock(&t);
        a.set(id(1), SecretKind::Password, "kept").unwrap();
        let b = unlock(&t);
        b.change_password(PW, "a new password").unwrap();
        b.set(id(2), SecretKind::Password, "under the new password").unwrap();
        assert_eq!(a.reload_if_changed().err(), Some(CoreError::VaultChanged));
        // A keeps what it had, and writes nothing.
        assert_eq!(a.get(id(1), SecretKind::Password).as_deref(), Some("kept"));
        assert_eq!(a.get(id(2), SecretKind::Password), None);
        assert_eq!(Vault::unlock(t.vault(), "a new password").unwrap().get(id(2), SecretKind::Password).as_deref(), Some("under the new password"));

        // Damaged, gone, or replaced by a new vault: the same.
        let c = Vault::unlock(t.vault(), "a new password").unwrap();
        let good = fs::read(t.vault()).unwrap();
        let mut bad = good.clone();
        *bad.last_mut().unwrap() ^= 1;
        fs::write(t.vault(), &bad).unwrap();
        assert_eq!(c.reload_if_changed().err(), Some(CoreError::VaultChanged));
        fs::remove_file(t.vault()).unwrap();
        assert_eq!(c.reload_if_changed().err(), Some(CoreError::VaultChanged));
        assert!(!t.vault().exists(), "nothing written in its place");
        Vault::create(t.vault(), "a new password", KdfParams::TEST, BTreeMap::new()).unwrap();
        assert_eq!(c.reload_if_changed().err(), Some(CoreError::VaultChanged));
        assert_eq!(c.get(id(2), SecretKind::Password).as_deref(), Some("under the new password"));
    }

    #[test]
    fn a_vault_changed_elsewhere_is_not_overwritten() {
        let t = TempDir::new();
        drop(create(&t));
        let a = unlock(&t);
        let b = unlock(&t);
        b.change_password(PW, "a new password").unwrap();
        assert_eq!(a.set(id(1), SecretKind::Password, "lost").err(), Some(CoreError::VaultChanged));
        assert_eq!(a.delete(id(1), SecretKind::Password).err(), Some(CoreError::VaultChanged));
        assert_eq!(a.change_password(PW, "a third password").err(), Some(CoreError::VaultChanged));
        let c = Vault::unlock(t.vault(), "a new password").unwrap();
        assert_eq!(c.get(id(1), SecretKind::Password), None);

        // Deleted: nothing is written in its place.
        fs::remove_file(t.vault()).unwrap();
        assert_eq!(a.set(id(1), SecretKind::Password, "lost").err(), Some(CoreError::VaultChanged));
        assert_eq!(c.set(id(1), SecretKind::Password, "lost").err(), Some(CoreError::VaultChanged));
        assert!(!t.vault().exists());

        // Replaced by a new vault (Reset, then Set): not written either.
        let d = Vault::create(t.vault(), "a new password", KdfParams::TEST, BTreeMap::new()).unwrap();
        assert_eq!(c.set(id(1), SecretKind::Password, "lost").err(), Some(CoreError::VaultChanged));
        assert_eq!(Vault::unlock(t.vault(), "a new password").unwrap().get(id(1), SecretKind::Password), None);
        assert_ne!(c.history_key(), d.history_key());
    }

    /// Reset deletes the vault under the lock writes take, leaving the
    /// `vault.reset` marker (a time, never a secret) until a new vault is
    /// made, by Set or by a move from the Keychain. A vault already gone (or
    /// its folder) is no error, and another window's handle then writes
    /// nothing. While another window holds the lock it waits, then says busy
    /// and changes nothing.
    #[test]
    fn reset_deletes_the_vault_and_leaves_a_marker() {
        let _short = short_lock_wait();
        let t = TempDir::new();
        let v = create(&t);
        v.set(id(1), SecretKind::Password, "pw one").unwrap();
        assert!(!Vault::was_reset(&t.vault()));
        Vault::reset(&t.vault()).unwrap();
        assert!(!t.vault().exists());
        assert!(Vault::was_reset(&t.vault()));
        let marker = fs::read(t.vault().with_extension("reset")).unwrap();
        assert!(!contains(&marker, b"pw one") && !contains(&marker, PW.as_bytes()) && !contains(&marker, v.history_key().to_hex().as_bytes()));
        assert_eq!(v.set(id(1), SecretKind::Password, "lost").err(), Some(CoreError::VaultChanged));
        assert!(!t.vault().exists(), "nothing written in its place");
        Vault::reset(&t.vault()).unwrap();
        assert!(Vault::was_reset(&t.vault()));

        // Set: the marker goes once the new vault is there.
        drop(create(&t));
        assert!(!Vault::was_reset(&t.vault()));

        let held = File::options().read(true).write(true).open(t.vault().with_extension("lock")).unwrap();
        held.lock().unwrap();
        assert_eq!(Vault::reset(&t.vault()).err(), Some(CoreError::VaultBusy));
        assert!(t.vault().exists() && !Vault::was_reset(&t.vault()), "nothing changed");
        drop(held);
        Vault::reset(&t.vault()).unwrap();
        assert!(!t.vault().exists() && Vault::was_reset(&t.vault()));

        // A move from the Keychain makes a vault too: the marker goes.
        let mem = keychain::Memory::default();
        drop(keychain::move_out(&mem, &[], t.vault(), PW, KdfParams::TEST).unwrap().vault);
        assert!(t.vault().exists() && !Vault::was_reset(&t.vault()));

        // No folder (nothing there to delete): the marker is left all the same.
        let gone = t.0.join("gone").join("vault");
        Vault::reset(&gone).unwrap();
        assert!(Vault::was_reset(&gone) && !gone.exists());
    }

    #[test]
    fn change_password_keeps_the_history_key() {
        let t = TempDir::new();
        let v = create(&t);
        v.set(id(1), SecretKind::Password, "kept").unwrap();
        let hk = v.history_key();
        let before = fs::read(t.vault()).unwrap();
        assert_eq!(v.change_password("not it", "a new password").err(), Some(CoreError::WrongPassword));
        assert!(matches!(v.change_password(PW, "short"), Err(CoreError::InvalidInput(_))));
        assert_eq!(fs::read(t.vault()).unwrap(), before);

        v.change_password(PW, "a new password").unwrap();
        // The handle goes on under the new password.
        v.set(id(2), SecretKind::Password, "after").unwrap();
        assert_eq!(v.history_key(), hk);
        let w = Vault::unlock(t.vault(), "a new password").unwrap();
        assert_eq!(w.history_key(), hk);
        assert_eq!(w.get(id(1), SecretKind::Password).as_deref(), Some("kept"));
        assert_eq!(w.get(id(2), SecretKind::Password).as_deref(), Some("after"));
        assert_eq!(Vault::unlock(t.vault(), PW).err(), Some(CoreError::WrongPassword));
    }

    #[test]
    fn thai_and_spaces_are_kept_as_typed() {
        let t = TempDir::new();
        let pw = " รหัสผ่าน ดี ";
        Vault::create(t.vault(), pw, KdfParams::TEST, BTreeMap::new()).unwrap();
        assert_eq!(Vault::unlock(t.vault(), "รหัสผ่าน ดี").err(), Some(CoreError::WrongPassword));
        assert_eq!(Vault::unlock(t.vault(), pw.trim_end()).err(), Some(CoreError::WrongPassword));
        assert!(Vault::unlock(t.vault(), pw).is_ok());

        // No Unicode normalisation either: a precomposed é is not e + combining accent.
        let t = TempDir::new();
        Vault::create(t.vault(), "caf\u{e9} au lait", KdfParams::TEST, BTreeMap::new()).unwrap();
        assert_eq!(Vault::unlock(t.vault(), "cafe\u{301} au lait").err(), Some(CoreError::WrongPassword));
        assert!(Vault::unlock(t.vault(), "caf\u{e9} au lait").is_ok());
    }

    #[test]
    fn new_passwords_need_eight_characters() {
        assert!(check_new_password("1234567").is_err());
        assert!(check_new_password("12345678").is_ok());
        assert!(check_new_password("รหัสผ่านไทย").is_ok());
        // Counted in characters, not bytes (18 bytes, 6 characters).
        assert!(check_new_password("รหัสผ่").is_err());
        let t = TempDir::new();
        assert!(matches!(Vault::create(t.vault(), "1234567", KdfParams::TEST, BTreeMap::new()), Err(CoreError::InvalidInput(_))));
        assert!(!Vault::exists(&t.vault()));
    }

    #[test]
    fn the_default_cost_is_pinned() {
        // 256 MiB, 5 passes: 0.49 s in release on an M4 Pro (64 MiB × 3 took 0.07 s).
        assert_eq!((KdfParams::DEFAULT.m_kib, KdfParams::DEFAULT.t, KdfParams::DEFAULT.p), (256 * 1024, 5, 1));
    }

    /// `cargo test -p rongo-core --release --lib vault -- --ignored --nocapture report_unlock_time`
    #[test]
    #[ignore]
    fn report_unlock_time() {
        let start = std::time::Instant::now();
        derive("a master password", KdfParams::DEFAULT, &[7; SALT_LEN]).unwrap();
        println!("one Argon2id derivation with {:?}: {:?}", KdfParams::DEFAULT, start.elapsed());
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let t = TempDir::new();
        let path = t.0.join("made").join("vault");
        let v = Vault::create(path.clone(), PW, KdfParams::TEST, BTreeMap::new()).unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
        assert_eq!(mode(&path.with_extension("lock")), 0o600);
        v.set(id(1), SecretKind::Password, "x").unwrap();
        assert_eq!(mode(&path), 0o600);
    }
}
