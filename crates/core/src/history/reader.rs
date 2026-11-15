//! Reading an entry back: its before-images (`EntryReader`), and what a file
//! holds (`scan`) for listing.

use super::*;

/// The before-images of one entry. For an update, each carries the hash of the
/// document as left; a batch marker ends the previous batch, so at most one
/// batch is held in memory to pair them.
pub struct EntryReader {
    pub(super) recs: Option<Records>,
    pub(super) kind: Kind,
    pub(super) out: VecDeque<Saved>,
    pub(super) befores: Vec<RawDocumentBuf>,
    pub(super) afters: HashMap<Vec<u8>, i64>,
    pub(super) done: bool,
    pub(super) err: Option<CoreError>,
}

impl EntryReader {
    /// The entry's kind; `None` when its header could not be read (a damaged
    /// file, or one sealed with another key), in which case it yields nothing.
    pub fn kind(&self) -> Option<Kind> {
        self.recs.is_some().then_some(self.kind)
    }

    fn flush(&mut self) {
        for before in self.befores.drain(..) {
            let after_hash = before.get("_id").ok().flatten().and_then(|v| self.afters.get(&pair_key(v)).copied());
            self.out.push_back(Saved { before, after_hash });
        }
        self.afters.clear();
    }
}

impl Iterator for EntryReader {
    type Item = CoreResult<Saved>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(s) = self.out.pop_front() {
                return Some(Ok(s));
            }
            if self.done {
                return self.err.take().map(Err);
            }
            match self.recs.as_mut().map_or(Ok(None), Records::next) {
                Err(e) => {
                    self.err = Some(e);
                    self.done = true;
                    self.flush();
                }
                Ok(None | Some(Record::End { .. } | Record::Header { .. })) => {
                    self.done = true;
                    self.flush();
                }
                Ok(Some(Record::Batch)) => self.flush(),
                Ok(Some(Record::Before(d))) => {
                    if self.kind == Kind::Delete {
                        self.out.push_back(Saved { before: d, after_hash: None });
                    } else {
                        self.befores.push(d);
                    }
                }
                Ok(Some(Record::After(k, h))) => {
                    if self.kind == Kind::Update {
                        self.afters.insert(k, h);
                    }
                }
            }
        }
    }
}

#[derive(Clone, Default)]
pub(super) struct Scan {
    pub(super) header: Option<(Kind, DateTime, String)>,
    pub(super) count: u64,
    pub(super) complete: bool,
    pub(super) cancelled: bool,
}

/// Reads a whole file (one record in memory at a time) to learn what it holds.
pub(super) fn scan(path: &Path, key: &HistoryKey) -> CoreResult<Scan> {
    let mut recs = Records::open(path, key)?;
    let mut s = Scan::default();
    match recs.next()? {
        Some(Record::Header { kind, at, conn }) => s.header = Some((kind, at, conn)),
        _ => return Ok(s),
    }
    while let Some(rec) = recs.next()? {
        match rec {
            Record::Before(_) => s.count += 1,
            Record::End { count, cancelled } => {
                s.complete = true;
                s.count = count;
                s.cancelled = cancelled;
                break;
            }
            Record::After(..) | Record::Batch => {}
            Record::Header { .. } => break,
        }
    }
    Ok(s)
}

/// What `scan` finds, without decrypting the middle of the file: walks the
/// length prefixes, then reads only the header and the last record. `None` when
/// the file does not end in a valid end record (or its structure is off), in
/// which case `scan` answers. The last record's position is authenticated, so
/// a record dropped from the middle is caught here; a flipped byte inside a
/// middle record is not (reading the entry will stop there).
pub(super) fn scan_fast(path: &Path, key: &HistoryKey) -> CoreResult<Option<Scan>> {
    let mut recs = Records::open(path, key)?;
    let Some(Record::Header { kind, at, conn }) = recs.next()? else { return Ok(None) };
    let mut last = None; // (offset of its length prefix, its sequence number)
    let mut seq = 1;
    while recs.left >= 4 {
        let start = recs.len - recs.left;
        let mut len = [0u8; 4];
        recs.r.read_exact(&mut len).map_err(io_err)?;
        let len = u64::from(u32::from_le_bytes(len));
        if len < (NONCE_LEN + TAG_LEN) as u64 || len > MAX_RECORD || len > recs.left - 4 {
            return Ok(None);
        }
        recs.r.seek_relative(len as i64).map_err(io_err)?;
        recs.left -= 4 + len;
        last = Some((start, seq));
        seq += 1;
    }
    let Some((start, last_seq)) = last.filter(|_| recs.left == 0) else { return Ok(None) };
    recs.r.seek(SeekFrom::Start(start)).map_err(io_err)?;
    recs.left = recs.len - start;
    recs.seq = last_seq;
    match recs.next()? {
        Some(Record::End { count, cancelled }) => Ok(Some(Scan { header: Some((kind, at, conn)), count, complete: true, cancelled })),
        _ => Ok(None),
    }
}
