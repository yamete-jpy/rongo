//! The file format: entry folder and file names, and the sealed records a
//! file is made of.

use super::*;

// Names

// Names

/// Every byte outside `[a-z0-9_-]` is written as `%XX`, so a name can't climb
/// out of its directory and stays reversible. Capitals are encoded too: the
/// default macOS volume ignores case, and `Users` and `users` are different
/// collections that must not share a history.
///
/// An encoded name longer than `LONG_NAME` (MongoDB allows 255-byte
/// namespaces, and a non-ASCII byte takes three characters) becomes its encoded
/// start, `~` and 128 bits of the SHA-256 of the whole name, so distinct long names stay
/// distinct; the flag says the full name must be kept in a `NAME_FILE` inside.
///
/// On Windows a name Windows reads as a device (see `is_device_name`) has its
/// first character written as `%XX` too, which a folder may be named; elsewhere
/// the folders are as they have always been.
pub(super) fn encode_name(s: &str) -> CoreResult<(String, bool)> {
    encode_name_for(s, cfg!(windows))
}

/// Whether Windows takes a file or folder name for a device: `con`, `prn`,
/// `aux`, `nul`, `com1`-`com9`, `lpt1`-`lpt9`, in any case, alone or
/// followed by `.` and anything.
fn is_device_name(s: &str) -> bool {
    let stem = s.split('.').next().unwrap_or_default().to_ascii_lowercase();
    match stem.as_bytes() {
        b"con" | b"prn" | b"aux" | b"nul" => true,
        [b'c', b'o', b'm', n] | [b'l', b'p', b't', n] => (b'1'..=b'9').contains(n),
        _ => false,
    }
}

/// `encode_name`, for Windows (`windows`) or not.
pub(super) fn encode_name_for(s: &str, windows: bool) -> CoreResult<(String, bool)> {
    if s.is_empty() {
        return Err(history_err("empty database or collection name"));
    }
    let escape_first = windows && is_device_name(s);
    let mut out = String::with_capacity(s.len());
    for (i, b) in s.bytes().enumerate() {
        if (b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-') && !(escape_first && i == 0) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    if out.len() <= LONG_NAME {
        return Ok((out, false));
    }
    // Cut between encoded bytes, not inside a `%XX`.
    let mut cut = LONG_NAME;
    let o = out.as_bytes();
    if o[cut - 1] == b'%' {
        cut -= 1;
    } else if o[cut - 2] == b'%' {
        cut -= 2;
    }
    let hash: String = digest::digest(&digest::SHA256, s.as_bytes()).as_ref()[..16].iter().map(|b| format!("{b:02x}")).collect();
    Ok((format!("{}~{hash}", &out[..cut]), true))
}

/// The name a history directory stands for.
pub(super) fn dir_name(dir: &Path) -> Option<String> {
    let file = dir.file_name()?.to_str()?;
    if !file.contains('~') {
        return decode_name(file);
    }
    let name = fs::read_to_string(dir.join(NAME_FILE)).ok()?;
    (encode_name(&name).ok()?.0 == file).then_some(name)
}

/// The name a plain directory name stands for, if it is exactly what
/// `encode_name` writes for it (so `%61`, `%2e` and the like are refused).
pub(super) fn decode_name(s: &str) -> Option<String> {
    decode_name_for(s, cfg!(windows))
}

/// `decode_name`, for Windows (`windows`) or not.
pub(super) fn decode_name_for(s: &str, windows: bool) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let hex = s.get(i + 1..i + 3)?;
                if !hex.bytes().all(|c| c.is_ascii_digit() || (b'A'..=b'F').contains(&c)) {
                    return None;
                }
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            c if c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-' => {
                out.push(c);
                i += 1;
            }
            _ => return None,
        }
    }
    let name = String::from_utf8(out).ok()?;
    (encode_name_for(&name, windows).ok()?.0 == s).then_some(name)
}

fn format_time(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).unwrap_or_default().format("%Y%m%dT%H%M%S%3fZ").to_string()
}

