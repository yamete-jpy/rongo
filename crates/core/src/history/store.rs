//! The store: where entries live, beginning, listing, reading and removing
//! them, moving a collection's history.

use super::*;

#[derive(Clone, Debug)]
pub struct Store {
    pub(super) root: PathBuf,
    pub(super) key: HistoryKey,
}

/// What listing read from entry files, by path, with the size and
/// modification time the file had then and a tag of the key that read it.
/// An entry file only grows while it is written (or is cut back to what it
/// was) and a finished one never changes, so while those match it is not
/// read again. A file that could not be read is not kept.
static HEADERS: LazyLock<Mutex<HashMap<PathBuf, Cached>>> = LazyLock::new(Default::default);

/// Files `HEADERS` keeps at most; past that it starts again.
const HEADERS_MAX: usize = 20_000;

struct Cached {
    len: u64,
    modified: std::time::SystemTime,
    key: [u8; 16],
    scan: Scan,
}

fn headers() -> MutexGuard<'static, HashMap<PathBuf, Cached>> {
    HEADERS.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(super) fn read_dir_opt(p: &Path) -> CoreResult<Vec<fs::DirEntry>> {
    match fs::read_dir(p) {
        Ok(rd) => rd.collect::<Result<_, _>>().map_err(io_err),
        Err(e) if is_not_found(&e) => Ok(Vec::new()),
        Err(e) => Err(io_err(e)),
    }
}

/// Real subdirectories (not links) of `p`.
fn subdirs(p: &Path) -> CoreResult<Vec<PathBuf>> {
    Ok(read_dir_opt(p)?.into_iter().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()).collect())
}

pub(super) fn remove_file_opt(p: &Path) -> CoreResult<()> {
    match fs::remove_file(p) {
        Ok(()) => Ok(()),
        Err(e) if is_not_found(&e) => Ok(()),
        Err(e) => Err(io_err(e)),
    }
}

/// Removes what is in `dir` but the files in `keep`, and `dir` itself when
/// nothing is left in it; a folder's name file stays while the folder does.
fn remove_all_but(dir: &Path, keep: &HashSet<PathBuf>) -> CoreResult<()> {
    for e in read_dir_opt(dir)? {
        let p = e.path();
        if e.file_type().map_err(io_err)?.is_dir() {
            remove_all_but(&p, keep)?;
        } else if !keep.contains(&p) && e.file_name() != NAME_FILE {
            remove_file_opt(&p)?;
        }
    }
    let name = dir.join(NAME_FILE);
    if fs::read_dir(dir).is_ok_and(|rd| rd.count() == usize::from(name.is_file())) {
        remove_file_opt(&name)?;
        let _ = fs::remove_dir(dir);
    }
    Ok(())
}

/// Creates the directories (private); once more if a concurrent prune removed one meanwhile.
fn create_dirs(p: &Path) -> CoreResult<()> {
    let make = || fsx::create_private_dir(p);
    match make() {
        Err(e) if is_not_found(&e) => make(),
        r => r,
    }
    .map_err(io_err)
}

impl Store {
    pub fn new(root: PathBuf, key: HistoryKey) -> Store {
        Store { root, key }
    }

    pub(super) fn ns_dir(&self, profile: Uuid, ns: &Namespace) -> CoreResult<PathBuf> {
        Ok(self.root.join(profile.to_string()).join(encode_name(&ns.db)?.0).join(encode_name(&ns.coll)?.0))
    }

    /// Keeps the full names of shortened directories in step with `ns`: written
    /// where the name is shortened, removed where a moved directory left a stale one.
    fn sync_name_files(&self, profile: Uuid, ns: &Namespace) -> CoreResult<()> {
        let db_dir = self.root.join(profile.to_string()).join(encode_name(&ns.db)?.0);
        for (dir, name) in [(db_dir.clone(), &ns.db), (db_dir.join(encode_name(&ns.coll)?.0), &ns.coll)] {
            let file = dir.join(NAME_FILE);
            if encode_name(name)?.1 {
                if fs::read(&file).ok().as_deref() != Some(name.as_bytes()) {
                    let mut f = fsx::private_options().write(true).create(true).truncate(true).open(&file).map_err(io_err)?;
                    f.write_all(name.as_bytes()).map_err(io_err)?;
                }
            } else {
                remove_file_opt(&file)?;
            }
        }
        Ok(())
    }

    /// Only a file name this module writes gets a path: nothing can climb out
    /// of the collection's directory.
    pub(super) fn entry_path(&self, id: &EntryId) -> CoreResult<PathBuf> {
        if parse_file_name(&id.file).is_none() {
            return Err(history_err("invalid entry name"));
        }
        Ok(self.ns_dir(id.profile, &id.ns)?.join(&id.file))
    }

