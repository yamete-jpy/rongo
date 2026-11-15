//! The file operations that differ between operating systems, each with its
//! Unix and Windows bodies side by side: private folders and files, free
//! space, syncing a folder, and renames and removals that wait out a file
//! Windows reports as held open. Everything else in rongo is one code path
//! for every OS (bar the macOS-only Keychain move): elsewhere `cfg(unix)`
//! marks only tests that rely on Unix permissions.
//!
//! "Private" is owner-only on both. On Unix, folders are `0700` and files
//! `0600`. On Windows it is a protected DACL (nothing inherited from above)
//! granting full control to the current user and SYSTEM and to nobody else:
//! `create_private_dir` sets it on each folder it makes, inheritable, so
//! every file and folder made inside gets it too; `set_private` sets it on
//! one file. A new file from `private_options` takes its folder's DACL:
//! owner-only in rongo's own folders, the folder's elsewhere until
//! `set_private`. (The per-user `%APPDATA%`/`%LOCALAPPDATA%` themselves are
//! open to the user, SYSTEM and administrators.) The two differ in what
//! they do when no DACL can be set. `set_private` is strict (for a file the
//! user chose to save somewhere, with a password in it): only on FAT, FAT32
//! and exFAT, by the file system's name, which have no ACLs, is the file
//! left as it is (as macOS accepts a chmod on such a drive); anywhere else
//! the failure is the error. `create_private_dir` is lenient (rongo's own
//! folders, under a per-user folder that may be redirected to a server
//! share): a folder whose DACL can't be set keeps its inherited ACL and the
//! call is still `Ok`.
//!
//! Windows can't rename or delete a file another program has open without
//! sharing it for deletion, and antivirus opens every file just written for
//! a moment. `rename` and `remove_file` therefore try again for about a
//! second (`HOLD_WAIT`) while the answer is "in use", then give that error.
//! On Unix nothing is held that way: they try once, as `std::fs` does.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

/// How long `rename` and `remove_file` keep trying a file that is held open.
pub const HOLD_WAIT: Duration = Duration::from_secs(1);

/// Makes `dir` and the folders above it that are missing, each private.
/// One that exists already is left as it is. On Windows this is lenient:
/// rongo's own folders live under the per-user `%APPDATA%`/`%LOCALAPPDATA%`
/// (which may be redirected to a server share), so if the owner-only DACL
/// can't be set on a folder (any volume, any error) the folder is kept with
/// the ACL it inherited and this is still `Ok`: a vault or history must
/// never become impossible because a share refuses ACL changes.
pub fn create_private_dir(dir: &Path) -> io::Result<()> {
    sys::create_private_dir(dir)
}

/// Options that make a new file private (on Windows, as private as its
/// folder: see above). A file that exists already keeps its permissions
/// through `open`: `set_private` narrows it.
pub fn private_options() -> OpenOptions {
    sys::private_options()
}

/// Narrows an open file to private: `0600` on Unix, the owner-only DACL on
/// Windows. Strict on Windows: only on FAT, FAT32 or exFAT (by file system
/// name) is there nothing to set and the file left as it is; anywhere else
/// (a network share, say) a DACL that can't be set is the error, so a file
/// that was meant to be private is never left readable by others.
pub fn set_private(f: &File) -> io::Result<()> {
    sys::set_private(f)
}

/// Bytes the user may still write on the volume `path` is on, or would be
/// on once made: the nearest folder above it that exists is asked.
pub fn free_space(path: &Path) -> io::Result<u64> {
    let mut p = path;
    while !p.exists() {
        match p.parent() {
            Some(q) => p = q,
            None => break,
        }
    }
    fs4::available_space(p)
}

/// Makes the renames done in `dir` durable, best effort (a folder that can't
/// be opened is skipped). Nothing to do on Windows: NTFS journals the rename.
pub fn sync_dir(dir: &Path) {
    sys::sync_dir(dir);
}

/// Whether a private file on a volume whose file system Windows names
/// `fs_name` (`GetVolumeInformationByHandleW`) must get the owner-only
/// DACL. Only FAT, FAT32 and exFAT (a USB stick, say) are known to have no
/// ACLs to set, compared without regard to case; every other volume, any
/// share or file system that merely doesn't say it keeps ACLs included, is
/// asked to set one, and a refusal is an error (it fails closed).
#[cfg(any(windows, test))]
fn needs_dacl(fs_name: &str) -> bool {
    !["FAT", "FAT32", "exFAT"].iter().any(|fat| fs_name.eq_ignore_ascii_case(fat))
}

