//! NTFS volume access through documented Win32 control codes: the volume
//! handle, the change journal (`FSCTL_QUERY/CREATE/READ_USN_JOURNAL`) and
//! the MFT enumeration (`FSCTL_ENUM_USN_DATA`). Opening a volume needs
//! administrator rights; everything else here is safe to call unelevated.

use crate::snapshot::VolumeSnapshot;
use crate::usn::{UsnRecord, parse_enum_output, parse_read_output};
use std::io;

/// Journal state of a volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JournalInfo {
    pub id: u64,
    /// Oldest USN still in the journal.
    pub first_usn: i64,
    pub next_usn: i64,
}

/// What to do with a saved volume snapshot at start-up.
#[derive(Debug, PartialEq, Eq)]
pub enum Replay {
    /// Replay journal records from this USN on top of the snapshot.
    From(i64),
    /// The journal cannot fill the gap; scan the volume again.
    Rescan(&'static str),
}

/// Decides whether a snapshot can be brought up to date from the journal:
/// not when the journal was recreated (new id) or has already discarded
/// records the snapshot did not see.
pub fn replay_decision(saved: &VolumeSnapshot, now: &JournalInfo) -> Replay {
    replay_from(saved.journal_id, saved.next_usn, now)
}

/// [`replay_decision`] from a saved journal id and position.
pub fn replay_from(journal_id: u64, next_usn: i64, now: &JournalInfo) -> Replay {
    if journal_id != now.id {
        Replay::Rescan("the change journal was recreated")
    } else if next_usn < now.first_usn {
        Replay::Rescan("the change journal no longer holds every change since the snapshot")
    } else if next_usn > now.next_usn {
        Replay::Rescan("the snapshot is ahead of the change journal")
    } else {
        Replay::From(next_usn)
    }
}

/// `ERROR_ACCESS_DENIED`: opening a volume without administrator rights.
pub const ERROR_ACCESS_DENIED: i32 = 5;

/// Message for a full-index attempt without administrator rights.
pub const NEEDS_ADMIN: &str =
    "full indexing needs administrator rights (run elevated or install the service)";

#[cfg(windows)]
pub use os::*;

#[cfg(windows)]
mod os {
    use super::*;
    use crate::ntfs;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_HANDLE_EOF, ERROR_INVALID_PARAMETER, ERROR_JOURNAL_NOT_ACTIVE,
        ERROR_NO_MORE_FILES, ERROR_NOT_ALL_ASSIGNED, GENERIC_READ, GENERIC_WRITE, HANDLE,
        INVALID_HANDLE_VALUE, LUID,
    };
    use windows_sys::Win32::Security::{
        AdjustTokenPrivileges, GetTokenInformation, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW,
        SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_ELEVATION, TOKEN_PRIVILEGES,
        TOKEN_QUERY, TokenElevation,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_NO_BUFFERING, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_DESCRIPTOR,
        FILE_ID_DESCRIPTOR_0, FILE_LIST_DIRECTORY, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo, FileIdType,
        FlushFileBuffers, GetDriveTypeW, GetFileInformationByHandle, GetFileInformationByHandleEx,
        GetLogicalDrives, GetVolumeInformationW, OPEN_EXISTING, OpenFileById, READ_CONTROL,
        ReadFile, SYNCHRONIZE,
    };
    use windows_sys::Win32::System::IO::{CancelSynchronousIo, DeviceIoControl, OVERLAPPED};
    use windows_sys::Win32::System::Ioctl::{
        CREATE_USN_JOURNAL_DATA, FSCTL_CREATE_USN_JOURNAL, FSCTL_ENUM_USN_DATA,
        FSCTL_GET_NTFS_FILE_RECORD, FSCTL_GET_NTFS_VOLUME_DATA, FSCTL_QUERY_USN_JOURNAL,
        FSCTL_READ_USN_JOURNAL, MFT_ENUM_DATA_V0, NTFS_VOLUME_DATA_BUFFER,
        READ_USN_JOURNAL_DATA_V0, USN_JOURNAL_DATA_V0,
    };

    /// Largest MFT record size NTFS uses.
    const MAX_RECORD: usize = 4096;
    /// Bytes per raw read of the MFT or `$SDS` (whole clusters).
    const READ_BLOCK: usize = 4 << 20;
    /// Larger than any real one (NTFS keeps attribute lists under
    /// 256 KiB; `$SDS` was 7.5 MiB on a 0.97 M-folder volume).
    const ATTRIBUTE_LIST_MAX: u64 = 1 << 20;
    const SDS_MAX: u64 = 256 << 20;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    fn wide(s: &str) -> Vec<u16> {
        std::ffi::OsStr::new(s).encode_wide().chain([0]).collect()
    }