    /// Removes the directories a removal left empty, never the root.
    fn prune(&self, mut dir: PathBuf) {
        while dir != self.root && dir.starts_with(&self.root) {
            // A shortened name's file does not keep its directory alive.
            let name_file = dir.join(NAME_FILE);
            if name_file.is_file() && fs::read_dir(&dir).is_ok_and(|rd| rd.count() == 1) {
                let _ = fs::remove_file(&name_file);
            }
            // Already gone (a renamed directory): its parents may be empty now.
            if fs::remove_dir(&dir).is_err_and(|e| !is_not_found(&e)) {
                break;
            }
            dir.pop();
        }
    }

    /// Starts recording an operation: the file exists (header only) from now on.
    pub fn begin(&self, profile: Uuid, conn_name: &str, ns: &Namespace, kind: Kind, how: Document) -> CoreResult<EntryWriter> {
        self.begin_at(profile, conn_name, ns, kind, how, DateTime::now())
    }

    pub(super) fn begin_at(&self, profile: Uuid, conn_name: &str, ns: &Namespace, kind: Kind, how: Document, at: DateTime) -> CoreResult<EntryWriter> {
        let dir = self.ns_dir(profile, ns)?;
        let cipher = self.key.cipher();
        let mut buf = MAGIC.to_vec();
        let header = doc! { "k": "h", "kind": kind.name(), "at": at, "conn": conn_name, "how": how, "db": &ns.db, "coll": &ns.coll, "v": env!("CARGO_PKG_VERSION") };
        let file_id = seal(&cipher, &[0; NONCE_LEN], 0, &bson::serialize_to_vec(&header)?, &mut buf)?;
        create_dirs(&dir)?;
        self.sync_name_files(profile, ns)?;
        let (mut n, mut retried) = (0u32, false);
        loop {
            let file = file_name(at.timestamp_millis(), kind, n);
            let path = dir.join(&file);
            // Open before the file exists, so no removal finds it unprotected.
            let fresh = open_writers().insert(path.clone());
            let made = if fresh { fsx::private_options().write(true).create_new(true).open(&path) } else { Err(std::io::ErrorKind::AlreadyExists.into()) };
            let mut f = match made {
                Ok(f) => f,
                Err(e) => {
                    if fresh {
                        open_writers().remove(&path);
                    }
                    match e {
                        e if e.kind() == std::io::ErrorKind::AlreadyExists && n < 10_000 => {
                            n += 1;
                            continue;
                        }
                        // A purge removed the emptied directory just now: make it again, once.
                        e if is_not_found(&e) && !retried => {
                            retried = true;
                            create_dirs(&dir)?;
                            self.sync_name_files(profile, ns)?;
                            continue;
                        }
                        e => return Err(io_err(e)),
                    }
                }
            };
            let written = f.write_all(&buf).and_then(|()| f.flush()).map_err(io_err);
            drop(f);
            // Named again now the file exists, so a prune that began earlier cannot leave the directory unnamed.
            if let Err(e) = written.and_then(|()| self.sync_name_files(profile, ns)) {
                let _ = fs::remove_file(&path);
                open_writers().remove(&path);
                return Err(e);
            }
            let id = EntryId { profile, ns: ns.clone(), file };
            return Ok(EntryWriter { path, id, cipher, file_id, kind, seq: 1, written: buf.len() as u64, saved: 0 });
        }
    }

    /// The entries of one collection, newest first. A finished entry is read by
    /// its header and last record only, and an entry already read is not read
    /// again while its file stays the same. An entry whose header can't
    /// be decrypted is listed from its file name (empty connection name, count 0).
    pub fn list(&self, profile: Uuid, ns: &Namespace) -> CoreResult<Vec<EntryHeader>> {
        let dir = self.ns_dir(profile, ns)?;
        let tag = self.key.tag();
        let mut found = Vec::new();
        for e in read_dir_opt(&dir)? {
            let file = e.file_name().to_string_lossy().into_owned();
            let Some(name) = parse_file_name(&file) else { continue };
            let meta = match e.metadata() {
                Err(e) if is_not_found(&e) => continue,
                m => m.map_err(io_err),
            };
            let header = self.header_at(EntryId { profile, ns: ns.clone(), file: file.clone() }, &name, &e.path(), meta, &tag);
            found.push(((name.ms, name.n, file), header));
        }
        // What was read of files no longer here is let go.
        let here: HashSet<PathBuf> = found.iter().map(|(k, _)| dir.join(&k.2)).collect();
        headers().retain(|p, _| p.parent() != Some(dir.as_path()) || here.contains(p));
        found.sort_by(|a, b| b.0.cmp(&a.0));
        Ok(found.into_iter().map(|(_, h)| h).collect())
    }

