//! Directory watch through `ReadDirectoryChangesW` (overlapped, so a stop
//! request wakes it at once; no polling, idle CPU stays 0).

use crate::walk::FsEvent;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

#[cfg(windows)]
pub use os::*;

/// Pairs rename halves and turns raw notifications into events.
/// `raw` is `(action, path relative to the root)` in arrival order.
pub fn events_from_raw(
    root: &Path,
    raw: &[(u32, String)],
    pending_old: &mut Option<PathBuf>,
) -> Vec<FsEvent> {
    const ADDED: u32 = 1;
    const REMOVED: u32 = 2;
    const MODIFIED: u32 = 3;
    const RENAMED_OLD: u32 = 4;
    const RENAMED_NEW: u32 = 5;
    let mut out = Vec::new();
    for (action, rel) in raw {
        let p = root.join(rel);
        match *action {
            ADDED => out.push(FsEvent::Added(p)),
            REMOVED => out.push(FsEvent::Removed(p)),
            MODIFIED => out.push(FsEvent::Modified(p)),
            RENAMED_OLD => *pending_old = Some(p),
            RENAMED_NEW => match pending_old.take() {
                Some(old) => out.push(FsEvent::Renamed(old, p)),
                None => out.push(FsEvent::Added(p)),
            },
            _ => {}
        }
    }
    out
}

/// Parses a `FILE_NOTIFY_INFORMATION` chain (bounds-checked).
pub fn parse_notify(buf: &[u8]) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + 12 <= buf.len() {
        let u32_at = |o: usize| u32::from_le_bytes(buf[o..o + 4].try_into().expect("4 bytes"));
        let next = u32_at(at) as usize;
        let action = u32_at(at + 4);
        let len = u32_at(at + 8) as usize;
        let start = at + 12;
        if start + len > buf.len() {
            break;
        }
        let units: Vec<u16> = buf[start..start + len]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        out.push((action, String::from_utf16_lossy(&units)));
        if next == 0 {
            break;
        }
        at += next;
    }
    out
}

#[cfg(windows)]
mod os {
    use super::*;
    use crate::volume::Handle;
    use std::os::windows::ffi::OsStrExt;
    use std::thread::JoinHandle;
    use windows_sys::Win32::Foundation::{
        ERROR_NOTIFY_ENUM_DIR, ERROR_OPERATION_ABORTED, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
        FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadDirectoryChangesW,
    };
    use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
    use windows_sys::Win32::System::Threading::{
        CreateEventW, INFINITE, SetEvent, WaitForMultipleObjects,
    };

    fn event() -> io::Result<Handle> {
        // SAFETY: default security, manual reset, initially unset, unnamed.
        let h = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
        if h.is_null() {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: a new event handle, owned here only.
            Ok(unsafe { crate::volume::Handle::from_raw(h) })
        }
    }

    /// A running watch; dropping it stops the thread and waits for it.
    pub struct WatchHandle {
        stop: std::sync::Arc<Handle>,
        thread: Option<JoinHandle<()>>,
    }

