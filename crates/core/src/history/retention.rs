//! Retention and measuring: walking the whole tree by file names and sizes,
//! purging by age and size (with a running total, so not every operation
//! walks), and Reset's removal by time.

use super::*;

/// Retention's running count, per history root (see `Store::trim`).
static TOTALS: LazyLock<Mutex<HashMap<PathBuf, Tally>>> = LazyLock::new(Default::default);

/// Retention's count for one history root. `TOTALS` is held only to read
/// and change it, never while a purge reads or removes files.
#[derive(Default)]
struct Tally {
    /// Bytes the entries take: as the last purge left them, plus each entry
    /// recorded (and trimmed for) in this process since.
    bytes: Option<u64>,
    /// When the last full purge (`Store::purge`) began.
    full_at: Option<std::time::Instant>,
    /// Purges under way. Meanwhile a trim only counts its entry and leaves
    /// the trimming to them.
    purging: usize,
    /// What trims counted while a purge was under way, which that purge's
    /// own count may leave out (counted twice at worst, the safe side).
    added: u64,
}

fn totals() -> MutexGuard<'static, HashMap<PathBuf, Tally>> {
    TOTALS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A purge under way for one root: it settles the root's `Tally` when it
/// ends, also if it ends early.
struct Purging<'a> {
    root: &'a Path,
    /// Also the age limit's hourly work: leftovers, found by reading.
    full: bool,
    /// The bytes the entries take as the purge left them, once it knows.
    left: Option<u64>,
}

impl<'a> Purging<'a> {
    fn begin(root: &'a Path, full: bool) -> Self {
        Self::claim(root, full, totals().entry(root.to_path_buf()).or_default())
    }

    /// Marks a purge under way in `tally` (its root's, under `TOTALS`).
    fn claim(root: &'a Path, full: bool, tally: &mut Tally) -> Self {
        tally.purging += 1;
        if full {
            tally.full_at = Some(std::time::Instant::now());
        }
        Purging { root, full, left: None }
    }
}

impl Drop for Purging<'_> {
    fn drop(&mut self) {
        let mut totals = totals();
        let tally = totals.entry(self.root.to_path_buf()).or_default();
        if let Some(left) = self.left {
            tally.bytes = Some(left.saturating_add(tally.added));
        }
        tally.purging = tally.purging.saturating_sub(1);
        if tally.purging == 0 {
            tally.added = 0;
        }
    }
}

/// After an operation, entries past their age (and leftovers) are looked for
/// at most this often; the size limit is watched by the running total.
const FULL_EVERY: std::time::Duration = std::time::Duration::from_secs(60 * 60);

/// An entry file holding only its header and untouched for this long, with no
/// writer in this process, is a crash's leftover.
const STALE: std::time::Duration = std::time::Duration::from_secs(60);

/// Tests: called (once) by the next purge of a root, after its walk, while
/// it works on the files.
#[cfg(test)]
pub(crate) static PURGE_PAUSE: LazyLock<Mutex<HashMap<PathBuf, Pause>>> = LazyLock::new(Default::default);

#[cfg(test)]
pub(crate) type Pause = Box<dyn FnOnce() + Send>;

struct WalkedFile {
    profile: Uuid,
    path: PathBuf,
    at_ms: i64,
    n: u32,
    bytes: u64,
    modified: Option<std::time::SystemTime>,
}

/// What `Store::walk` found.
#[derive(Default)]
struct Walked {
    entries: Vec<WalkedFile>,
    /// Restored marks with no entry file beside them.
    orphan_marks: Vec<PathBuf>,
    /// What could not be read, and why; the rest was walked all the same.
    problems: Vec<String>,
}

impl Walked {
    /// The entries, or the first thing that could not be read.
    fn complete(self) -> CoreResult<Vec<WalkedFile>> {
        match self.problems.into_iter().next() {
            Some(p) => Err(history_err(p)),
            None => Ok(self.entries),
        }
    }
}