pub(super) struct FileName {
    pub(super) ms: i64,
    pub(super) kind: Kind,
    /// Disambiguates entries begun in the same millisecond.
    pub(super) n: u32,
}

/// `20261113T185900123Z-delete.rhist`, or `…Z-delete-2.rhist`: only exactly
/// what `file_name` writes. The fields are read by position (retention reads
/// every file name), the date must exist, and the counter is never 0, signed
/// or zero-padded.
pub(super) fn parse_file_name(name: &str) -> Option<FileName> {
    let stem = name.strip_suffix(".rhist")?;
    let b = stem.as_bytes();
    if b.len() < 21 || b[8] != b'T' || b[18] != b'Z' || b[19] != b'-' {
        return None;
    }
    let num = |from: usize, to: usize| b[from..to].iter().try_fold(0i64, |n, c| c.is_ascii_digit().then(|| n * 10 + i64::from(c - b'0')));
    let (y, mo, d) = (num(0, 4)?, num(4, 6)?, num(6, 8)?);
    let (h, mi, sec, ms) = (num(9, 11)?, num(11, 13)?, num(13, 15)?, num(15, 18)?);
    if !(1..=12).contains(&mo) || d < 1 || d > days_in_month(y, mo) || h > 23 || mi > 59 || sec > 59 {
        return None;
    }
    let rest = &stem[20..];
    let (kind, n) = match rest.split_once('-') {
        Some((k, n)) if n.bytes().all(|c| c.is_ascii_digit()) && !n.starts_with('0') => (k, n.parse().ok()?),
        Some(_) => return None,
        None => (rest, 0),
    };
    let secs = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec;
    Some(FileName { ms: secs * 1000 + ms, kind: Kind::from_name(kind)?, n })
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days from 1970-01-01 to a date of the proleptic Gregorian calendar
/// (Howard Hinnant's `days_from_civil`).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

pub(super) fn file_name(ms: i64, kind: Kind, n: u32) -> String {
    if n == 0 { format!("{}-{}.{EXT}", format_time(ms), kind.name()) } else { format!("{}-{}-{n}.{EXT}", format_time(ms), kind.name()) }
}

pub(super) fn restored_path(file: &Path) -> PathBuf {
    let mut s = file.as_os_str().to_owned();
    s.push(format!(".{RESTORED_EXT}"));
    PathBuf::from(s)
}

/// The entry file name a `.restored` marker's name belongs to.
pub(super) fn strip_restored(file: &str) -> Option<&str> {
    file.strip_suffix(RESTORED_EXT)?.strip_suffix('.')
}

// Sealing and scanning records

// Sealing and scanning records

/// What every record authenticates besides itself: which file it is in (the
/// header's nonce; all zeros for the header, which is what names the file) and
/// where.
fn aad(file_id: &[u8; NONCE_LEN], seq: u64) -> [u8; NONCE_LEN + 8] {
    let mut a = [0u8; NONCE_LEN + 8];
    a[..NONCE_LEN].copy_from_slice(file_id);
    a[NONCE_LEN..].copy_from_slice(&seq.to_le_bytes());
    a
}

/// Seals one record onto `out`; returns its nonce.
pub(super) fn seal(cipher: &XChaCha20Poly1305, file_id: &[u8; NONCE_LEN], seq: u64, plain: &[u8], out: &mut Vec<u8>) -> CoreResult<[u8; NONCE_LEN]> {
    let nonce: [u8; NONCE_LEN] = rand::random();
    let ct = cipher.encrypt(XNonce::from_slice(&nonce), Payload { msg: plain, aad: &aad(file_id, seq) }).map_err(|_| history_err("could not encrypt a record"))?;
    out.extend_from_slice(&((NONCE_LEN + ct.len()) as u32).to_le_bytes());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(nonce)
}

pub(super) enum Record {
    Header { kind: Kind, at: DateTime, conn: String },
    /// A `save` call's records follow.
    Batch,
    Before(RawDocumentBuf),
    After(Vec<u8>, i64),
    End { count: u64, cancelled: bool },
}

/// The bytes of `{ id: <value> }`, to match a before-image with its result.
pub(super) fn pair_key(v: RawBsonRef<'_>) -> Vec<u8> {
    let mut d = RawDocumentBuf::new();
    d.append(cstr!("id"), v);
    d.into_bytes()
}

fn parse_record(raw: RawDocumentBuf) -> Option<Record> {
    match raw.get_str("k").ok()? {
        "h" => Some(Record::Header { kind: Kind::from_name(raw.get_str("kind").ok()?)?, at: raw.get_datetime("at").ok()?, conn: raw.get_str("conn").ok()?.to_string() }),
        "n" => Some(Record::Batch),
        "b" => Some(Record::Before(raw.get_document("d").ok()?.to_owned())),
        "a" => Some(Record::After(pair_key(raw.get("id").ok()??), raw.get_i64("h").ok()?)),
        "e" => Some(Record::End { count: raw.get_i64("count").ok()?.max(0) as u64, cancelled: raw.get_bool("cancelled").unwrap_or(false) }),
        _ => None,
    }
}

/// The sealed records of one file, in order, up to the first that is
/// incomplete or does not authenticate.
pub(super) struct Records {
    pub(super) r: BufReader<File>,
    pub(super) cipher: XChaCha20Poly1305,
    /// Zeros until the header has been read; then the header's nonce.
    pub(super) file_id: [u8; NONCE_LEN],
    pub(super) seq: u64,
    /// Bytes of the file not yet read.
    pub(super) left: u64,
    /// Size of the file.
    pub(super) len: u64,
}

impl Records {
    pub(super) fn open(path: &Path, key: &HistoryKey) -> CoreResult<Records> {
        let f = File::open(path).map_err(io_err)?;
        let len = f.metadata().map_err(io_err)?.len();
        let mut r = BufReader::new(f);
        let mut magic = [0u8; MAGIC.len()];
        let left = if len >= MAGIC.len() as u64 && r.read_exact(&mut magic).is_ok() && magic == MAGIC { len - MAGIC.len() as u64 } else { 0 };
        Ok(Records { r, cipher: key.cipher(), file_id: [0; NONCE_LEN], seq: 0, left, len })
    }

    fn stop<T>(&mut self) -> CoreResult<Option<T>> {
        self.left = 0;
        Ok(None)
    }

    pub(super) fn next(&mut self) -> CoreResult<Option<Record>> {
        match self.next_raw()?.and_then(parse_record) {
            Some(rec) => Ok(Some(rec)),
            None => self.stop(),
        }
    }

    /// The next record's authenticated plaintext.
    pub(super) fn next_raw(&mut self) -> CoreResult<Option<RawDocumentBuf>> {
        if self.left < 4 {
            return Ok(None);
        }
        let mut len = [0u8; 4];
        self.r.read_exact(&mut len).map_err(io_err)?;
        self.left -= 4;
        let len = u32::from_le_bytes(len) as u64;
        if len < (NONCE_LEN + TAG_LEN) as u64 || len > MAX_RECORD || len > self.left {
            return self.stop();
        }
        let mut buf = vec![0u8; len as usize];
        self.r.read_exact(&mut buf).map_err(io_err)?;
        self.left -= len;
        let (nonce, ct) = buf.split_at(NONCE_LEN);
        #[cfg(test)]
        DECRYPTS.with(|c| c.set(c.get() + 1));
        let Ok(plain) = self.cipher.decrypt(XNonce::from_slice(nonce), Payload { msg: ct, aad: &aad(&self.file_id, self.seq) }) else { return self.stop() };
        if self.seq == 0 {
            self.file_id.copy_from_slice(nonce);
        }
        self.seq += 1;
        match RawDocumentBuf::from_bytes(plain) {
            Ok(raw) => Ok(Some(raw)),
            Err(_) => self.stop(),
        }
    }
}