    impl Drop for WatchHandle {
        fn drop(&mut self) {
            // SAFETY: stop is a live event handle owned by self.
            unsafe { SetEvent(self.stop.raw()) };
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    /// Watches `root` and its subfolders; events go to `tx` until the
    /// handle is dropped or the receiver hangs up. A watch error that ends
    /// the thread sends `Overflow` first, so the owner re-walks.
    pub fn watch(root: &Path, tx: Sender<FsEvent>) -> io::Result<WatchHandle> {
        let w: Vec<u16> = root.as_os_str().encode_wide().chain([0]).collect();
        // SAFETY: w is NUL-terminated; overlapped directory handle for
        // ReadDirectoryChangesW; null security and template are allowed.
        let h = unsafe {
            CreateFileW(
                w.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a new directory handle, owned here only.
        let dir = unsafe { Handle::from_raw(h) };
        let stop = std::sync::Arc::new(event()?);
        let io_ev = event()?;
        let root = root.to_path_buf();
        let stop2 = stop.clone();
        // Windows records changes only from the first ReadDirectoryChangesW
        // call on: return once it is issued, so no change made after
        // watch() returns can be missed.
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("flint-watch".into())
            .spawn(move || run(dir, io_ev, &stop2, &root, &tx, ready_tx))?;
        let _ = ready_rx.recv(); // also returns if the thread ended early
        Ok(WatchHandle {
            stop,
            thread: Some(thread),
        })
    }

    fn run(
        dir: Handle,
        io_ev: Handle,
        stop: &Handle,
        root: &Path,
        tx: &Sender<FsEvent>,
        ready: Sender<()>,
    ) {
        // DWORD-aligned 64 KiB buffer, as ReadDirectoryChangesW requires
        let mut buf = vec![0u32; 16 * 1024];
        let mut pending_old = None;
        loop {
            // SAFETY: zeroed OVERLAPPED is valid; hEvent set to our event.
            let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
            ov.hEvent = io_ev.raw();
            // SAFETY: dir is an overlapped directory handle; buf and ov stay
            // alive and unmoved until the request completes or is cancelled
            // and waited for below.
            let ok = unsafe {
                ReadDirectoryChangesW(
                    dir.raw(),
                    buf.as_mut_ptr() as *mut _,
                    (buf.len() * 4) as u32,
                    1,
                    FILE_NOTIFY_CHANGE_FILE_NAME
                        | FILE_NOTIFY_CHANGE_DIR_NAME
                        | FILE_NOTIFY_CHANGE_LAST_WRITE,
                    std::ptr::null_mut(),
                    &mut ov,
                    None,
                )
            };
            if ok == 0 {
                let _ = tx.send(FsEvent::Overflow);
                return;
            }
            let _ = ready.send(());
            let handles: [HANDLE; 2] = [io_ev.raw(), stop.raw()];
            // SAFETY: two live event handles.
            let w = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), 0, INFINITE) };
            let mut got = 0u32;
            if w != WAIT_OBJECT_0 {
                // stop: cancel and wait for the request so buf/ov can be freed
                // SAFETY: cancels our own request on dir; then waits for it.
                unsafe {
                    CancelIoEx(dir.raw(), &ov);
                    GetOverlappedResult(dir.raw(), &ov, &mut got, 1);
                }
                return;
            }
            // SAFETY: the request completed (its event is set).
            let ok = unsafe { GetOverlappedResult(dir.raw(), &ov, &mut got, 0) };
            let err = if ok == 0 {
                io::Error::last_os_error().raw_os_error()
            } else {
                None
            };
            if err == Some(ERROR_OPERATION_ABORTED as i32) {
                return;
            }
            if err == Some(ERROR_NOTIFY_ENUM_DIR as i32) || (ok != 0 && got == 0) {
                if tx.send(FsEvent::Overflow).is_err() {
                    return;
                }
                continue;
            }
            if ok == 0 {
                let _ = tx.send(FsEvent::Overflow);
                return;
            }
            // SAFETY: the kernel wrote `got` bytes into buf.
            let bytes =
                unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, got as usize) };
            for ev in events_from_raw(root, &parse_notify(bytes), &mut pending_old) {
                if tx.send(ev).is_err() {
                    return;
                }
            }
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::walk::tests::tmp;
    use std::sync::mpsc::{Receiver, channel};
    use std::time::{Duration, Instant};

    /// Waits (event-driven, with a deadline) until `want` has been seen.
    fn expect(rx: &Receiver<FsEvent>, want: &FsEvent) {
        let end = Instant::now() + Duration::from_secs(5);
        let mut seen = Vec::new();
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(ev) if &ev == want => return,
                Ok(ev) => seen.push(ev),
                Err(_) => break,
            }
        }
        panic!("did not see {want:?}; saw {seen:?}");
    }

    #[test]
    fn watch_reports_create_rename_delete() {
        let d = tmp("watch");
        let (tx, rx) = channel();
        let h = watch(&d, tx).unwrap();
        std::fs::write(d.join("yeni.txt"), b"x").unwrap();
        expect(&rx, &FsEvent::Added(d.join("yeni.txt")));
        std::fs::rename(d.join("yeni.txt"), d.join("Bütçe.txt")).unwrap();
        expect(
            &rx,
            &FsEvent::Renamed(d.join("yeni.txt"), d.join("Bütçe.txt")),
        );
        std::fs::create_dir(d.join("alt")).unwrap();
        std::fs::write(d.join(r"alt\ic.md"), b"x").unwrap();
        expect(&rx, &FsEvent::Added(d.join(r"alt\ic.md")));
        std::fs::remove_file(d.join("Bütçe.txt")).unwrap();
        expect(&rx, &FsEvent::Removed(d.join("Bütçe.txt")));
        let t = Instant::now();
        drop(h);
        assert!(t.elapsed() < Duration::from_secs(2), "stop is prompt");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn parse_notify_is_bounds_checked() {
        assert!(parse_notify(&[1, 2, 3]).is_empty());
        let mut b = vec![0u8; 12];
        b[4] = 1;
        b[8] = 200; // name length past the end
        assert!(parse_notify(&b).is_empty());
    }

    #[test]
    fn rename_halves_pair_across_buffers() {
        let root = Path::new(r"C:\r");
        let mut pending = None;
        assert!(events_from_raw(root, &[(4, "a.txt".into())], &mut pending).is_empty());
        let ev = events_from_raw(root, &[(5, "b.txt".into())], &mut pending);
        assert_eq!(
            ev,
            vec![FsEvent::Renamed(root.join("a.txt"), root.join("b.txt"))]
        );
    }
}