/// One error naming what could not be read or removed (the first, and how many more).
fn problems_err(problems: &[String]) -> Option<CoreError> {
    let (first, more) = problems.split_first()?;
    Some(history_err(match more.len() {
        0 => first.clone(),
        n => format!("{first} (and {n} more)"),
    }))
}

/// Whether a walked entry file holds nothing past its header (a crash
/// between beginning an entry and saving its first batch, or in the middle
/// of beginning it: an empty file, the magic line only, a header cut short)
/// and was last changed over `STALE` ago. Read without the key: what there
/// is of the magic line, then the header's length. A file that does not
/// start as history does is not one.
fn header_only(f: &WalkedFile) -> bool {
    /// A header is far smaller; larger files are not opened.
    const HEADER_MAX: u64 = 1 << 20;
    if f.bytes > HEADER_MAX || !f.modified.is_some_and(|m| m.elapsed().is_ok_and(|age| age >= STALE)) {
        return false;
    }
    let mut head = Vec::with_capacity(MAGIC.len() + 4);
    let Ok(_) = File::open(&f.path).and_then(|file| file.take((MAGIC.len() + 4) as u64).read_to_end(&mut head)) else { return false };
    let magic = head.len().min(MAGIC.len());
    if head[..magic] != MAGIC[..magic] {
        return false;
    }
    // Cut off in the magic line or the header's length: nothing after it.
    let Some(len) = head.get(MAGIC.len()..).and_then(|l| <[u8; 4]>::try_from(l).ok()) else { return true };
    // The header ends the file, or is cut short.
    (MAGIC.len() + 4) as u64 + u64::from(u32::from_le_bytes(len)) >= f.bytes
}