    /// An owned Win32 handle, closed on drop.
    pub struct Handle(HANDLE);

    // SAFETY: a file/volume handle may be used and closed from any thread.
    unsafe impl Send for Handle {}
    // SAFETY: DeviceIoControl on one handle from several threads is allowed.
    unsafe impl Sync for Handle {}

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: self.0 is a valid handle this value owns; closed once.
            unsafe { CloseHandle(self.0) };
        }
    }

    impl Handle {
        pub fn raw(&self) -> HANDLE {
            self.0
        }

        /// Takes ownership of a handle; it is closed on drop.
        ///
        /// # Safety
        /// `h` must be a valid, open handle that nothing else closes.
        pub unsafe fn from_raw(h: HANDLE) -> Handle {
            Handle(h)
        }
    }

    fn open(path: &str, access: u32, flags: u32) -> io::Result<Handle> {
        let w = wide(path);
        // SAFETY: w is NUL-terminated and outlives the call; null security
        // attributes and template are allowed.
        let h = unsafe {
            CreateFileW(
                w.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                flags,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(Handle(h))
        }
    }

    /// Runs one control code; returns the number of output bytes.
    fn ioctl<I>(h: &Handle, code: u32, input: Option<&I>, out: &mut [u8]) -> io::Result<usize> {
        let mut got = 0u32;
        let (ip, il) = match input {
            Some(i) => (i as *const I as *const _, size_of::<I>() as u32),
            None => (std::ptr::null(), 0),
        };
        // SAFETY: input points to a live I of il bytes (or is null with 0);
        // out is a writable buffer of the given length; synchronous call.
        let ok = unsafe {
            DeviceIoControl(
                h.raw(),
                code,
                ip,
                il,
                out.as_mut_ptr() as *mut _,
                out.len() as u32,
                &mut got,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(got as usize)
        }
    }

    /// `DRIVE_FIXED` from `GetDriveTypeW`.
    const DRIVE_FIXED: u32 = 3;

    /// Fixed (not removable, not network) drives with another file
    /// system (ReFS, exFAT, FAT32): no MFT, so the app walks them in user
    /// mode.
    pub fn fixed_other_drives() -> Vec<String> {
        fixed_drives().into_iter().filter(|d| !is_ntfs(d)).collect()
    }

    /// Fixed (not removable, not network) drives formatted NTFS, as
    /// "C:" strings.
    pub fn fixed_ntfs_drives() -> Vec<String> {
        fixed_drives().into_iter().filter(|d| is_ntfs(d)).collect()
    }

    fn fixed_drives() -> Vec<String> {
        // SAFETY: no arguments.
        let mask = unsafe { GetLogicalDrives() };
        (0..26u8)
            .filter(|i| mask & (1 << i) != 0)
            .map(|i| format!("{}:", (b'A' + i) as char))
            .filter(|d| {
                let w = wide(&format!("{d}\\"));
                // SAFETY: w is NUL-terminated.
                let t = unsafe { GetDriveTypeW(w.as_ptr()) };
                t == DRIVE_FIXED
            })
            .collect()
    }

    /// Wakes `thread` from a blocking I/O call (a journal read) with
    /// `ERROR_OPERATION_ABORTED`; does nothing if it is not in one.
    pub fn cancel_blocking_io(thread: &std::thread::JoinHandle<()>) {
        use std::os::windows::io::AsRawHandle;
        // SAFETY: the join handle keeps the thread handle open.
        unsafe { CancelSynchronousIo(thread.as_raw_handle() as HANDLE) };
    }

    /// Enables a privilege this process's token holds (e.g.
    /// "SeBackupPrivilege"); an error when the token lacks it.
    pub fn enable_privilege(name: &str) -> io::Result<()> {
        let mut t: HANDLE = std::ptr::null_mut();
        // SAFETY: pseudo handle of this process; t receives a token handle.
        if unsafe {
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                &mut t,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let t = Handle(t);
        let w = wide(name);
        let mut tp = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: LUID {
                    LowPart: 0,
                    HighPart: 0,
                },
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        // SAFETY: w is NUL-terminated; the LUID is written into tp.
        if unsafe {
            LookupPrivilegeValueW(std::ptr::null(), w.as_ptr(), &mut tp.Privileges[0].Luid)
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: t has TOKEN_ADJUST_PRIVILEGES; tp is a valid one-entry list.
        let ok = unsafe {
            AdjustTokenPrivileges(
                t.raw(),
                0,
                &tp,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        // AdjustTokenPrivileges succeeds without assigning a privilege the
        // token lacks; that is reported by the last error
        let e = io::Error::last_os_error();
        if ok == 0 || e.raw_os_error() == Some(ERROR_NOT_ALL_ASSIGNED as i32) {
            return Err(e);
        }
        Ok(())
    }

    /// Whether this process runs elevated (administrator token).
    pub fn is_elevated() -> bool {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: the pseudo handle of the current process is always valid;
        // token receives a new handle on success.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return false;
        }
        let token = Handle(token);
        let mut e = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut len = 0u32;
        // SAFETY: e is a TOKEN_ELEVATION of the size passed.
        let ok = unsafe {
            GetTokenInformation(
                token.raw(),
                TokenElevation,
                &mut e as *mut _ as *mut _,
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut len,
            )
        };
        ok != 0 && e.TokenIsElevated != 0
    }

    /// File system name and serial of `drive` ("C:"), without admin.
    pub fn volume_info(drive: &str) -> io::Result<(String, u32)> {
        let root = wide(&format!("{drive}\\"));
        let mut fs = [0u16; 64];
        let mut serial = 0u32;
        // SAFETY: root is NUL-terminated; fs is a writable buffer of the
        // length passed; unused outputs are null.
        let ok = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                std::ptr::null_mut(),
                0,
                &mut serial,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                fs.as_mut_ptr(),
                fs.len() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let n = fs.iter().position(|&c| c == 0).unwrap_or(fs.len());
        Ok((String::from_utf16_lossy(&fs[..n]), serial))
    }

    pub fn is_ntfs(drive: &str) -> bool {
        volume_info(drive).is_ok_and(|(fs, _)| fs == "NTFS")
    }

    /// The entries of the folder open as `h` (with `FILE_LIST_DIRECTORY`)
    /// as creation records under `frn`.
    fn list_open_dir(h: &Handle, frn: u64) -> io::Result<Vec<UsnRecord>> {
        // FILE_ID_BOTH_DIR_INFO entries are 8-byte aligned
        let mut buf = vec![0u64; 8 << 10];
        let bytes = buf.len() * 8;
        let mut class = FileIdBothDirectoryRestartInfo;
        let mut out = Vec::new();
        loop {
            // SAFETY: h is an open folder handle; buf is writable for
            // `bytes` bytes and outlives the call.
            let ok = unsafe {
                GetFileInformationByHandleEx(h.raw(), class, buf.as_mut_ptr().cast(), bytes as u32)
            };
            if ok == 0 {
                let e = io::Error::last_os_error();
                return match e.raw_os_error() {
                    Some(c) if c == ERROR_NO_MORE_FILES as i32 => Ok(out),
                    _ => Err(e),
                };
            }
            // SAFETY: buf holds `bytes` initialised bytes.
            let raw = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), bytes) };
            out.extend(
                crate::usn::parse_dir_info(raw, frn)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e:?}")))?,
            );
            class = FileIdBothDirectoryInfo;
        }
    }

    /// [`list_open_dir`] of a folder by path, under parent FRN 7 (tests).
    #[cfg(test)]
    pub(super) fn list_dir_at(dir: &std::path::Path) -> io::Result<Vec<UsnRecord>> {
        let h = open(
            &dir.to_string_lossy(),
            FILE_LIST_DIRECTORY | SYNCHRONIZE,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
        )?;
        list_open_dir(&h, 7)
    }

    /// The file id (FRN on NTFS) of a path (tests).
    #[cfg(test)]
    pub(super) fn file_id(p: &std::path::Path) -> io::Result<u64> {
        let h = open(&p.to_string_lossy(), 0, FILE_FLAG_BACKUP_SEMANTICS)?;
        // SAFETY: zeroed is a valid BY_HANDLE_FILE_INFORMATION (plain data).
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: h is an open handle; info is writable.
        if unsafe { GetFileInformationByHandle(h.raw(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64)
    }

    /// FRN of the volume's root folder (works without admin).
    pub fn root_frn(drive: &str) -> io::Result<u64> {
        let h = open(&format!("{drive}\\"), 0, FILE_FLAG_BACKUP_SEMANTICS)?;
        // SAFETY: zeroed is a valid BY_HANDLE_FILE_INFORMATION (plain data).
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: h is an open handle; info is writable.
        if unsafe { GetFileInformationByHandle(h.raw(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64)
    }

    /// An open NTFS volume (`\\.\C:`).
    pub struct Volume {
        h: Handle,
        pub drive: String,
        pub serial: u32,
    }

    impl Volume {
        /// Opens `drive` ("C:"). Fails with `ERROR_ACCESS_DENIED` when the
        /// process is not elevated.
        pub fn open(drive: &str) -> io::Result<Volume> {
            let (_, serial) = volume_info(drive)?;
            let h = open(&format!(r"\\.\{drive}"), GENERIC_READ, 0)?;
            Ok(Volume {
                h,
                drive: drive.to_string(),
                serial,
            })
        }

        /// The journal's state; creates the journal when the volume has none
        /// (32 MiB, as Windows itself sizes it on a system drive).
        pub fn query_journal(&self) -> io::Result<(JournalInfo, bool)> {
            let mut out = [0u8; size_of::<USN_JOURNAL_DATA_V0>()];
            let created = match ioctl::<()>(&self.h, FSCTL_QUERY_USN_JOURNAL, None, &mut out) {
                Ok(_) => false,
                Err(e) if e.raw_os_error() == Some(ERROR_JOURNAL_NOT_ACTIVE as i32) => {
                    let create = CREATE_USN_JOURNAL_DATA {
                        MaximumSize: 32 << 20,
                        AllocationDelta: 8 << 20,
                    };
                    ioctl(&self.h, FSCTL_CREATE_USN_JOURNAL, Some(&create), &mut [])?;
                    ioctl::<()>(&self.h, FSCTL_QUERY_USN_JOURNAL, None, &mut out)?;
                    true
                }
                Err(e) => return Err(e),
            };
            // SAFETY: out holds a USN_JOURNAL_DATA_V0 written by the kernel;
            // read_unaligned copes with the byte buffer's alignment.
            let d: USN_JOURNAL_DATA_V0 =
                unsafe { std::ptr::read_unaligned(out.as_ptr() as *const _) };
            Ok((
                JournalInfo {
                    id: d.UsnJournalID,
                    first_usn: d.FirstUsn,
                    next_usn: d.NextUsn,
                },
                created,
            ))
        }

        /// Every file and folder record in the MFT, in FRN order (children
        /// often before parents). `high_usn` is the journal's next USN.
        pub fn enum_mft(&self, high_usn: i64, sink: impl FnMut(UsnRecord)) -> io::Result<()> {
            self.enum_mft_until(high_usn, &std::sync::atomic::AtomicBool::new(false), sink)
        }

        /// [`Volume::enum_mft`] that ends early with `Interrupted` once
        /// `stop` is set (checked between 1 MiB batches).
        pub fn enum_mft_until(
            &self,
            high_usn: i64,
            stop: &std::sync::atomic::AtomicBool,
            mut sink: impl FnMut(UsnRecord),
        ) -> io::Result<()> {
            let mut buf = vec![0u8; 1 << 20];
            let mut input = MFT_ENUM_DATA_V0 {
                StartFileReferenceNumber: 0,
                LowUsn: 0,
                HighUsn: high_usn,
            };
            loop {
                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                    return Err(io::Error::new(io::ErrorKind::Interrupted, "stopped"));
                }
                let n = match ioctl(&self.h, FSCTL_ENUM_USN_DATA, Some(&input), &mut buf) {
                    Ok(n) => n,
                    Err(e) if e.raw_os_error() == Some(ERROR_HANDLE_EOF as i32) => return Ok(()),
                    Err(e) => return Err(e),
                };
                let (next, recs) = parse_enum_output(&buf[..n])
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e:?}")))?;
                if recs.is_empty() && next == input.StartFileReferenceNumber {
                    return Ok(());
                }
                recs.into_iter().for_each(&mut sink);
                input.StartFileReferenceNumber = next;
            }
        }

        /// Opens the file or folder with this FRN for its security
        /// descriptor only (`READ_CONTROL`), never its contents.
        pub fn open_by_id(&self, frn: u64) -> io::Result<Handle> {
            self.open_id(frn, READ_CONTROL, 0)
        }

        /// The names in the folder with this FRN, as creation records
        /// (its listing only: no file is opened). Backup semantics read
        /// any folder; what the index shows of them is still decided by
        /// the owner rules and folder checks.
        pub fn list_dir(&self, frn: u64) -> io::Result<Vec<UsnRecord>> {
            // a link to a folder is opened itself, never followed: its
            // target's entries are indexed at their own place
            let h = self.open_id(
                frn,
                FILE_LIST_DIRECTORY | SYNCHRONIZE,
                FILE_FLAG_OPEN_REPARSE_POINT,
            )?;
            list_open_dir(&h, frn)
        }

        /// MFT record `number` as NTFS keeps it in memory (update
        /// sequence applied); `NotFound` when that record is not in use.
        pub fn file_record(&self, number: u64) -> io::Result<Vec<u8>> {
            let input = (number & 0xFFFF_FFFF_FFFF) as i64;
            // 12-byte header (record number, length), then the record
            let mut out = vec![0u8; 12 + MAX_RECORD];
            let n = match ioctl(&self.h, FSCTL_GET_NTFS_FILE_RECORD, Some(&input), &mut out) {
                Ok(n) => n,
                Err(e) if e.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) => {
                    return Err(io::Error::new(io::ErrorKind::NotFound, "past the MFT"));
                }
                Err(e) => return Err(e),
            };
            let got = u64::from_le_bytes(out[0..8].try_into().unwrap_or_default());
            let len = u32::from_le_bytes(out[8..12].try_into().unwrap_or_default()) as usize;
            // a record not in use: NTFS returns the nearest one before it
            if got & 0xFFFF_FFFF_FFFF != number & 0xFFFF_FFFF_FFFF {
                return Err(io::Error::new(io::ErrorKind::NotFound, "record not in use"));
            }
            if n < 12 + len || len == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "short file record",
                ));
            }
            out.truncate(12 + len);
            out.drain(..12);
            Ok(out)
        }

        fn ntfs_data(&self) -> io::Result<NTFS_VOLUME_DATA_BUFFER> {
            let mut out = [0u8; size_of::<NTFS_VOLUME_DATA_BUFFER>()];
            ioctl::<()>(&self.h, FSCTL_GET_NTFS_VOLUME_DATA, None, &mut out)?;
            // SAFETY: out holds the structure written by the kernel;
            // read_unaligned copes with the byte buffer's alignment.
            Ok(unsafe { std::ptr::read_unaligned(out.as_ptr() as *const _) })
        }

        /// A handle for reading the volume's sectors directly: the volume
        /// is flushed first, so metadata Windows changed a moment ago and
        /// still holds in memory (a folder's new permissions) is on disk,
        /// and reads bypass the cache, which may hold sectors read before.
        /// Its own handle: never queued behind the journal reads.
        fn raw_reader(&self) -> io::Result<Handle> {
            let h = open(
                &format!(r"\\.\{}", self.drive),
                GENERIC_READ | GENERIC_WRITE,
                FILE_FLAG_NO_BUFFERING,
            )?;
            // SAFETY: h is an open volume handle with write access.
            if unsafe { FlushFileBuffers(h.raw()) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(h)
        }

        /// Reads `buf.len()` bytes at `offset` (sector multiples, `buf`
        /// sector-aligned) with a `raw_reader` handle.
        fn read_at(h: &Handle, offset: u64, buf: &mut [u8]) -> io::Result<()> {
            let mut done = 0;
            while done < buf.len() {
                let at = offset
                    .checked_add(done as u64)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "offset"))?;
                let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
                ov.Anonymous.Anonymous.Offset = at as u32;
                ov.Anonymous.Anonymous.OffsetHigh = (at >> 32) as u32;
                let rest = &mut buf[done..];
                let mut got = 0u32;
                // SAFETY: rest is writable for its length; ov carries the
                // offset of this synchronous read and outlives it.
                let ok = unsafe {
                    ReadFile(
                        h.raw(),
                        rest.as_mut_ptr(),
                        rest.len().min(u32::MAX as usize) as u32,
                        &mut got,
                        &mut ov,
                    )
                };
                if ok == 0 {
                    return Err(io::Error::last_os_error());
                }
                if got == 0 {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                done += got as usize;
            }
            Ok(())
        }

        /// Streams the first `bytes` of the clusters `runs` maps to
        /// `sink`, in blocks of at most `READ_BLOCK` (sparse runs as
        /// zeros); stops when `sink` returns false.
        fn read_runs(
            h: &Handle,
            runs: &[ntfs::Run],
            cluster: u64,
            bytes: u64,
            mut sink: impl FnMut(&[u8]) -> bool,
        ) -> io::Result<()> {
            let bad = || io::Error::new(io::ErrorKind::InvalidData, "run past the volume");
            // sector-aligned for reads that bypass the cache
            let mut space = vec![0u8; READ_BLOCK + 4096];
            let skip = space.as_ptr().align_offset(4096);
            let buf = &mut space[skip..skip + READ_BLOCK];
            let mut left = bytes;
            for run in runs {
                let size = run.clusters.checked_mul(cluster).ok_or_else(bad)?;
                let mut pos = 0;
                while pos < size && left > 0 {
                    let n = (size - pos).min(READ_BLOCK as u64) as usize;
                    let b = &mut buf[..n];
                    match run.lcn {
                        Some(lcn) => {
                            let at = lcn.checked_mul(cluster).and_then(|o| o.checked_add(pos));
                            Self::read_at(h, at.ok_or_else(bad)?, b)?
                        }
                        None => b.fill(0),
                    }
                    let take = (n as u64).min(left) as usize;
                    if !sink(&b[..take]) {
                        return Ok(());
                    }
                    left -= take as u64;
                    pos += n as u64;
                }
            }
            if left > 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "runs shorter than the stream",
                ));
            }
            Ok(())
        }

        /// The real size and runs of the non-resident attribute
        /// `kind`/`name` of record `number`, with the parts held in the
        /// records its attribute list names.
        fn attribute_runs(
            &self,
            h: &Handle,
            number: u64,
            kind: u32,
            name: &str,
        ) -> io::Result<(u64, Vec<ntfs::Run>)> {
            let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_string());
            let cluster = u64::from(self.ntfs_data()?.BytesPerCluster);
            let base = self.file_record(number)?;
            let mut parts: Vec<(u64, u64, Vec<ntfs::Run>)> = Vec::new();
            let mut take = |rec: &[u8]| {
                for a in ntfs::attributes(rec) {
                    if a.kind == kind && a.name == name && !a.resident {
                        parts.extend(ntfs::data_runs(&a));
                    }
                }
            };
            take(&base);
            let attrs = ntfs::attributes(&base);
            if let Some(list) = attrs.iter().find(|a| a.kind == ntfs::ATTR_ATTRIBUTE_LIST) {
                let bytes = match ntfs::resident_value(list) {
                    Some(v) => v.to_vec(),
                    None => {
                        let (_, real, runs) =
                            ntfs::data_runs(list).ok_or_else(|| bad("attribute list"))?;
                        if real > ATTRIBUTE_LIST_MAX {
                            return Err(bad("attribute list size"));
                        }
                        let mut v = Vec::new();
                        Self::read_runs(h, &runs, cluster, real, |b| {
                            v.extend_from_slice(b);
                            true
                        })?;
                        v
                    }
                };
                for r in ntfs::listed_records(&bytes, kind, name, number) {
                    take(&self.file_record(r)?);
                }
            }
            ntfs::join_parts(parts).ok_or_else(|| bad("attribute parts"))
        }

        /// (FRN, security id) of every in-use folder, sorted by FRN, read
        /// from the MFT in large blocks: no folder is opened. Records
        /// being written at that moment are left out (their folders are
        /// checked another way). Ends early with `Interrupted` on `stop`.
        pub fn folder_security_ids(
            &self,
            stop: &std::sync::atomic::AtomicBool,
        ) -> io::Result<Vec<(u64, u32)>> {
            let d = self.ntfs_data()?;
            let size = d.BytesPerFileRecordSegment as usize;
            if !(256..=MAX_RECORD).contains(&size) {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "record size"));
            }
            let h = self.raw_reader()?;
            let (real, runs) = self.attribute_runs(&h, 0, ntfs::ATTR_DATA, "")?;
            let bytes = real.min(d.MftValidDataLength.max(0) as u64);
            let mut out = Vec::new();
            let mut number = 0u64;
            let mut carry: Vec<u8> = Vec::new();
            let mut stopped = false;
            Self::read_runs(&h, &runs, u64::from(d.BytesPerCluster), bytes, |block| {
                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                    stopped = true;
                    return false;
                }
                carry.extend_from_slice(block);
                let whole = carry.len() / size * size;
                for rec in carry[..whole].chunks_exact_mut(size) {
                    // a record saying another number: the runs went wrong
                    if ntfs::apply_fixup(rec)
                        && ntfs::record_number(rec) == Some(number as u32)
                        && let Some(sid) = ntfs::folder_security_id(rec)
                        && let Some(frn) = ntfs::frn_of(rec, number)
                    {
                        out.push((frn, sid));
                    }
                    number += 1;
                }
                carry.drain(..whole);
                true
            })?;
            if stopped {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "stopped"));
            }
            out.sort_unstable_by_key(|x| x.0);
            Ok(out)
        }

        /// The volume's security descriptors by security id, read from
        /// `$Secure:$SDS`.
        pub fn security_descriptors(&self) -> io::Result<std::collections::HashMap<u32, Vec<u8>>> {
            /// `$Secure`'s record number.
            const SECURE: u64 = 9;
            let cluster = u64::from(self.ntfs_data()?.BytesPerCluster);
            let h = self.raw_reader()?;
            let (real, runs) = self.attribute_runs(&h, SECURE, ntfs::ATTR_DATA, "$SDS")?;
            if real > SDS_MAX {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "$SDS size"));
            }
            let mut sds = Vec::with_capacity(real as usize);
            Self::read_runs(&h, &runs, cluster, real, |b| {
                sds.extend_from_slice(b);
                true
            })?;
            Ok(ntfs::sds_descriptors(&sds))
        }

        fn open_id(&self, frn: u64, access: u32, flags: u32) -> io::Result<Handle> {
            let id = FILE_ID_DESCRIPTOR {
                dwSize: size_of::<FILE_ID_DESCRIPTOR>() as u32,
                Type: FileIdType,
                Anonymous: FILE_ID_DESCRIPTOR_0 { FileId: frn as i64 },
            };
            // SAFETY: the volume handle is open; id is a valid descriptor;
            // null security attributes are allowed.
            let h = unsafe {
                OpenFileById(
                    self.h.raw(),
                    &id,
                    access,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    std::ptr::null(),
                    FILE_FLAG_BACKUP_SEMANTICS | flags,
                )
            };
            if h == INVALID_HANDLE_VALUE {
                Err(io::Error::last_os_error())
            } else {
                Ok(Handle(h))
            }
        }

        /// Journal records from `from`. With `wait`, blocks in the kernel
        /// until at least one record exists (no polling, idle CPU stays 0).
        pub fn read_journal(
            &self,
            from: i64,
            journal_id: u64,
            wait: bool,
        ) -> io::Result<(i64, Vec<UsnRecord>)> {
            let mut buf = vec![0u8; 64 << 10];
            let input = READ_USN_JOURNAL_DATA_V0 {
                StartUsn: from,
                ReasonMask: 0xFFFF_FFFF,
                ReturnOnlyOnClose: 0,
                Timeout: 0,
                BytesToWaitFor: u64::from(wait),
                UsnJournalID: journal_id,
            };
            let n = ioctl(&self.h, FSCTL_READ_USN_JOURNAL, Some(&input), &mut buf)?;
            parse_read_output(&buf[..n])
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e:?}")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved(journal_id: u64, next_usn: i64) -> VolumeSnapshot {
        VolumeSnapshot {
            drive: "C:".into(),
            volume_serial: 1,
            journal_id,
            next_usn,
            root_frn: 5,
            records: vec![],
            cut: Default::default(),
        }
    }

    #[test]
    fn journal_reset_forces_rescan() {
        let now = JournalInfo {
            id: 8,
            first_usn: 0,
            next_usn: 1000,
        };
        assert!(matches!(
            replay_decision(&saved(7, 500), &now),
            Replay::Rescan(_)
        ));
    }

    #[test]
    fn journal_wrapped_forces_rescan() {
        let now = JournalInfo {
            id: 7,
            first_usn: 600,
            next_usn: 1000,
        };
        assert!(matches!(
            replay_decision(&saved(7, 500), &now),
            Replay::Rescan(_)
        ));
    }

    #[test]
    fn snapshot_ahead_of_journal_forces_rescan() {
        let now = JournalInfo {
            id: 7,
            first_usn: 0,
            next_usn: 400,
        };
        assert!(matches!(
            replay_decision(&saved(7, 500), &now),
            Replay::Rescan(_)
        ));
    }

    #[test]
    fn normal_gap_replays_from_saved() {
        let now = JournalInfo {
            id: 7,
            first_usn: 100,
            next_usn: 1000,
        };
        assert_eq!(replay_decision(&saved(7, 500), &now), Replay::From(500));
        assert_eq!(
            replay_decision(&saved(7, 100), &now),
            Replay::From(100),
            "exactly at the oldest record"
        );
        assert_eq!(
            replay_decision(&saved(7, 1000), &now),
            Replay::From(1000),
            "nothing changed"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_folder_lists_its_entries_with_their_ids() {
        use super::os::*;
        let dir = std::env::temp_dir().join(format!("flint-list-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Alt Klasör")).unwrap();
        std::fs::write(dir.join("not.txt"), b"x").unwrap();
        // enough entries for more than one buffer
        for i in 0..900 {
            std::fs::write(dir.join(format!("dosya-uzun-bir-ad-{i:04}.txt")), b"").unwrap();
        }
        let r = list_dir_at(&dir).unwrap();
        assert_eq!(r.len(), 902);
        let sub = r.iter().find(|r| r.name == "Alt Klasör").unwrap();
        assert!(sub.is_dir() && sub.parent == 7);
        assert_eq!(sub.frn, file_id(&dir.join("Alt Klasör")).unwrap());
        assert!(
            r.iter()
                .any(|r| r.name == "not.txt" && !r.is_dir() && r.time > 0)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A link to a folder is not followed: none of its target's entries.
    #[cfg(windows)]
    #[test]
    fn a_folder_link_is_not_followed() {
        use super::os::*;
        let dir = std::env::temp_dir().join(format!("flint-junction-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("hedef")).unwrap();
        std::fs::write(dir.join("hedef").join("gizli.txt"), b"x").unwrap();
        let link = dir.join("bag");
        let made = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(dir.join("hedef"))
            .output()
            .unwrap();
        assert!(made.status.success(), "{made:?}");
        let r = list_dir_at(&link).unwrap_or_default();
        assert!(!r.iter().any(|r| r.name == "gizli.txt"), "{r:?}");
        let _ = std::fs::remove_dir(&link);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Elevated only (CI runs so): every folder's security id and the
    /// descriptors come from the volume without opening a folder, and
    /// decide as the opened root folder does.
    #[cfg(windows)]
    #[test]
    fn folder_security_ids_and_descriptors_are_read_from_the_volume() {
        use super::os::*;
        if !is_elevated() {
            return;
        }
        let _ = enable_privilege("SeBackupPrivilege");
        let v = Volume::open("C:").unwrap();
        let root = root_frn("C:").unwrap();
        let number = root & 0xFFFF_FFFF_FFFF;
        let rec = v.file_record(number).unwrap();
        assert_eq!(crate::ntfs::frn_of(&rec, number), Some(root));
        let sid = crate::ntfs::folder_security_id(&rec).expect("the root's security id");
        let ids = v
            .folder_security_ids(&std::sync::atomic::AtomicBool::new(false))
            .unwrap();
        assert!(ids.len() > 100, "{}", ids.len());
        assert!(
            ids.windows(2).all(|w| w[0].0 < w[1].0),
            "sorted, no duplicates"
        );
        let at = ids
            .binary_search_by_key(&root, |x| x.0)
            .expect("the root is listed");
        assert_eq!(ids[at].1, sid);
        let d = v.security_descriptors().unwrap();
        assert!(
            ids.iter().all(|x| d.contains_key(&x.1)),
            "every id has a descriptor"
        );
        let mut c = crate::privacy::os::AccessChecker::new().unwrap();
        assert_eq!(
            c.check_descriptor(&d[&sid]).unwrap(),
            c.check_path(std::path::Path::new(r"C:\")).unwrap()
        );
        assert_eq!(
            v.file_record(0x00FF_FFFF_FFFF).map_err(|e| e.kind()).err(),
            Some(io::ErrorKind::NotFound),
            "past the MFT"
        );
        let stopped = v.folder_security_ids(&std::sync::atomic::AtomicBool::new(true));
        assert_eq!(
            stopped.map_err(|e| e.kind()).err(),
            Some(io::ErrorKind::Interrupted)
        );
    }

    /// Elevated only: a folder whose permissions changed a moment ago
    /// (its MFT record maybe not on disk yet, the same sectors read raw
    /// just before) shows its new security id in the raw MFT read.
    #[cfg(windows)]
    #[test]
    fn a_permission_change_a_moment_ago_is_seen_by_the_raw_read() {
        use super::os::*;
        use std::sync::atomic::AtomicBool;
        if !is_elevated() {
            return;
        }
        let _ = enable_privilege("SeBackupPrivilege");
        let base = std::env::temp_dir().join(format!("flint-raw-{}", std::process::id()));
        assert!(
            base.to_string_lossy().to_uppercase().starts_with("C:"),
            "{base:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let v = Volume::open("C:").unwrap();
        let me = crate::pipe::current_user_sid().unwrap();
        let id_in = |ids: &[(u64, u32)], frn: u64| {
            ids.binary_search_by_key(&frn, |x| x.0)
                .ok()
                .map(|i| ids[i].1)
        };
        for round in 0..5u32 {
            let dir = base.join(format!("k{round}"));
            std::fs::create_dir(&dir).unwrap();
            let frn = file_id(&dir).unwrap();
            // the sectors are read raw once before the change
            let _ = v.folder_security_ids(&AtomicBool::new(false)).unwrap();
            let owner = crate::privacy::os::descriptor_bytes(&dir).unwrap();
            let _ = owner;
            let unique = format!(
                "S-1-5-21-{}-{}-{round}-1001",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .subsec_nanos()
            );
            let sddl = format!("O:{me}D:P(A;OICI;FA;;;{me})(A;OICI;0x1200a9;;;{unique})");
            crate::privacy::os::secure_dir(&dir, &sddl, &[&me, "S-1-5-32-544", "S-1-5-18"])
                .unwrap();
            let want = crate::ntfs::folder_security_id(&v.file_record(frn).unwrap()).unwrap();
            let ids = v.folder_security_ids(&AtomicBool::new(false)).unwrap();
            assert_eq!(id_in(&ids, frn), Some(want), "round {round}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(windows)]
    #[test]
    fn root_frn_and_volume_info_work_unelevated() {
        let (fs, _) = volume_info("C:").unwrap();
        assert!(!fs.is_empty());
        assert_ne!(root_frn("C:").unwrap(), 0);
    }
}
