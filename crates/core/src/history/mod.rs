//! Collection History: before rongo deletes or changes documents it saves them
//! here, encrypted, so they can be restored later. This module is the on-disk
//! store: writing and reading entries, listing, removing and moving them, and
//! retention. The commands (`recorded`, `restore`, `copy`) and the UI use it.
//!
//! Layout: `<root>/<profile id>/<db>/<collection>/<time>-<kind>[-n].rhist`, one
//! file per operation. A file is the magic line `RONGOHIST1`, then sealed
//! records, each `[len: u32 LE][nonce: 24 bytes][ciphertext]` where `len`
//! covers the nonce and the ciphertext. A record is one BSON document sealed on
//! its own with XChaCha20-Poly1305, which is why a file cut short still reads
//! up to its last whole record. The associated data is the file's id (the
//! header record's nonce; zeros for the header itself) and the record's
//! position, so records can be neither dropped, reordered nor spliced in from
//! another entry. A reader stops at the first incomplete or unauthentic record.
//!
//! Records: header `{ k: "h", kind, at, conn, how, db, coll, v }` (`v` is the
//! rongo version); then, per `save` call, a batch marker `{ k: "n" }`, its
//! `{ k: "b", d }` before-images and, for an update, its `{ k: "a", id, h }`
//! results (the hash of the document as it was left); and a last
//! `{ k: "e", count, cancelled }`. The markers bound what a reader holds: one
//! batch.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};

use bson::raw::{RawBsonRef, cstr};
use bson::{Bson, DateTime, Document, RawDocumentBuf, doc};
use ring::digest;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use uuid::Uuid;

use crate::api::Namespace;
use crate::error::{CoreError, CoreResult};
use crate::fsx;

const MAGIC: &[u8] = b"RONGOHIST1\n";
const EXT: &str = "rhist";
const RESTORED_EXT: &str = "restored";
const NONCE_LEN: usize = 24;
const TAG_LEN: usize = 16;
/// A BSON document is at most 16 MB; anything much bigger is a damaged length.
const MAX_RECORD: u64 = 64 << 20;
/// Encoded names longer than this are shortened (see `encode_name`).
const LONG_NAME: usize = 200;
/// Inside a directory with a shortened name: the full name, as UTF-8. It starts
/// with a dot because an encoded name never does, so it can't be a collection.
const NAME_FILE: &str = ".name";
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

fn io_err(e: std::io::Error) -> CoreError {
    CoreError::History(e.to_string())
}

fn history_err(msg: impl Into<String>) -> CoreError {
    CoreError::History(msg.into())
}

/// An error's text without the "Collection History: " its own kind adds.
pub(crate) fn reason(e: CoreError) -> String {
    match e {
        CoreError::History(m) => m,
        e => e.to_string(),
    }
}

fn is_not_found(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::NotFound
}

/// The entry files being written in this process now, by path (shared by
/// every `Store`): retention and every removal leave them alone, and a
/// rename does not move them from under their writer. A path is added
/// before its file is made and dropped when its `EntryWriter` is, so no
/// removal can find the file unprotected; the removals that take whole
/// folders hold the lock while they work, so no writer can begin inside.
static OPEN: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(Default::default);

fn open_writers() -> MutexGuard<'static, HashSet<PathBuf>> {
    OPEN.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The answer of a removal that had to keep `n` entries still being written.
fn kept(n: usize) -> CoreResult<()> {
    match n {
        0 => Ok(()),
        1 => Err(history_err("an entry still being written was kept")),
        n => Err(history_err(format!("{n} entries still being written were kept"))),
    }
}

/// The 256-bit key every record is sealed with. The vault keeps it and hands
/// it out only as `Vault::history_key` (`get` and `set` refuse its kind); the
/// app passes it to `Store::new`.
#[derive(Clone)]
pub struct HistoryKey([u8; 32]);

impl HistoryKey {
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub(crate) fn generate() -> Self {
        Self(rand::random())
    }

    /// 64 lowercase hex characters, for the vault.
    pub(crate) fn to_hex(&self) -> String {
        crate::vault::hex(&self.0)
    }

    pub(crate) fn from_hex(s: &str) -> Option<Self> {
        crate::vault::unhex(s).map(Self)
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new((&self.0).into())
    }

    /// Tells keys apart in `HEADERS` without keeping the key there.
    fn tag(&self) -> [u8; 16] {
        let mut h = digest::Context::new(&digest::SHA256);
        h.update(b"rongo history listing\0");
        h.update(&self.0);
        h.finish().as_ref()[..16].try_into().expect("a SHA-256 has 32 bytes")
    }
}

impl std::fmt::Debug for HistoryKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HistoryKey(…)")
    }
}

impl PartialEq for HistoryKey {
    fn eq(&self, other: &Self) -> bool {
        self.0.iter().zip(&other.0).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
    }
}

impl Eq for HistoryKey {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Delete,
    Update,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Delete => "delete",
            Kind::Update => "update",
        }
    }

    fn from_name(s: &str) -> Option<Self> {
        match s {
            "delete" => Some(Kind::Delete),
            "update" => Some(Kind::Update),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EntryId {
    pub profile: Uuid,
    pub ns: Namespace,
    /// File name inside the collection's directory.
    pub file: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EntryHeader {
    pub id: EntryId,
    pub kind: Kind,
    pub at: DateTime,
    /// Empty when the header could not be decrypted.
    pub conn_name: String,
    /// Before-images actually readable (a complete entry: the end record's count).
    pub count: u64,
    /// The end record was read: the operation finished (or was cancelled cleanly).
    pub complete: bool,
    /// The end record says the operation stopped between batches (cancelled,
    /// or it could not go on); the entry holds what it got to.
    pub cancelled: bool,
    pub restored: bool,
    /// Size of the file.
    pub bytes: u64,
    /// Why the file could not be read just now (an I/O error such as its
    /// permissions, not damage): listed as "could not be read", from its
    /// file name, and never restored.
    pub unreadable: Option<String>,
}

/// One saved document. `after_hash` (updates only) is `doc_hash` of the
/// document as rongo left it, when that was recorded.
#[derive(Clone, Debug, PartialEq)]
pub struct Saved {
    pub before: RawDocumentBuf,
    pub after_hash: Option<i64>,
}

/// 64-bit hash of the raw bytes of a document: the first 8 bytes
/// (little-endian) of their SHA-256. Kept on disk in update entries, so it
/// must never change (std's hasher may, between Rust releases).
pub fn doc_hash(raw: &RawDocumentBuf) -> i64 {
    let h = digest::digest(&digest::SHA256, raw.as_bytes());
    i64::from_le_bytes(h.as_ref()[..8].try_into().expect("a SHA-256 has 32 bytes"))
}

#[cfg(test)]
thread_local! {
    /// Records decrypted by this thread, to show what `list` avoids.
    static DECRYPTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Appends this thread made durable (`sync_data`).
    pub(crate) static SYNCS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Walks of a whole history tree by this thread.
    static WALKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

mod format;
mod reader;
mod retention;
mod store;
#[cfg(test)]
mod tests;
mod writer;

use format::*;
pub use reader::EntryReader;
use reader::*;
#[cfg(test)]
use retention::PURGE_PAUSE;
pub use store::Store;
use store::*;
pub use writer::EntryWriter;
