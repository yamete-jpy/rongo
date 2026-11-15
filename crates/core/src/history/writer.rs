//! Writing an entry: `EntryWriter`, from `Store::begin` to `finish`.

use super::*;

/// One operation being recorded. Dropping it without `finish` or `abandon`
/// leaves the file as it is: an entry cut short, which still reads.
pub struct EntryWriter {
    pub(super) path: PathBuf,
    pub(super) id: EntryId,
    pub(super) cipher: XChaCha20Poly1305,
    pub(super) file_id: [u8; NONCE_LEN],
    pub(super) kind: Kind,
    pub(super) seq: u64,
    pub(super) written: u64,
    pub(super) saved: u64,
}

impl EntryWriter {
    /// Appends sealed records in one write; `sync` makes them durable before
    /// it returns. Before-images are synced: the server changes documents once
    /// they are saved, so a power loss after that must not take them. Results
    /// and the end record are not: losing them leaves an entry that reads as
    /// incomplete or "changed since", and the next sync covers them anyway. A
    /// failed write or sync is cut back so the file never holds half a record
    /// before later ones.
    fn append(&mut self, plains: Vec<Vec<u8>>, sync: bool) -> CoreResult<()> {
        if plains.is_empty() {
            return Ok(());
        }
        let mut buf = Vec::new();
        for (i, p) in plains.iter().enumerate() {
            seal(&self.cipher, &self.file_id, self.seq + i as u64, p, &mut buf)?;
        }
        let mut f = OpenOptions::new().append(true).open(&self.path).map_err(io_err)?;
        if let Err(e) = f.write_all(&buf).and_then(|()| f.flush()).and_then(|()| if sync { f.sync_data() } else { Ok(()) }) {
            let _ = f.set_len(self.written);
            return Err(io_err(e));
        }
        #[cfg(test)]
        if sync {
            SYNCS.with(|c| c.set(c.get() + 1));
        }
        self.seq += plains.len() as u64;
        self.written += buf.len() as u64;
        Ok(())
    }

    /// The entry being written.
    pub fn id(&self) -> &EntryId {
        &self.id
    }

    /// Bytes the entry holds on disk so far.
    pub fn bytes(&self) -> u64 {
        self.written
    }

    /// Saves before-images. Written (and synced to disk) before the call returns.
    pub fn save<'a>(&mut self, before: impl IntoIterator<Item = &'a RawDocumentBuf>) -> CoreResult<()> {
        let mut marker = RawDocumentBuf::new();
        marker.append(cstr!("k"), "n");
        let mut plains = vec![marker.into_bytes()];
        plains.extend(before.into_iter().map(|d| {
            let mut r = RawDocumentBuf::new();
            r.append(cstr!("k"), "b");
            r.append(cstr!("d"), RawBsonRef::Document(d));
            r.into_bytes()
        }));
        let n = plains.len() as u64 - 1;
        if n == 0 {
            return Ok(());
        }
        self.append(plains, true)?;
        self.saved += n;
        Ok(())
    }

    /// Updates only: for each `_id`, the hash of the document as it was left.
    pub fn after(&mut self, hashes: &[(Bson, i64)]) -> CoreResult<()> {
        if self.kind != Kind::Update {
            return Err(history_err("only an update entry records results"));
        }
        let plains = hashes.iter().map(|(id, h)| bson::serialize_to_vec(&doc! { "k": "a", "id": id.clone(), "h": *h })).collect::<Result<Vec<_>, _>>()?;
        self.append(plains, false)
    }

    /// Writes the end record. An entry that saved nothing leaves no file
    /// (one held open a moment, by antivirus on Windows, is waited for).
    pub fn finish(mut self, cancelled: bool) -> CoreResult<EntryId> {
        if self.saved == 0 {
            let _ = fsx::remove_file(&self.path);
            return Ok(self.id.clone());
        }
        let end = bson::serialize_to_vec(&doc! { "k": "e", "count": self.saved as i64, "cancelled": cancelled })?;
        self.append(vec![end], false)?;
        Ok(self.id.clone())
    }

    /// Removes the file (waiting out a hold, as `finish`): the operation did not happen.
    pub fn abandon(self) {
        let _ = fsx::remove_file(&self.path);
    }
}

impl Drop for EntryWriter {
    /// No longer being written (finished, abandoned or cut short): from now
    /// on retention and removals may take it.
    fn drop(&mut self) {
        open_writers().remove(&self.path);
    }
}