impl Store {
    /// Every entry file under `<root>/<profile id>/*/*/`, found by path alone:
    /// purging and counting must not depend on a directory's name being
    /// readable. What can't be read is skipped and named, so retention goes on
    /// with the rest.
    fn walk(&self) -> Walked {
        #[cfg(test)]
        WALKS.with(|c| c.set(c.get() + 1));
        let mut w = Walked::default();
        let listed = |dir: &Path, w: &mut Walked| match read_dir_opt(dir) {
            Ok(entries) => entries,
            Err(e) => {
                w.problems.push(format!("could not read {}: {}", dir.display(), reason(e)));
                Vec::new()
            }
        };
        let dirs = |entries: Vec<fs::DirEntry>| entries.into_iter().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()).collect::<Vec<_>>();
        let profiles: Vec<(Uuid, PathBuf)> = dirs(listed(&self.root, &mut w)).into_iter().filter_map(|p| Some((p.file_name()?.to_str().and_then(|s| Uuid::parse_str(s).ok())?, p))).collect();
        for (profile, prof) in profiles {
            for db in dirs(listed(&prof, &mut w)) {
                for coll in dirs(listed(&db, &mut w)) {
                    let mut names = HashSet::new();
                    let mut marks = Vec::new();
                    for f in listed(&coll, &mut w) {
                        let Some(file) = f.file_name().to_str().map(str::to_string) else { continue };
                        let Some(name) = parse_file_name(&file) else {
                            if strip_restored(&file).is_some_and(|e| parse_file_name(e).is_some()) {
                                marks.push(f.path());
                            }
                            continue;
                        };
                        match f.metadata() {
                            Ok(m) => w.entries.push(WalkedFile { profile, path: f.path(), at_ms: name.ms, n: name.n, bytes: m.len(), modified: m.modified().ok() }),
                            Err(e) if is_not_found(&e) => {}
                            Err(e) => w.problems.push(format!("could not read {}: {e}", f.path().display())),
                        }
                        names.insert(file);
                    }
                    w.orphan_marks.extend(marks.into_iter().filter(|m| !m.file_name().and_then(|n| n.to_str()).and_then(strip_restored).is_some_and(|e| names.contains(e))));
                }
            }
        }
        w
    }

    /// Removes entries older than `keep_days`, then the oldest until the total
    /// is within `max_bytes`; never one in `protect`, nor one being written
    /// (another operation's). Also removes what crashes and races leave: a
    /// restored mark whose entry is gone, and an entry file holding only its
    /// header, untouched for a minute. Returns the bytes freed. What can't be
    /// read or removed is left and the rest done all the same; the error then
    /// names it.
    pub fn purge(&self, keep_days: u32, max_bytes: u64, protect: &[EntryId]) -> CoreResult<u64> {
        self.purge_counted(keep_days, max_bytes, protect, Purging::begin(&self.root, true))
    }

    /// Retention after an operation that recorded `new` (never removed, nor
    /// anything in `protect`). It adds `new`'s size to a running total of
    /// what the history takes and purges only when that passes `max_bytes`,
    /// or fully (age limit, leftovers) when the last full purge was an hour
    /// ago or more, or not yet in this process. While another purge of this
    /// history is under way it only counts. Returns the bytes freed.
    pub fn trim(&self, keep_days: u32, max_bytes: u64, new: &EntryId, protect: &[EntryId]) -> CoreResult<u64> {
        let added = self.entry_path(new).ok().and_then(|p| fs::metadata(p).ok()).map_or(0, |m| m.len());
        let purging = {
            let mut totals = totals();
            let tally = totals.entry(self.root.clone()).or_default();
            if tally.purging > 0 {
                tally.added = tally.added.saturating_add(added);
                if let Some(bytes) = &mut tally.bytes {
                    *bytes = bytes.saturating_add(added);
                }
                return Ok(0);
            }
            let full = tally.full_at.is_none_or(|at| at.elapsed() >= FULL_EVERY);
            if !full && let Some(bytes) = tally.bytes {
                let total = bytes.saturating_add(added);
                tally.bytes = Some(total);
                if total <= max_bytes {
                    return Ok(0);
                }
            }
            // Claimed while still locked, so a trim meanwhile only counts.
            Purging::claim(&self.root, full, tally)
        };
        let mut keep = protect.to_vec();
        keep.push(new.clone());
        self.purge_counted(keep_days, max_bytes, &keep, purging)
    }

    /// `purge`, settling the count through `purging` as it ends. Only a full
    /// one opens small entry files to find those holding only a header.
    fn purge_counted(&self, keep_days: u32, max_bytes: u64, protect: &[EntryId], mut purging: Purging<'_>) -> CoreResult<u64> {
        let full = purging.full;
        let protect: HashSet<PathBuf> = protect.iter().filter_map(|id| self.entry_path(id).ok()).collect();
        let cutoff = DateTime::now().timestamp_millis() - i64::from(keep_days) * DAY_MS;
        let Walked { entries, orphan_marks, mut problems } = self.walk();
        #[cfg(test)]
        if let Some(pause) = PURGE_PAUSE.lock().unwrap_or_else(PoisonError::into_inner).remove(&self.root) {
            pause();
        }
        let remove = |path: &Path, problems: &mut Vec<String>| match self.remove_entry_files(path) {
            Ok(removed) => removed,
            Err(e) => {
                problems.push(format!("could not remove {}: {}", path.display(), reason(e)));
                false
            }
        };
        for mark in orphan_marks {
            if let Err(e) = self.remove_orphan_mark(&mark) {
                problems.push(format!("could not remove {}: {}", mark.display(), reason(e)));
            }
        }
        let mut freed = 0;
        let mut left = Vec::new();
        for f in entries {
            let doomed = !protect.contains(&f.path) && (f.at_ms < cutoff || (full && header_only(&f)));
            if doomed && remove(&f.path, &mut problems) {
                freed += f.bytes;
            } else {
                left.push(f);
            }
        }
        let mut total: u64 = left.iter().map(|f| f.bytes).sum();
        left.sort_by(|a, b| (a.at_ms, a.n, &a.path).cmp(&(b.at_ms, b.n, &b.path)));
        for f in left {
            if total <= max_bytes {
                break;
            }
            if protect.contains(&f.path) || !remove(&f.path, &mut problems) {
                continue;
            }
            total -= f.bytes;
            freed += f.bytes;
        }
        purging.left = Some(total);
        drop(purging);
        match problems_err(&problems) {
            Some(e) => Err(e),
            None => Ok(freed),
        }
    }

    /// Removes a restored mark whose entry file is gone; one whose entry is
    /// there (again) stays. Under the open set's lock, as a rename moves an
    /// entry and its mark under it.
    fn remove_orphan_mark(&self, mark: &Path) -> CoreResult<()> {
        let _open = open_writers();
        if mark.with_extension("").exists() {
            return Ok(());
        }
        remove_file_opt(mark)
    }

    /// Tests: the running total of this store's root, if one is kept.
    #[cfg(test)]
    pub(crate) fn running_total(&self) -> Option<u64> {
        totals().get(&self.root).and_then(|t| t.bytes)
    }

    /// Tests: as if the last full purge of this store's root ran `d` ago.
    #[cfg(test)]
    pub(crate) fn aged_ago(&self, d: std::time::Duration) {
        totals().entry(self.root.clone()).or_default().full_at = std::time::Instant::now().checked_sub(d);
    }

    /// Bytes the history takes on disk.
    pub fn usage(&self) -> CoreResult<u64> {
        Ok(self.walk().complete()?.iter().map(|f| f.bytes).sum())
    }

    /// Bytes each profile's history takes on disk, counted as `usage` counts
    /// them; a profile without entries is left out.
    pub fn usage_by_profile(&self) -> CoreResult<BTreeMap<Uuid, u64>> {
        let mut out = BTreeMap::new();
        for f in self.walk().complete()? {
            *out.entry(f.profile).or_default() += f.bytes;
        }
        Ok(out)
    }

    /// Removes every entry begun at or before `cutoff_ms` (the time in its
    /// file name), whatever key sealed it, with its restored mark and the
    /// folders that leaves empty; later entries stay. Reset's deletion: it
    /// may run only after writes running at the Reset are done, by when
    /// entries recorded under the new master password must not go with it.
    /// Entries being written are kept (and the answer says so); so is what
    /// can't be read or removed, which the answer names, the rest removed.
    pub fn clear_before(&self, cutoff_ms: i64) -> CoreResult<()> {
        let Walked { entries, mut problems, .. } = self.walk();
        let mut left = 0;
        for f in entries.iter().filter(|f| f.at_ms <= cutoff_ms) {
            match self.remove_entry_files(&f.path) {
                Ok(removed) => left += usize::from(!removed),
                Err(e) => problems.push(format!("could not remove {}: {}", f.path.display(), reason(e))),
            }
        }
        match problems_err(&problems) {
            Some(e) => Err(e),
            None => kept(left),
        }
    }

    /// Every collection with a history: (profile, namespace, entries, bytes).
    /// A directory whose name can't be read back (a shortened name without its
    /// name file) is left out here; it is still counted by `usage` and purged.
    pub fn all(&self) -> CoreResult<Vec<(Uuid, Namespace, u64, u64)>> {
        let mut by_dir: BTreeMap<PathBuf, (u64, u64)> = BTreeMap::new();
        for f in self.walk().complete()? {
            let e = by_dir.entry(f.path.parent().expect("an entry file has a directory").to_path_buf()).or_default();
            e.0 += 1;
            e.1 += f.bytes;
        }
        let mut out = Vec::new();
        for (coll_dir, (entries, bytes)) in by_dir {
            let db_dir = coll_dir.parent().expect("a collection directory has a parent");
            let profile = db_dir.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()).and_then(|s| Uuid::parse_str(s).ok());
            if let (Some(profile), Some(db), Some(coll)) = (profile, dir_name(db_dir), dir_name(&coll_dir)) {
                out.push((profile, Namespace::new(db, coll), entries, bytes));
            }
        }
        out.sort_by(|a, b| (a.0, &a.1.db, &a.1.coll).cmp(&(b.0, &b.1.db, &b.1.coll)));
        Ok(out)
    }
}
