//! Helpers the core's tests share: a temporary folder, a Collection History
//! record in it, and the demo server's commonest set-up and read-backs.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use bson::{Document, RawDocumentBuf};
use uuid::Uuid;

use crate::api::{Namespace, Record};
use crate::conn::Conn;
use crate::error::CoreResult;
use crate::history::{EntryHeader, EntryId, HistoryKey, Saved, Store};
use crate::mock::MockStore;

/// A unique directory under the system temp dir, removed on drop.
pub(crate) struct TempDir(pub(crate) PathBuf);

impl TempDir {
    /// Not made yet: the test (or the code under test) makes it.
    pub(crate) fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        TempDir(std::env::temp_dir().join(format!("rongo-core-test-{}-{}-{nanos}", std::process::id(), N.fetch_add(1, Ordering::SeqCst))))
    }

    /// Made, empty.
    pub(crate) fn made() -> Self {
        let t = TempDir::new();
        fs::create_dir_all(&t.0).unwrap();
        t
    }

    /// Where the tests keep Collection History: `history` inside.
    pub(crate) fn root(&self) -> PathBuf {
        self.0.join("history")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Collection History in `t`, for the connection "local", with a fixed key.
pub(crate) fn record(t: &TempDir) -> Record {
    Record { root: t.root(), key: HistoryKey::new([7; 32]), profile: Uuid::from_u128(42), conn_name: "local".into(), keep_days: 30, max_bytes: 1 << 30 }
}

pub(crate) fn store(r: &Record) -> Store {
    r.store()
}

/// The entries of `ns()`.
pub(crate) fn entries(r: &Record) -> Vec<EntryHeader> {
    store(r).list(r.profile, &ns()).unwrap()
}

pub(crate) fn saved(r: &Record, id: &EntryId) -> Vec<Saved> {
    store(r).read(id).unwrap().collect::<CoreResult<Vec<_>>>().unwrap()
}

/// Every file under `dir` (none when it does not exist), sorted.
pub(crate) fn files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir).into_iter().flatten() {
        let p = e.unwrap().path();
        if p.is_dir() { out.extend(files(&p)) } else { out.push(p) }
    }
    out.sort();
    out
}

pub(crate) fn ns() -> Namespace {
    Namespace::new("shop", "orders")
}

/// An empty demo server.
pub(crate) fn mock() -> Conn {
    Conn::mock(MockStore::empty(), false)
}

pub(crate) fn raw(d: &Document) -> RawDocumentBuf {
    RawDocumentBuf::try_from(d).unwrap()
}

/// `docs` into `ns()`.
pub(crate) async fn seed(c: &Conn, docs: &[Document]) {
    c.insert_many(&ns(), docs.to_vec()).await.unwrap();
}