    /// One entry, read as `list` reads it (without the others).
    pub fn header(&self, id: &EntryId) -> CoreResult<EntryHeader> {
        let path = self.entry_path(id)?;
        let name = parse_file_name(&id.file).ok_or_else(|| history_err("invalid entry name"))?;
        let meta = match fs::metadata(&path) {
            Err(e) if is_not_found(&e) => return Err(history_err("that entry no longer exists")),
            m => m.map_err(io_err),
        };
        Ok(self.header_at(id.clone(), &name, &path, meta, &self.key.tag()))
    }

    /// What an entry file holds: a finished one is read by its header and
    /// last record only (or not at all, when `HEADERS` has it as it is); one
    /// whose header can't be decrypted is described from its file name (empty
    /// connection name, count 0), and so is one that could not be read just
    /// now, with why.
    fn header_at(&self, id: EntryId, name: &FileName, path: &Path, meta: CoreResult<fs::Metadata>, tag: &[u8; 16]) -> EntryHeader {
        let (bytes, modified) = match &meta {
            Ok(m) => (m.len(), m.modified().ok()),
            Err(_) => (0, None),
        };
        let cached = modified.and_then(|m| headers().get(path).filter(|c| (c.len, c.modified, &c.key) == (bytes, m, tag)).map(|c| c.scan.clone()));
        let read = match (meta, cached) {
            (Err(e), _) => Err(e),
            (Ok(_), Some(s)) => Ok(s),
            (Ok(_), None) => {
                let read = match scan_fast(path, &self.key) {
                    Ok(Some(s)) => Ok(s),
                    Ok(None) => scan(path, &self.key),
                    Err(e) => Err(e),
                };
                if let (Ok(s), Some(modified)) = (&read, modified) {
                    let mut cache = headers();
                    if cache.len() >= HEADERS_MAX {
                        cache.clear();
                    }
                    cache.insert(path.to_path_buf(), Cached { len: bytes, modified, key: *tag, scan: s.clone() });
                }
                read
            }
        };
        let (s, unreadable) = match read {
            Ok(s) => (s, None),
            Err(e) => (Scan::default(), Some(reason(e))),
        };
        let (kind, at, conn_name) = s.header.unwrap_or((name.kind, DateTime::from_millis(name.ms), String::new()));
        let restored = restored_path(path).exists();
        EntryHeader { id, kind, at, conn_name, count: s.count, complete: s.complete, cancelled: s.cancelled, restored, bytes, unreadable }
    }

    /// The saved documents of an entry, in the order they were saved. Reading
    /// stops quietly at a damaged record; a different key reads nothing.
    pub fn read(&self, id: &EntryId) -> CoreResult<EntryReader> {
        let mut recs = Records::open(&self.entry_path(id)?, &self.key)?;
        let (recs, kind, done) = match recs.next()? {
            Some(Record::Header { kind, .. }) => (Some(recs), kind, false),
            _ => (None, Kind::Delete, true),
        };
        Ok(EntryReader { recs, kind, out: VecDeque::new(), befores: Vec::new(), afters: HashMap::new(), done, err: None })
    }

    pub fn mark_restored(&self, id: &EntryId) -> CoreResult<()> {
        let path = self.entry_path(id)?;
        if !path.is_file() {
            return Err(history_err("that entry no longer exists"));
        }
        fsx::private_options().write(true).create(true).truncate(true).open(restored_path(&path)).map_err(io_err)?;
        Ok(())
    }

    /// Removes an entry file and its marker, unless the entry is being
    /// written (false).
    fn unlink_entry(&self, path: &Path) -> CoreResult<bool> {
        let open = open_writers();
        if open.contains(path) {
            return Ok(false);
        }
        remove_file_opt(path)?;
        headers().remove(path);
        remove_file_opt(&restored_path(path))?;
        Ok(true)
    }

    /// Removes an entry file, its marker, and the directories that leaves
    /// empty; not an entry being written (false).
    pub(super) fn remove_entry_files(&self, path: &Path) -> CoreResult<bool> {
        if !self.unlink_entry(path)? {
            return Ok(false);
        }
        if let Some(dir) = path.parent() {
            self.prune(dir.to_path_buf());
        }
        Ok(true)
    }

    /// Removes one entry; refused while it is being written.
    pub fn remove(&self, id: &EntryId) -> CoreResult<()> {
        if !self.remove_entry_files(&self.entry_path(id)?)? {
            return Err(history_err("that entry is still being written"));
        }
        Ok(())
    }