/// `std::fs::rename`, tried again for up to `HOLD_WAIT` while `from` or
/// `to` is held open (Windows), then that error. It may block for up to
/// `HOLD_WAIT` on Windows while the file is held: don't call it per file in
/// a loop while holding a lock.
pub fn rename(from: &Path, to: &Path) -> io::Result<()> {
    retry(HOLD_WAIT, sys::held, || std::fs::rename(from, to))
}

/// `std::fs::remove_file`, tried again for up to `HOLD_WAIT` while the
/// file is held open (Windows), then that error. It may block for up to
/// `HOLD_WAIT` on Windows while the file is held: don't call it per file in
/// a loop while holding a lock.
pub fn remove_file(path: &Path) -> io::Result<()> {
    retry(HOLD_WAIT, sys::held, || std::fs::remove_file(path))
}

/// Runs `op` until it succeeds, fails with an error `held` doesn't count, or
/// `wait` has passed (then its last error), pausing a little longer each time.
fn retry<T>(wait: Duration, held: impl Fn(&io::Error) -> bool, mut op: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let deadline = Instant::now() + wait;
    let mut pause = Duration::from_millis(10);
    loop {
        match op() {
            Err(e) if held(&e) => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(e);
                }
                std::thread::sleep(pause.min(deadline - now));
                pause = (pause * 2).min(Duration::from_millis(100));
            }
            r => return r,
        }
    }
}

#[cfg(unix)]
mod sys {
    use std::fs::{DirBuilder, File, OpenOptions, Permissions};
    use std::io;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    use std::path::Path;

    pub(super) fn create_private_dir(dir: &Path) -> io::Result<()> {
        DirBuilder::new().recursive(true).mode(0o700).create(dir)
    }

    pub(super) fn private_options() -> OpenOptions {
        let mut o = OpenOptions::new();
        o.mode(0o600);
        o
    }

    pub(super) fn set_private(f: &File) -> io::Result<()> {
        f.set_permissions(Permissions::from_mode(0o600))
    }

    pub(super) fn sync_dir(dir: &Path) {
        if let Ok(d) = File::open(dir) {
            let _ = d.sync_all();
        }
    }

    /// Nothing holds a file against a rename or removal on Unix.
    pub(super) fn held(_: &io::Error) -> bool {
        false
    }
}

#[cfg(windows)]
mod sys {
    use std::fs::{self, File, OpenOptions};
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::path::Path;
    use std::ptr;

    use windows_sys::Win32::Foundation::{INVALID_HANDLE_VALUE, MAX_PATH};
    use windows_sys::Win32::Security::Authorization::{SE_FILE_OBJECT, SetSecurityInfo};
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_FLAGS, ACL, ACL_REVISION, AddAccessAllowedAceEx, CONTAINER_INHERIT_ACE, CreateWellKnownSid, DACL_SECURITY_INFORMATION, GetLengthSid, GetTokenInformation, InitializeAcl, NO_INHERITANCE, OBJECT_INHERIT_ACE,
        PROTECTED_DACL_SECURITY_INFORMATION, PSID, SECURITY_MAX_SID_SIZE, TOKEN_QUERY, TOKEN_USER, TokenUser, WinLocalSystemSid,
    };
    use windows_sys::Win32::Storage::FileSystem::{FILE_ALL_ACCESS, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetVolumeInformationByHandleW, READ_CONTROL, ReOpenFile, WRITE_DAC};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    const ERROR_ACCESS_DENIED: i32 = 5;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    const ERROR_LOCK_VIOLATION: i32 = 33;

    /// Each folder made gets the owner-only DACL, which what is made in it
    /// later inherits; one made meanwhile by someone else is left as it is.
    /// Lenient: a folder whose DACL can't be set (a share that refuses ACL
    /// changes, or any other error) is kept with the ACL it inherited, and
    /// the call is still `Ok`. Only making a folder can fail it.
    pub(super) fn create_private_dir(dir: &Path) -> io::Result<()> {
        let mut missing = Vec::new();
        let mut p = Some(dir);
        while let Some(d) = p.filter(|d| !d.as_os_str().is_empty() && !d.is_dir()) {
            missing.push(d);
            p = d.parent();
        }
        if missing.is_empty() {
            return Ok(());
        }
        // Built once; without one (the token can't be read) the folders are made as they are.
        let acl = OwnerOnly::new(CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE).ok();
        for d in missing.into_iter().rev() {
            match fs::create_dir(d) {
                Ok(()) => {
                    if let Some(acl) = &acl {
                        let _ = make_dir_private(d, acl);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && d.is_dir() => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Gives a folder just made the owner-only DACL.
    fn make_dir_private(d: &Path, acl: &OwnerOnly) -> io::Result<()> {
        acl.set_on(&OpenOptions::new().access_mode(WRITE_DAC | READ_CONTROL).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(d)?)
    }

    /// The name Windows gives the file system of the volume `h` is on
    /// (`NTFS`, `ReFS`, `FAT32`, `exFAT`, whatever a share reports).
    pub(super) fn file_system_name(h: &impl AsRawHandle) -> io::Result<String> {
        // MAX_PATH + 1: a smaller buffer can make the whole call fail.
        let mut name = [0u16; MAX_PATH as usize + 1];
        // SAFETY: `h` is an open handle; only the file system's name is asked for, into `name` of the size given; every other buffer is null with size 0.
        if unsafe { GetVolumeInformationByHandleW(h.as_raw_handle(), ptr::null_mut(), 0, ptr::null_mut(), ptr::null_mut(), ptr::null_mut(), name.as_mut_ptr(), name.len() as u32) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        Ok(String::from_utf16_lossy(&name[..len]))
    }

    /// Whether a private file on the volume `h` is on must get the DACL (see
    /// `super::needs_dacl`). A volume that can't be asked is an error.
    pub(super) fn needs_dacl_on(h: &impl AsRawHandle) -> io::Result<bool> {
        Ok(super::needs_dacl(&file_system_name(h)?))
    }

    /// A new file takes its folder's DACL: owner-only in rongo's own folders.
    pub(super) fn private_options() -> OpenOptions {
        OpenOptions::new()
    }

    /// std opens files without `WRITE_DAC`: a second handle to the same file
    /// sets the DACL, and is closed on every path. Strict: only a file on FAT,
    /// FAT32 or exFAT (by name) is left as it is; anywhere else a DACL that
    /// can't be set is the error.
    pub(super) fn set_private(f: &File) -> io::Result<()> {
        if !needs_dacl_on(f)? {
            return Ok(());
        }
        let acl = OwnerOnly::new(NO_INHERITANCE)?;
        // SAFETY: `f` is open, so its handle is valid for the call; ReOpenFile answers a new handle or INVALID_HANDLE_VALUE.
        let h = unsafe { ReOpenFile(f.as_raw_handle(), WRITE_DAC | READ_CONTROL, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, 0) };
        if h == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `h` is a valid handle nothing else owns.
        let h = unsafe { OwnedHandle::from_raw_handle(h) };
        acl.set_on(&h)
    }

    /// Nothing to do: NTFS journals the rename itself.
    pub(super) fn sync_dir(_: &Path) {}

    /// What Windows answers while another program has the file open
    /// (antivirus, an indexer, a backup), or while a deleted file is still
    /// open somewhere: "access denied", "in use", "locked".
    pub(super) fn held(e: &io::Error) -> bool {
        matches!(e.raw_os_error(), Some(ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION))
    }

    /// A SID in a buffer of its own (DWORD-aligned, as a SID's sub-authorities are).
    struct Sid(Vec<u32>);

    impl Sid {
        fn ptr(&self) -> PSID {
            self.0.as_ptr().cast_mut().cast()
        }

        fn len(&self) -> usize {
            // SAFETY: the buffer holds a valid SID.
            unsafe { GetLengthSid(self.ptr()) as usize }
        }
    }

    /// The user this process runs as, from its token.
    fn current_user() -> io::Result<Sid> {
        let mut token = ptr::null_mut();
        // SAFETY: GetCurrentProcess is a pseudo-handle that needs no closing; `token` is an out parameter.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: OpenProcessToken succeeded: `token` is a handle of ours to close.
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let mut len = 0u32;
        // SAFETY: an empty buffer asks only for the size needed, into `len`.
        unsafe { GetTokenInformation(token.as_raw_handle(), TokenUser, ptr::null_mut(), 0, &mut len) };
        if len == 0 {
            return Err(io::Error::last_os_error());
        }
        // u64s: a TOKEN_USER starts with a pointer.
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        // SAFETY: `buf` has room for `len` bytes and is aligned for a TOKEN_USER.
        if unsafe { GetTokenInformation(token.as_raw_handle(), TokenUser, buf.as_mut_ptr().cast(), len, &mut len) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `buf` now holds a TOKEN_USER whose SID points into `buf`, alive while the SID is copied out.
        unsafe {
            let sid = (*buf.as_ptr().cast::<TOKEN_USER>()).User.Sid;
            let n = GetLengthSid(sid) as usize;
            let mut own = vec![0u32; n.div_ceil(4)];
            ptr::copy_nonoverlapping(sid.cast::<u8>(), own.as_mut_ptr().cast::<u8>(), n);
            Ok(Sid(own))
        }
    }

    /// SYSTEM (`S-1-5-18`).
    fn local_system() -> io::Result<Sid> {
        let mut len = SECURITY_MAX_SID_SIZE;
        let mut buf = vec![0u32; (len as usize).div_ceil(4)];
        // SAFETY: `buf` has room for `len` bytes, the most any SID takes; SYSTEM needs no domain SID.
        if unsafe { CreateWellKnownSid(WinLocalSystemSid, ptr::null_mut(), buf.as_mut_ptr().cast(), &mut len) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Sid(buf))
    }

    /// An ACL granting full control to the current user and SYSTEM, each
    /// ACE with `flags`, and to nobody else.
    struct OwnerOnly(Vec<u32>);

    impl OwnerOnly {
        fn new(flags: ACE_FLAGS) -> io::Result<Self> {
            let sids = [current_user()?, local_system()?];
            // An ACCESS_ALLOWED_ACE's last field is the SID's first DWORD.
            let len = size_of::<ACL>() + sids.iter().map(|s| size_of::<ACCESS_ALLOWED_ACE>() - size_of::<u32>() + s.len()).sum::<usize>();
            let len = len.next_multiple_of(4);
            let mut acl = vec![0u32; len / 4];
            // SAFETY: `acl` is `len` bytes, DWORD-aligned.
            if unsafe { InitializeAcl(acl.as_mut_ptr().cast(), len as u32, ACL_REVISION) } == 0 {
                return Err(io::Error::last_os_error());
            }
            for s in &sids {
                // SAFETY: the ACL was sized for both ACEs; `s` is a valid SID, which the ACE copies.
                if unsafe { AddAccessAllowedAceEx(acl.as_mut_ptr().cast(), ACL_REVISION, flags, FILE_ALL_ACCESS, s.ptr()) } == 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(OwnerOnly(acl))
        }

        /// Makes it the DACL of what `h` (open with `WRITE_DAC` and
        /// `READ_CONTROL`) is, protected: nothing inherited from above.
        fn set_on(&self, h: &impl AsRawHandle) -> io::Result<()> {
            let what = DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION;
            // SAFETY: `h` is an open handle; the ACL is initialised and lives through the call, which copies it.
            let e = unsafe { SetSecurityInfo(h.as_raw_handle(), SE_FILE_OBJECT, what, ptr::null_mut(), ptr::null_mut(), self.0.as_ptr().cast(), ptr::null()) };
            if e != 0 {
                return Err(io::Error::from_raw_os_error(e as i32));
            }
            Ok(())
        }
    }

    /// Reads a DACL back, for the tests.
    #[cfg(test)]
    pub(super) mod probe {
        use windows_sys::Win32::Foundation::LocalFree;
        use windows_sys::Win32::Security::Authorization::GetSecurityInfo;
        use windows_sys::Win32::Security::{ACE_HEADER, EqualSid, GetAce, GetSecurityDescriptorControl, SE_DACL_PROTECTED};

        use super::*;

        const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub(crate) enum Who {
            User,
            System,
            Other,
        }

        /// One ACE: whether it allows, whom, its access mask and its flags.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub(crate) struct Ace {
            pub(crate) allow: bool,
            pub(crate) who: Who,
            pub(crate) mask: u32,
            pub(crate) flags: u8,
        }

        #[derive(Debug, PartialEq, Eq)]
        pub(crate) struct Dacl {
            pub(crate) protected: bool,
            pub(crate) aces: Vec<Ace>,
            /// The ACL as stored, to tell a changed one.
            pub(crate) bytes: Vec<u8>,
        }

        pub(crate) fn dacl(path: &Path) -> io::Result<Dacl> {
            let f = OpenOptions::new().access_mode(READ_CONTROL).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(path)?;
            let (user, system) = (current_user()?, local_system()?);
            let (mut acl, mut sd) = (ptr::null_mut::<ACL>(), ptr::null_mut());
            // SAFETY: `f` is open with READ_CONTROL; on success `sd` is a descriptor to free with LocalFree, which `acl` points into.
            let e = unsafe { GetSecurityInfo(f.as_raw_handle(), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION, ptr::null_mut(), ptr::null_mut(), &mut acl, ptr::null_mut(), &mut sd) };
            if e != 0 {
                return Err(io::Error::from_raw_os_error(e as i32));
            }
            let read = (|| {
                let (mut control, mut revision) = (0u16, 0u32);
                // SAFETY: `sd` is the descriptor GetSecurityInfo answered.
                if unsafe { GetSecurityDescriptorControl(sd, &mut control, &mut revision) } == 0 {
                    return Err(io::Error::last_os_error());
                }
                let protected = control & SE_DACL_PROTECTED != 0;
                if acl.is_null() {
                    return Ok(Dacl { protected, aces: Vec::new(), bytes: Vec::new() });
                }
                // SAFETY: `acl` points at the valid ACL inside `sd`.
                let (count, size) = unsafe { ((*acl).AceCount, (*acl).AclSize) };
                let mut aces = Vec::new();
                for i in 0..u32::from(count) {
                    let mut ace = ptr::null_mut();
                    // SAFETY: `i` is below the ACL's ACE count.
                    if unsafe { GetAce(acl, i, &mut ace) } == 0 {
                        return Err(io::Error::last_os_error());
                    }
                    // SAFETY: every ACE starts with an ACE_HEADER.
                    let header = unsafe { *ace.cast::<ACE_HEADER>() };
                    let (allow, who, mask) = if header.AceType == ACCESS_ALLOWED_ACE_TYPE {
                        let a = ace.cast::<ACCESS_ALLOWED_ACE>();
                        // SAFETY: an access-allowed ACE is an ACCESS_ALLOWED_ACE whose SID starts at `SidStart`; both SIDs compared are valid.
                        unsafe {
                            let sid: PSID = (&raw mut (*a).SidStart).cast();
                            let who = if EqualSid(sid, user.ptr()) != 0 { Who::User } else if EqualSid(sid, system.ptr()) != 0 { Who::System } else { Who::Other };
                            (true, who, (*a).Mask)
                        }
                    } else {
                        (false, Who::Other, 0)
                    };
                    aces.push(Ace { allow, who, mask, flags: header.AceFlags });
                }
                // SAFETY: the ACL is `AclSize` bytes long.
                let bytes = unsafe { std::slice::from_raw_parts(acl.cast::<u8>(), usize::from(size)) }.to_vec();
                Ok(Dacl { protected, aces, bytes })
            })();
            // SAFETY: GetSecurityInfo said to free `sd` with LocalFree; nothing points into it after this.
            unsafe { LocalFree(sd) };
            read
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::testkit::TempDir;

    #[test]
    fn retry_gives_up_with_the_error_once_the_wait_is_over() {
        let tries = Cell::new(0);
        let start = Instant::now();
        let r: io::Result<()> = retry(Duration::from_millis(150), |_| true, || {
            tries.set(tries.get() + 1);
            Err(io::Error::other(format!("held, try {}", tries.get())))
        });
        let took = start.elapsed();
        assert_eq!(r.unwrap_err().to_string(), format!("held, try {}", tries.get()), "the last error");
        assert!(tries.get() > 2, "{} tries", tries.get());
        assert!(took >= Duration::from_millis(150) && took < Duration::from_millis(600), "{took:?}");
    }

    #[test]
    fn retry_stops_as_soon_as_it_works() {
        let tries = Cell::new(0);
        let r = retry(Duration::from_secs(5), |_| true, || {
            tries.set(tries.get() + 1);
            if tries.get() < 3 { Err(io::Error::other("held")) } else { Ok(7) }
        });
        assert_eq!((r.unwrap(), tries.get()), (7, 3));
    }

    #[test]
    fn retry_does_not_wait_for_any_other_error() {
        let tries = Cell::new(0);
        let start = Instant::now();
        let r: io::Result<()> = retry(Duration::from_secs(5), |_| false, || {
            tries.set(tries.get() + 1);
            Err(io::ErrorKind::NotFound.into())
        });
        assert_eq!((r.unwrap_err().kind(), tries.get()), (io::ErrorKind::NotFound, 1));
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn rename_and_remove_file_do_what_std_does() {
        let t = TempDir::made();
        let (a, b) = (t.0.join("a"), t.0.join("b"));
        std::fs::write(&a, "new").unwrap();
        std::fs::write(&b, "old").unwrap();
        rename(&a, &b).unwrap();
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "new", "renamed over the file there");
        assert!(!a.exists());
        remove_file(&b).unwrap();
        assert!(!b.exists());
        // What isn't there is said at once.
        let start = Instant::now();
        assert_eq!(rename(&a, &b).unwrap_err().kind(), io::ErrorKind::NotFound);
        assert_eq!(remove_file(&a).unwrap_err().kind(), io::ErrorKind::NotFound);
        assert!(start.elapsed() < HOLD_WAIT / 2, "{:?}", start.elapsed());
        // Best effort: a folder that isn't there is no error.
        sync_dir(&t.0);
        sync_dir(&t.0.join("missing"));
    }

    /// Only FAT, FAT32 and exFAT (by the name Windows gives the file system,
    /// whatever the case) are known to have no ACLs: every other volume, a
    /// share or a file system nobody has heard of included, gets the DACL.
    #[test]
    fn a_dacl_is_skipped_only_on_fat_by_name() {
        for fat in ["FAT", "FAT32", "exFAT", "fat", "Fat32", "EXFAT", "exfat", "eXFAT"] {
            assert!(!needs_dacl(fat), "{fat}");
        }
        for other in ["NTFS", "ntfs", "ReFS", "UDF", "CDFS", "NFS", "FAT32 ", " FAT", "exFAT\0", "FUSE", ""] {
            assert!(needs_dacl(other), "{other:?}");
        }
    }

    #[test]
    fn free_space_is_that_of_the_volume_even_below_a_folder_not_made_yet() {
        let t = TempDir::made();
        assert!(free_space(&t.0).unwrap() > 0);
        assert!(free_space(&t.0.join("not").join("made").join("yet")).unwrap() > 0);
        assert!(free_space(&TempDir::new().0).unwrap() > 0, "the temp folder's volume");
    }

    #[cfg(unix)]
    mod unix {
        use std::fs::{self, Permissions};
        use std::os::unix::fs::PermissionsExt;
        use std::path::Path;
        use std::time::Instant;

        use super::super::*;
        use crate::testkit::TempDir;

        fn mode(p: &Path) -> u32 {
            fs::metadata(p).unwrap().permissions().mode() & 0o777
        }

        #[test]
        fn a_private_dir_is_owner_only_at_every_level_it_makes() {
            let t = TempDir::made();
            fs::set_permissions(&t.0, Permissions::from_mode(0o755)).unwrap();
            let deep = t.0.join("a").join("b").join("c");
            create_private_dir(&deep).unwrap();
            for d in [t.0.join("a"), t.0.join("a").join("b"), deep.clone()] {
                assert_eq!(mode(&d), 0o700, "{d:?}");
            }
            assert_eq!(mode(&t.0), 0o755, "a folder already there is left as it is");
            fs::set_permissions(&deep, Permissions::from_mode(0o750)).unwrap();
            create_private_dir(&deep).unwrap();
            assert_eq!(mode(&deep), 0o750, "made already: no error, not changed");
        }

        #[test]
        fn a_private_file_is_owner_only_and_set_private_narrows_one_already_there() {
            let t = TempDir::made();
            let new = t.0.join("new");
            private_options().write(true).create_new(true).open(&new).unwrap();
            assert_eq!(mode(&new), 0o600);
            let old = t.0.join("old");
            fs::write(&old, "x").unwrap();
            fs::set_permissions(&old, Permissions::from_mode(0o644)).unwrap();
            let f = private_options().write(true).create(true).truncate(true).open(&old).unwrap();
            assert_eq!(mode(&old), 0o644, "open keeps the mode of a file already there");
            set_private(&f).unwrap();
            assert_eq!(mode(&old), 0o600);
        }

        /// The number history used to ask `statvfs` for: blocks free to
        /// the user × fragment size (other tests write meanwhile, hence the slack).
        #[test]
        fn free_space_is_what_statvfs_says() {
            use std::os::unix::ffi::OsStrExt;
            let t = TempDir::made();
            let c = std::ffi::CString::new(t.0.as_os_str().as_bytes()).unwrap();
            // SAFETY: `c` is a valid NUL-terminated path and `st` is a plain-data out parameter.
            let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
            assert_eq!(unsafe { libc::statvfs(c.as_ptr(), &mut st) }, 0);
            #[allow(clippy::unnecessary_cast)]
            let want = (st.f_bavail as u64).saturating_mul(st.f_frsize as u64);
            let got = free_space(&t.0).unwrap();
            assert!(got.abs_diff(want) < 256 << 20, "{got} against {want}");
        }

        /// Unix holds nothing open against a rename: a refusal is said at once.
        #[test]
        fn a_refused_rename_or_removal_is_not_tried_again() {
            let t = TempDir::made();
            let (closed, file) = (t.0.join("closed"), t.0.join("file"));
            fs::create_dir(&closed).unwrap();
            fs::write(closed.join("in"), "x").unwrap();
            fs::write(&file, "x").unwrap();
            fs::set_permissions(&closed, Permissions::from_mode(0o500)).unwrap();
            let start = Instant::now();
            let renamed = rename(&file, &closed.join("moved"));
            let removed = remove_file(&closed.join("in"));
            fs::set_permissions(&closed, Permissions::from_mode(0o700)).unwrap();
            assert_eq!(renamed.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
            assert_eq!(removed.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
            assert!(start.elapsed() < HOLD_WAIT / 2, "{:?}", start.elapsed());
            assert!(!sys::held(&io::ErrorKind::PermissionDenied.into()));
            assert!(!sys::held(&io::Error::from_raw_os_error(16)), "EBUSY");
        }
    }

    /// Compiled for Windows (`cargo test --target x86_64-pc-windows-gnu --no-run`)
    /// and run there.
    #[cfg(windows)]
    mod windows {
        use std::fs;
        use std::io::Write;
        use std::os::windows::fs::OpenOptionsExt;
        use std::time::{Duration, Instant};

        use super::super::sys::probe::{Ace, Who, dacl};
        use super::super::*;
        use crate::testkit::TempDir;

        #[test]
        fn in_use_counts_as_held_and_nothing_else_does() {
            for code in [5, 32, 33] {
                assert!(sys::held(&io::Error::from_raw_os_error(code)), "{code}");
            }
            for code in [2, 3, 80, 145] {
                assert!(!sys::held(&io::Error::from_raw_os_error(code)), "{code}");
            }
            assert!(!sys::held(&io::ErrorKind::NotFound.into()));
        }

        #[test]
        fn private_folders_and_files_are_made() {
            let t = TempDir::made();
            let deep = t.0.join("ก ไทย").join("b c");
            create_private_dir(&deep).unwrap();
            create_private_dir(&deep).unwrap();
            let f = private_options().write(true).create_new(true).open(deep.join("f")).unwrap();
            set_private(&f).unwrap();
            assert!(deep.join("f").is_file());
            sync_dir(&deep);
            assert!(free_space(&deep.join("not made")).unwrap() > 0);
        }

        const FULL: u32 = windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
        const INHERITABLE: u8 = (windows_sys::Win32::Security::OBJECT_INHERIT_ACE | windows_sys::Win32::Security::CONTAINER_INHERIT_ACE) as u8;
        const INHERITED: u8 = windows_sys::Win32::Security::INHERITED_ACE as u8;

        /// Full control for the user, then SYSTEM, each with `flags`, and nothing else.
        fn owner_only(flags: u8) -> Vec<Ace> {
            [Who::User, Who::System].map(|who| Ace { allow: true, who, mask: FULL, flags }).to_vec()
        }

        /// The temp folder is on the system volume (NTFS, or ReFS on a dev
        /// drive): the name is read whole from a file's handle and from a
        /// folder's, and its files get the DACL. (FAT32 and exFAT:
        /// `a_dacl_is_skipped_only_on_fat_by_name`.)
        #[test]
        fn the_system_volume_is_named_and_needs_the_dacl() {
            let t = TempDir::made();
            let file = t.0.join("f");
            let f = private_options().write(true).create_new(true).open(&file).unwrap();
            let name = sys::file_system_name(&f).unwrap();
            assert!(["NTFS", "ReFS"].contains(&name.as_str()), "{name:?}");
            assert!(sys::needs_dacl_on(&f).unwrap());
            let dir = fs::OpenOptions::new().access_mode(0).custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_BACKUP_SEMANTICS).open(&t.0).unwrap();
            assert_eq!(sys::file_system_name(&dir).unwrap(), name);
            assert!(sys::needs_dacl_on(&dir).unwrap());
        }

        /// A file the user saved anywhere (a shared folder, say): only the
        /// user and SYSTEM, nothing inherited from the folder.
        #[test]
        fn set_private_leaves_the_user_and_system_only() {
            let t = TempDir::made();
            let p = t.0.join("ก ไทย b.txt");
            let f = private_options().write(true).create(true).truncate(true).open(&p).unwrap();
            assert!(!dacl(&p).unwrap().protected, "the folder's, until set_private");
            set_private(&f).unwrap();
            let d = dacl(&p).unwrap();
            assert!(d.protected, "{d:?}");
            assert_eq!(d.aces, owner_only(0));
            (&f).write_all(b"still writable through std's handle").unwrap();
            drop(f);
            assert_eq!(fs::read_to_string(&p).unwrap(), "still writable through std's handle");
        }

        /// Every folder it makes is owner-only, and so is what is made inside;
        /// a folder already there is left as it is.
        #[test]
        fn a_private_folder_and_what_is_made_in_it_are_the_users_and_systems_only() {
            let t = TempDir::made();
            let before = dacl(&t.0).unwrap();
            let (outer, deep) = (t.0.join("ก ไทย"), t.0.join("ก ไทย").join("b c"));
            create_private_dir(&deep).unwrap();
            for d in [&outer, &deep] {
                let got = dacl(d).unwrap();
                assert!(got.protected, "{d:?}: {got:?}");
                assert_eq!(got.aces, owner_only(INHERITABLE), "{d:?}");
            }
            assert_eq!(dacl(&t.0).unwrap(), before, "a folder already there is left as it is");
            let file = deep.join("f.rhist");
            private_options().write(true).create_new(true).open(&file).unwrap();
            let got = dacl(&file).unwrap();
            assert!(!got.protected, "{got:?}");
            assert_eq!(got.aces, owner_only(INHERITED), "inherited from its folder");
            let made = dacl(&deep).unwrap();
            create_private_dir(&deep).unwrap();
            assert_eq!(dacl(&deep).unwrap(), made, "made already: no error, not changed");
        }

        /// A file another program holds without sharing it for deletion:
        /// removed once let go, within `HOLD_WAIT`.
        #[test]
        fn a_file_let_go_within_the_wait_is_removed() {
            let t = TempDir::made();
            let p = t.0.join("held");
            fs::write(&p, "x").unwrap();
            let f = fs::OpenOptions::new().read(true).share_mode(0).open(&p).unwrap();
            let let_go = std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                drop(f);
            });
            remove_file(&p).unwrap();
            let_go.join().unwrap();
            assert!(!p.exists());
        }

        /// Held all along: the error comes back after `HOLD_WAIT`.
        #[test]
        fn a_file_held_all_along_gives_the_error() {
            let t = TempDir::made();
            let (p, to) = (t.0.join("held"), t.0.join("to"));
            fs::write(&p, "x").unwrap();
            let f = fs::OpenOptions::new().read(true).share_mode(0).open(&p).unwrap();
            let start = Instant::now();
            let renamed = rename(&p, &to);
            let removed = remove_file(&p);
            drop(f);
            assert_eq!(renamed.unwrap_err().raw_os_error(), Some(32));
            assert_eq!(removed.unwrap_err().raw_os_error(), Some(32));
            assert!(start.elapsed() >= HOLD_WAIT * 2, "{:?}", start.elapsed());
            assert!(p.exists() && !to.exists());
        }
    }
}