    /// Removes every entry of one collection begun at or before
    /// `before_ms` (the time in its file name; the app passes when the user
    /// asked, so an entry recorded while the removal waited stays) but
    /// those being written, which are kept (and the answer says so).
    pub fn empty(&self, profile: Uuid, ns: &Namespace, before_ms: i64) -> CoreResult<()> {
        let dir = self.ns_dir(profile, ns)?;
        let mut left = 0;
        let due = |file: &str| parse_file_name(file).map(|f| f.ms <= before_ms);
        for e in read_dir_opt(&dir)? {
            let file = e.file_name().to_string_lossy().into_owned();
            match due(&file) {
                Some(true) => left += usize::from(!self.unlink_entry(&e.path())?),
                Some(false) => {}
                // A restored mark goes with its entry (or alone, its entry gone).
                None if strip_restored(&file).and_then(due) == Some(true) => remove_file_opt(&e.path())?,
                None => {}
            }
        }
        self.prune(dir);
        kept(left)
    }

    /// Removes `dir` and everything in it but the entries being written
    /// (and the folders, with their name files, that hold them); answers
    /// how many were kept. Holds the open set meanwhile, so no writer can
    /// begin inside.
    fn remove_tree(&self, dir: &Path) -> CoreResult<usize> {
        let open = open_writers();
        headers().retain(|p, _| !p.starts_with(dir) || open.contains(p));
        let left = open.iter().filter(|p| p.starts_with(dir)).count();
        if left == 0 {
            return match fs::remove_dir_all(dir) {
                Err(e) if !is_not_found(&e) => Err(io_err(e)),
                _ => Ok(0),
            };
        }
        remove_all_but(dir, &open)?;
        Ok(left)
    }

    /// The directories of the root named by a profile id; anything else
    /// there is not history.
    fn profile_dirs(&self) -> CoreResult<Vec<(Uuid, PathBuf)>> {
        Ok(subdirs(&self.root)?.into_iter().filter_map(|p| Some((p.file_name()?.to_str().and_then(|s| Uuid::parse_str(s).ok())?, p))).collect())
    }

    /// Removes every profile's history (Settings → Clear All History) but
    /// the entries being written, which are kept (and the answer says so).
    /// Files and folders in the root that aren't history stay.
    pub fn clear(&self) -> CoreResult<()> {
        let mut left = 0;
        for (_, dir) in self.profile_dirs()? {
            left += self.remove_tree(&dir)?;
        }
        kept(left)
    }

    /// Keeps a collection's history with it when the collection is renamed.
    /// Refused while one of its entries is being written: the file would
    /// move from under its writer.
    pub fn move_collection(&self, profile: Uuid, from: &Namespace, to: &Namespace) -> CoreResult<()> {
        let (src, dst) = (self.ns_dir(profile, from)?, self.ns_dir(profile, to)?);
        if src == dst || !src.is_dir() {
            return Ok(());
        }
        // Held while moving, so no writer begins in either folder meanwhile.
        let open = open_writers();
        if open.iter().any(|p| p.starts_with(&src)) {
            return Err(history_err(format!("an entry of {from} is still being written, so its history was not moved")));
        }
        // What listing read is kept by path: the files are about to have others.
        headers().retain(|p, _| !p.starts_with(&src));
        if !dst.exists() {
            if let Some(parent) = dst.parent() {
                create_dirs(parent)?;
            }
            fsx::rename(&src, &dst).map_err(io_err)?;
        } else {
            let listed = read_dir_opt(&src)?;
            for e in &listed {
                // Markers and strays are not entries; markers travel with theirs.
                let Some(name) = e.file_name().to_str().and_then(parse_file_name) else { continue };
                // Entries already there keep their names; ours take the next free one.
                let free = |t: &String| !dst.join(t).exists() && !restored_path(&dst.join(t)).exists() && !open.contains(&dst.join(t));
                let target = (name.n..).map(|n| file_name(name.ms, name.kind, n)).find(free).expect("a free name");
                let marker = restored_path(&e.path());
                fsx::rename(&e.path(), &dst.join(&target)).map_err(io_err)?;
                if marker.exists() {
                    fsx::rename(&marker, &restored_path(&dst.join(&target))).map_err(io_err)?;
                }
            }
            // Markers whose entry is gone.
            for e in &listed {
                if e.file_name().to_str().and_then(strip_restored).is_some_and(|f| parse_file_name(f).is_some()) {
                    remove_file_opt(&e.path())?;
                }
            }
        }
        self.sync_name_files(profile, to)?;
        drop(open);
        self.prune(src);
        Ok(())
    }

    /// Removes the history of a deleted connection profile but the entries
    /// being written, which are kept (and the answer says so).
    pub fn delete_profile(&self, profile: Uuid) -> CoreResult<()> {
        kept(self.remove_tree(&self.root.join(profile.to_string()))?)
    }

    /// Free space on the volume the history is (or would be) kept on.
    pub fn free_bytes(&self) -> CoreResult<u64> {
        fsx::free_space(&self.root).map_err(io_err)
    }
}
