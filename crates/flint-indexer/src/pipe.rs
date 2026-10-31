//! Named-pipe transport (Windows): overlapped duplex connections, the
//! accept loop with a restrictive ACL, the caller's SID by impersonation,
//! and the client used by the CLI and the app.

use crate::protocol::{self, ClientMsg, MAX_FRAME, PROTOCOL_VERSION, ServerMsg};
use crate::server::Conn;
use crate::volume::Handle;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread::JoinHandle;
use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_PIPE_BUSY,
    ERROR_PIPE_CONNECTED, ERROR_PIPE_NOT_CONNECTED, GENERIC_READ, GetLastError, HANDLE, HLOCAL,
    INVALID_HANDLE_VALUE, LocalFree, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_KERNEL_OBJECT,
};
use windows_sys::Win32::Security::{
    ACE_HEADER, ACL, GetAce, GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation,
    LABEL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    RevertToSelf, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_WRITE_DATA,
    OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
    WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, ImpersonateNamedPipeClient, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    WaitNamedPipeW,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, GetCurrentProcess, GetCurrentThread, INFINITE, OpenProcessToken, OpenThreadToken,
    SetEvent, WaitForMultipleObjects, WaitForSingleObject,
};

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s).encode_wide().chain([0]).collect()
}

fn event() -> io::Result<Handle> {
    // SAFETY: default security, manual reset, initially unset, unnamed.
    let h = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if h.is_null() {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: a new event handle, owned here only.
        Ok(unsafe { Handle::from_raw(h) })
    }
}

/// `FILE_CREATE_PIPE_INSTANCE` (= `FILE_APPEND_DATA`): the right to add
/// a server instance to an existing pipe. `GENERIC_WRITE` includes it.
pub const FILE_CREATE_PIPE_INSTANCE: u32 = 0x4;

/// What interactive users may do on the service pipe: read and write
/// data, read attributes and the security descriptor, wait. Not
/// `GENERIC_WRITE`: that includes creating pipe instances, which would let
/// any user attach a fake server to the live pipe.
pub const CLIENT_ACCESS: u32 = 0x0012_008B;

/// The SID the service runs as.
pub const SYSTEM_SID: &str = "S-1-5-18";

/// Pipe security for the service: owned by SYSTEM (clients check this),
/// SYSTEM full control, interactive users [`CLIENT_ACCESS`], network
/// logons denied (on top of rejecting remote clients).
pub fn service_sddl() -> String {
    format!("O:SYD:P(D;;GA;;;NU)(A;;GA;;;SY)(A;;{CLIENT_ACCESS:#010x};;;IU)")
}

/// Pipe security for a user-mode indexer: owned by and open to that user
/// only (and SYSTEM).
pub fn user_sddl(sid: &str) -> String {
    format!("O:{sid}D:P(D;;GA;;;NU)(A;;GA;;;SY)(A;;GA;;;{sid})")
}

/// The pipe a user-mode indexer serves on (one per user, so two users'
/// indexers never compete for one name).
pub fn user_pipe_name(sid: &str) -> String {
    format!(r"\\.\pipe\flint-index-{sid}")
}

/// SID string of a SID pointer.
pub(crate) fn sid_string(sid: windows_sys::Win32::Security::PSID) -> io::Result<String> {
    let mut s: *mut u16 = std::ptr::null_mut();
    // SAFETY: sid is a valid SID for the duration of the call; s receives
    // a LocalAlloc'd string.
    if unsafe { ConvertSidToStringSidW(sid, &mut s) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: s is a NUL-terminated wide string from ConvertSidToStringSidW.
    let out = unsafe {
        let n = (0..).take_while(|&i| *s.add(i) != 0).count();
        String::from_utf16_lossy(std::slice::from_raw_parts(s, n))
    };
    // SAFETY: s was allocated by LocalAlloc inside ConvertSidToStringSidW.
    unsafe { LocalFree(s as HLOCAL) };
    Ok(out)
}

/// SID string of the owner of the pipe `h` is connected to (the account
/// that created it: an impostor cannot make a pipe owned by SYSTEM or by
/// another user).
fn pipe_owner(h: HANDLE) -> io::Result<String> {
    let mut owner: windows_sys::Win32::Security::PSID = std::ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: h is an open pipe handle with READ_CONTROL; owner points
    // into sd, which is LocalAlloc'd and freed below.
    let r = unsafe {
        GetSecurityInfo(
            h,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sd,
        )
    };
    if r != 0 {
        return Err(io::Error::from_raw_os_error(r as i32));
    }
    let sd = Sddl(sd);
    let out = sid_string(owner);
    drop(sd);
    out
}

/// `SYSTEM_MANDATORY_LABEL_ACE_TYPE`.
const MANDATORY_LABEL_ACE: u8 = 0x11;

/// `SECURITY_MANDATORY_MEDIUM_RID`: a normal process's integrity level.
pub const MEDIUM_INTEGRITY: u32 = 0x2000;

/// Integrity level (the mandatory label's RID) of the pipe `h` is
/// connected to; `None` when it has no label (Windows treats that as
/// medium).
fn pipe_integrity(h: HANDLE) -> io::Result<Option<u32>> {
    let mut sacl: *mut ACL = std::ptr::null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: h is an open pipe handle with READ_CONTROL; sacl points into
    // sd, which is LocalAlloc'd and freed when `_sd` drops, after its use.
    let r = unsafe {
        GetSecurityInfo(
            h,
            SE_KERNEL_OBJECT,
            LABEL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sacl,
            &mut sd,
        )
    };
    if r != 0 {
        return Err(io::Error::from_raw_os_error(r as i32));
    }
    let _sd = Sddl(sd);
    if sacl.is_null() {
        return Ok(None);
    }
    // SAFETY: sacl is a valid ACL inside sd.
    let count = unsafe { (*sacl).AceCount };
    for i in 0..u32::from(count) {
        let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
        // SAFETY: i < AceCount; ace receives a pointer into the ACL.
        if unsafe { GetAce(sacl, i, &mut ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: every ACE starts with an ACE_HEADER.
        if unsafe { (*(ace as *const ACE_HEADER)).AceType } != MANDATORY_LABEL_ACE {
            continue;
        }
        // SAFETY: a SYSTEM_MANDATORY_LABEL_ACE is header (4), mask (4),
        // then the label SID; GetSidSubAuthority* read inside that SID.
        let rid = unsafe {
            let sid = (ace as *mut u8).add(8) as PSID;
            match *GetSidSubAuthorityCount(sid) {
                0 => 0,
                n => *GetSidSubAuthority(sid, u32::from(n) - 1),
            }
        };
        return Ok(Some(rid));
    }
    Ok(None)
}

/// SID string of a token's user.
fn token_sid(token: HANDLE) -> io::Result<String> {
    let mut len = 0u32;
    // SAFETY: size query with a null buffer; len receives the size.
    unsafe { GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len) };
    if len == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: buf is 8-aligned and at least len bytes.
    if unsafe { GetTokenInformation(token, TokenUser, buf.as_mut_ptr() as *mut _, len, &mut len) }
        == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the kernel wrote a TOKEN_USER at the start of buf, which
    // stays alive across sid_string.
    let sid = unsafe { (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid };
    sid_string(sid)
}

/// SID string of the user running this process.
pub fn current_user_sid() -> io::Result<String> {
    let mut t: HANDLE = std::ptr::null_mut();
    // SAFETY: pseudo handle of this process; t receives a new token handle.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut t) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a new token handle, owned here only.
    token_sid(unsafe { Handle::from_raw(t) }.raw())
}

/// One end of a pipe, opened for overlapped I/O so a read and a write can
/// be pending at once from different threads.
pub struct PipeConn {
    h: Handle,
    rd: Mutex<Handle>,
    wr: Mutex<Handle>,
    /// Read timeout in ms (`INFINITE` = none).
    read_timeout: AtomicU32,
    /// Set by [`PipeConn::cancel`]: no further read or write starts.
    closed: AtomicBool,
}

fn closed_err() -> io::Error {
    io::Error::new(
        io::ErrorKind::ConnectionAborted,
        "the connection was closed",
    )
}

fn io_err_eof(e: io::Error) -> io::Error {
    match e.raw_os_error() {
        Some(c) if c == ERROR_BROKEN_PIPE as i32 || c == ERROR_PIPE_NOT_CONNECTED as i32 => {
            io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the other side closed the pipe",
            )
        }
        _ => e,
    }
}

impl PipeConn {
    fn new(h: Handle) -> io::Result<PipeConn> {
        Ok(PipeConn {
            h,
            rd: Mutex::new(event()?),
            wr: Mutex::new(event()?),
            read_timeout: AtomicU32::new(INFINITE),
            closed: AtomicBool::new(false),
        })
    }

    /// One overlapped ReadFile/WriteFile, waited for before returning (so
    /// the OVERLAPPED and the buffer outlive the request).
    fn transfer(&self, ev: &Handle, read: bool, ptr: *mut u8, len: u32) -> io::Result<usize> {
        if self.closed.load(Ordering::SeqCst) {
            return Err(closed_err());
        }
        // SAFETY: zeroed OVERLAPPED is valid; its event is ours.
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = ev.raw();
        // SAFETY: ptr/len describe a live buffer that stays put until
        // GetOverlappedResult(wait = TRUE) below returns.
        let ok = unsafe {
            if read {
                ReadFile(self.h.raw(), ptr, len, std::ptr::null_mut(), &mut ov)
            } else {
                WriteFile(self.h.raw(), ptr, len, std::ptr::null_mut(), &mut ov)
            }
        };
        // SAFETY: reads this thread's last error code.
        if ok == 0 && unsafe { GetLastError() } != ERROR_IO_PENDING {
            return Err(io_err_eof(io::Error::last_os_error()));
        }
        let mut n = 0u32;
        // a cancel between the check above and this request found nothing
        // in flight: it is seen here, and this request is cancelled
        if self.closed.load(Ordering::SeqCst) {
            // SAFETY: cancels the request issued above, then waits for it to
            // end, so the buffer and OVERLAPPED outlive it.
            unsafe {
                CancelIoEx(self.h.raw(), &ov);
                GetOverlappedResult(self.h.raw(), &ov, &mut n, 1);
            }
            return Err(closed_err());
        }
        let limit = if read {
            self.read_timeout.load(Ordering::Relaxed)
        } else {
            INFINITE
        };
        // SAFETY: waits on our own event for the request issued above.
        if limit != INFINITE && unsafe { WaitForSingleObject(ev.raw(), limit) } == WAIT_TIMEOUT {
            // SAFETY: cancels that request, then waits for it to end, so the
            // buffer and OVERLAPPED outlive it.
            unsafe {
                CancelIoEx(self.h.raw(), &ov);
                GetOverlappedResult(self.h.raw(), &ov, &mut n, 1);
            }
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the other side sent nothing in time",
            ));
        }
        // SAFETY: waits for the request issued above on this handle.
        if unsafe { GetOverlappedResult(self.h.raw(), &ov, &mut n, 1) } == 0 {
            return Err(io_err_eof(io::Error::last_os_error()));
        }
        Ok(n as usize)
    }

    fn read_exact(&self, ev: &Handle, buf: &mut [u8]) -> io::Result<()> {
        let mut at = 0;
        while at < buf.len() {
            let n = self.transfer(ev, true, buf[at..].as_mut_ptr(), (buf.len() - at) as u32)?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the other side closed the pipe",
                ));
            }
            at += n;
        }
        Ok(())
    }

    /// Closes the connection: every pending read/write ends with an error
    /// and no later one starts (also when nothing was in flight).
    pub fn cancel(&self) {
        // set before the cancel, so a request issued after it sees the flag
        self.closed.store(true, Ordering::SeqCst);
        // SAFETY: cancels I/O on our own handle; null = every request.
        unsafe { CancelIoEx(self.h.raw(), std::ptr::null()) };
    }

    pub fn send(&self, m: &impl serde::Serialize) -> io::Result<()> {
        self.write_frame(&serde_json::to_vec(m).map_err(io::Error::other)?)
    }

    pub fn recv<M: for<'de> serde::Deserialize<'de>>(&self) -> io::Result<M> {
        let f = self.read_frame()?;
        serde_json::from_slice(&f).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    /// Writes raw bytes (tests send malformed frames with it).
    pub fn write_raw(&self, bytes: &[u8]) -> io::Result<()> {
        let ev = self.wr.lock().map_err(|_| io::Error::other("poisoned"))?;
        let mut at = 0;
        while at < bytes.len() {
            at += self.transfer(
                &ev,
                false,
                bytes[at..].as_ptr() as *mut u8,
                (bytes.len() - at) as u32,
            )?;
        }
        Ok(())
    }
}

/// Reverts the thread's impersonation when dropped. A failed revert would
/// leave the thread running as the client: the process ends instead.
struct Revert;
impl Drop for Revert {
    fn drop(&mut self) {
        // SAFETY: ends impersonation on this thread.
        if unsafe { RevertToSelf() } == 0 {
            crate::log::line("RevertToSelf failed; stopping to avoid running as the client");
            std::process::abort();
        }
    }
}

impl Conn for PipeConn {
    fn read_frame(&self) -> io::Result<Vec<u8>> {
        let ev = self.rd.lock().map_err(|_| io::Error::other("poisoned"))?;
        let mut len = [0u8; 4];
        self.read_exact(&ev, &mut len)?;
        let n = u32::from_le_bytes(len) as usize;
        if n > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("frame of {n} bytes exceeds the limit"),
            ));
        }
        let mut buf = vec![0u8; n];
        self.read_exact(&ev, &mut buf)?;
        Ok(buf)
    }

    fn write_frame(&self, payload: &[u8]) -> io::Result<()> {
        let mut framed = Vec::new();
        protocol::write_frame(&mut framed, payload)?;
        self.write_raw(&framed)
    }

    fn client_sid(&self) -> io::Result<String> {
        // SAFETY: the server end of a pipe that has read the client's first
        // message; the guard reverts on every path out of this function.
        if unsafe { ImpersonateNamedPipeClient(self.h.raw()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let _revert = Revert;
        let mut t: HANDLE = std::ptr::null_mut();
        // SAFETY: opens the impersonation token of this thread, checked
        // against the process's own rights (OpenAsSelf = TRUE).
        if unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut t) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a new token handle, owned here only.
        token_sid(unsafe { Handle::from_raw(t) }.raw())
    }

    fn close(&self) {
        self.cancel();
    }

    fn set_read_timeout(&self, ms: Option<u32>) {
        self.read_timeout
            .store(ms.unwrap_or(INFINITE), Ordering::Relaxed);
    }
}

/// A running pipe server; dropping it stops accepting and wakes every
/// open connection so their threads end.
pub struct ServerHandle {
    stop: Arc<Handle>,
    thread: Option<JoinHandle<()>>,
    conns: Arc<Mutex<Vec<Weak<PipeConn>>>>,
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        // SAFETY: stop is a live event owned by this server.
        unsafe { SetEvent(self.stop.raw()) };
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if let Ok(conns) = self.conns.lock() {
            for c in conns.iter().filter_map(Weak::upgrade) {
                c.cancel();
            }
        }
    }
}

/// A LocalAlloc'd security descriptor, freed on drop.
pub(crate) struct Sddl(pub(crate) PSECURITY_DESCRIPTOR);
impl Drop for Sddl {
    fn drop(&mut self) {
        // SAFETY: LocalAlloc'd by ConvertStringSecurityDescriptorToSecurityDescriptorW
        // or GetSecurityInfo, the two places that make an Sddl; freed once.
        unsafe { LocalFree(self.0 as HLOCAL) };
    }
}

/// The security descriptor an SDDL string describes.
pub(crate) fn parse_sddl(sddl: &str) -> io::Result<Sddl> {
    let s = wide(sddl);
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: s is NUL-terminated; sd receives a LocalAlloc'd descriptor.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            s.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(Sddl(sd))
}

fn create_instance(name: &[u16], sddl: &str, first: bool) -> io::Result<Handle> {
    let sd = parse_sddl(sddl)?;
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0,
        bInheritHandle: 0,
    };
    let flags = PIPE_ACCESS_DUPLEX
        | FILE_FLAG_OVERLAPPED
        | if first {
            FILE_FLAG_FIRST_PIPE_INSTANCE
        } else {
            0
        };
    // SAFETY: name is NUL-terminated; sa and its descriptor live across the call.
    let h = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            flags,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            64 << 10,
            64 << 10,
            0,
            &sa,
        )
    };
    if h == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: a new pipe instance handle, owned here only.
        Ok(unsafe { Handle::from_raw(h) })
    }
}

/// Creates a server instance of `name` and drops it (tests: proves an
/// ACL refuses instance creation, or, with `first`, that the name is
/// taken).
#[doc(hidden)]
pub fn add_instance_for_test(name: &str, sddl: &str, first: bool) -> io::Result<()> {
    create_instance(&wide(name), sddl, first).map(drop)
}

enum NextErr {
    /// The pipe belongs to another account now: stop serving.
    Foreign(String),
    /// Could not create an instance (try again later).
    Io(io::Error),
}

/// A further instance of our pipe, refused when the pipe is no longer
/// ours (`owner`: the first instance's owner): the server stops rather
/// than serve on a pipe another account created.
fn next_instance(name: &[u16], sddl: &str, owner: &str) -> Result<Handle, NextErr> {
    let h = create_instance(name, sddl, false).map_err(NextErr::Io)?;
    let o = pipe_owner(h.raw()).map_err(NextErr::Io)?;
    if !o.eq_ignore_ascii_case(owner) {
        return Err(NextErr::Foreign(format!(
            "the pipe now belongs to {o}, not {owner}"
        )));
    }
    Ok(h)
}

/// Starts serving `name`; each connection runs `handler` on its own
/// thread. Fails at once when another process already owns the name.
pub fn serve(
    name: &str,
    sddl: &str,
    handler: Arc<dyn Fn(Arc<PipeConn>) + Send + Sync>,
) -> io::Result<ServerHandle> {
    let wname = wide(name);
    let first = create_instance(&wname, sddl, true).map_err(|e| {
        if e.raw_os_error() == Some(ERROR_ACCESS_DENIED as i32) {
            let m = format!("the pipe {name} is already held by another process (another indexer, or an impostor)");
            crate::log::line(&m);
            io::Error::new(io::ErrorKind::AddrInUse, m)
        } else {
            e
        }
    })?;
    let owner = pipe_owner(first.raw())?;
    let stop = Arc::new(event()?);
    let conns: Arc<Mutex<Vec<Weak<PipeConn>>>> = Arc::default();
    let (stop2, conns2, sddl) = (stop.clone(), conns.clone(), sddl.to_string());
    let thread = std::thread::Builder::new()
        .name("flint-pipe-accept".into())
        .spawn(move || {
            let connect_ev = match event() {
                Ok(e) => e,
                Err(e) => {
                    crate::log::line(format!("pipe server cannot start: {e}"));
                    return;
                }
            };
            let mut next = Some(first);
            loop {
                let inst = match next.take() {
                    Some(i) => i,
                    None => match next_instance(&wname, &sddl, &owner) {
                        Ok(i) => i,
                        Err(NextErr::Foreign(e)) => {
                            crate::log::line(format!("pipe server stopped: {e}"));
                            return;
                        }
                        Err(NextErr::Io(e)) => {
                            crate::log::line(format!("cannot create a pipe instance: {e}"));
                            // SAFETY: waits on our stop event with a timeout.
                            let w = unsafe { WaitForMultipleObjects(1, &stop2.raw(), 0, 1000) };
                            if w == WAIT_OBJECT_0 {
                                return;
                            }
                            continue;
                        }
                    },
                };
                // SAFETY: zeroed OVERLAPPED is valid; event is ours.
                let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
                ov.hEvent = connect_ev.raw();
                // SAFETY: inst is a fresh overlapped pipe instance; ov stays
                // alive until the request completes or is cancelled and waited.
                let ok = unsafe { ConnectNamedPipe(inst.raw(), &mut ov) };
                // SAFETY: reads this thread's last error code.
                let err = if ok == 0 {
                    unsafe { GetLastError() }
                } else {
                    0
                };
                let connected = if err == ERROR_PIPE_CONNECTED {
                    true
                } else if err == ERROR_IO_PENDING {
                    let hs: [HANDLE; 2] = [connect_ev.raw(), stop2.raw()];
                    // SAFETY: two live events.
                    let w = unsafe { WaitForMultipleObjects(2, hs.as_ptr(), 0, INFINITE) };
                    let mut n = 0u32;
                    if w == WAIT_FAILED {
                        crate::log::line(format!(
                            "pipe server stopped: wait failed: {}",
                            io::Error::last_os_error()
                        ));
                    }
                    if w != WAIT_OBJECT_0 {
                        // SAFETY: cancel our pending connect, then wait for it.
                        unsafe {
                            CancelIoEx(inst.raw(), &ov);
                            GetOverlappedResult(inst.raw(), &ov, &mut n, 1);
                        }
                        return;
                    }
                    // SAFETY: the connect completed.
                    unsafe { GetOverlappedResult(inst.raw(), &ov, &mut n, 0) != 0 }
                } else {
                    false
                };
                // the next instance exists before this one is dropped or
                // handed off: the name is never free for another process
                // to create
                match next_instance(&wname, &sddl, &owner) {
                    Ok(i) => next = Some(i),
                    Err(NextErr::Foreign(e)) => {
                        crate::log::line(format!("pipe server stopped: {e}"));
                        return;
                    }
                    Err(NextErr::Io(e)) => {
                        crate::log::line(format!("cannot create a pipe instance: {e}"))
                    }
                }
                if !connected {
                    continue; // the client left already; drop the instance
                }
                let conn = match PipeConn::new(inst) {
                    Ok(c) => Arc::new(c),
                    Err(e) => {
                        crate::log::line(format!("pipe connection setup failed: {e}"));
                        continue;
                    }
                };
                let open = match conns2.lock() {
                    Ok(mut list) => {
                        list.retain(|w| w.strong_count() > 0);
                        list.push(Arc::downgrade(&conn));
                        list.len()
                    }
                    Err(_) => 0,
                };
                if open > crate::server::MAX_CONNECTIONS {
                    let _ = conn.send(&ServerMsg::Error {
                        code: "busy".into(),
                        message: format!(
                            "the indexer has {} connections open",
                            crate::server::MAX_CONNECTIONS
                        ),
                    });
                    continue;
                }
                let handler = handler.clone();
                if let Err(e) = std::thread::Builder::new()
                    .name("flint-pipe-conn".into())
                    .spawn(move || handler(conn))
                {
                    crate::log::line(format!("cannot start a connection thread: {e}"));
                }
            }
        })?;
    Ok(ServerHandle {
        stop,
        thread: Some(thread),
        conns,
    })
}

/// Opens the client end of `name` with identification-level impersonation
/// (the server may read who we are, never act as us).
pub fn connect_raw(name: &str) -> io::Result<PipeConn> {
    connect_raw_waiting(name, 5, 2000)
}

/// [`connect_raw`] that waits for a busy pipe `tries` times, up to
/// `wait_ms` each, and tries again after each wait.
fn connect_raw_waiting(name: &str, tries: u32, wait_ms: u32) -> io::Result<PipeConn> {
    let w = wide(name);
    for attempt in 0..=tries {
        // SAFETY: w is NUL-terminated; null security attributes/template.
        let h = unsafe {
            CreateFileW(
                w.as_ptr(),
                // not GENERIC_WRITE: the service grants clients only
                // CLIENT_ACCESS (no FILE_CREATE_PIPE_INSTANCE)
                GENERIC_READ | FILE_WRITE_DATA,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED | SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                std::ptr::null_mut(),
            )
        };
        if h != INVALID_HANDLE_VALUE {
            // SAFETY: a new client pipe handle, owned here only.
            return PipeConn::new(unsafe { Handle::from_raw(h) });
        }
        let e = io::Error::last_os_error();
        if e.raw_os_error() != Some(ERROR_PIPE_BUSY as i32) {
            return Err(e);
        }
        if attempt == tries {
            break;
        }
        // SAFETY: w is NUL-terminated; waits up to wait_ms for a free instance.
        unsafe { WaitNamedPipeW(w.as_ptr(), wait_ms) };
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "the indexer is busy",
    ))
}

/// A connected, greeted client.
pub struct Client {
    pub conn: PipeConn,
    pub mode: String,
    seq: AtomicU64,
}

impl Client {
    /// The service's pipe, which must be owned by SYSTEM.
    pub fn connect_service() -> io::Result<Client> {
        Client::connect(crate::protocol::PIPE_NAME, SYSTEM_SID)
    }

    /// The service, else this user's user-mode indexer (the order the
    /// app and the CLI use).
    pub fn connect_any() -> io::Result<Client> {
        Client::connect_service().or_else(|service_err| {
            Client::connect_user().map_err(|user_err| {
                io::Error::other(format!(
                    "no indexer is running (service: {service_err}; user mode: {user_err})"
                ))
            })
        })
    }

    /// This user's user-mode indexer, whose pipe must be owned by this
    /// user.
    pub fn connect_user() -> io::Result<Client> {
        let me = current_user_sid()?;
        Client::connect(&user_pipe_name(&me), &me)
    }

    /// Connects, checks that the pipe is owned by `expected_owner` (so a
    /// process that grabbed the name first is refused before it sees a
    /// single query) and exchanges `Hello`; a version
    /// mismatch is an error naming both versions.
    pub fn connect(name: &str, expected_owner: &str) -> io::Result<Client> {
        Client::open(connect_raw(name)?, name, expected_owner)
    }

    /// [`Client::connect`] that gives up on a pipe that stays busy, or does
    /// not greet, within about `ms` (each read after that waits `ms` too).
    pub fn connect_within(name: &str, expected_owner: &str, ms: u32) -> io::Result<Client> {
        let conn = connect_raw_waiting(name, 1, ms)?;
        crate::server::Conn::set_read_timeout(&conn, Some(ms));
        Client::open(conn, name, expected_owner)
    }

    fn open(conn: PipeConn, name: &str, expected_owner: &str) -> io::Result<Client> {
        let owner = pipe_owner(conn.h.raw())?;
        if !owner.eq_ignore_ascii_case(expected_owner) {
            crate::log::line(format!(
                "refused {name}: owned by {owner}, expected {expected_owner}"
            ));
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{name} is not served by the expected account (owner {owner}); not connecting"
                ),
            ));
        }
        // a sandboxed process of the same account would pass the owner
        // check; it runs below medium integrity
        if let Some(level) = pipe_integrity(conn.h.raw())?
            && level < MEDIUM_INTEGRITY
        {
            crate::log::line(format!("refused {name}: integrity level {level:#x}"));
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{name} was created at low integrity ({level:#x}), by a sandboxed process; not connecting"
                ),
            ));
        }
        conn.send(&ClientMsg::Hello {
            version: PROTOCOL_VERSION,
        })?;
        match conn.recv::<ServerMsg>()? {
            ServerMsg::Welcome { mode, .. } => Ok(Client {
                conn,
                mode,
                seq: AtomicU64::new(0),
            }),
            ServerMsg::Error { code, message } => {
                Err(io::Error::other(format!("{code}: {message}")))
            }
            other => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected reply {other:?}"),
            )),
        }
    }

    /// Sends a query without waiting; returns its sequence number.
    pub fn send_query(
        &self,
        text: &str,
        limit: u32,
        boosts: Vec<(String, f32)>,
    ) -> io::Result<u64> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        self.conn.send(&ClientMsg::Query {
            seq,
            text: text.into(),
            limit,
            boosts,
        })?;
        Ok(seq)
    }

    /// Next message from the server.
    pub fn recv(&self) -> io::Result<ServerMsg> {
        self.conn.recv()
    }

    /// Sends a query and returns its answer (`Results` or `Cancelled`),
    /// dropping answers to older queries.
    pub fn query(
        &self,
        text: &str,
        limit: u32,
        boosts: Vec<(String, f32)>,
    ) -> io::Result<ServerMsg> {
        let seq = self.send_query(text, limit, boosts)?;
        loop {
            match self.recv()? {
                m @ (ServerMsg::Results { seq: s, .. } | ServerMsg::Cancelled { seq: s })
                    if s == seq =>
                {
                    return Ok(m);
                }
                ServerMsg::Results { .. } | ServerMsg::Cancelled { .. } => {}
                ServerMsg::Error { code, message } => {
                    return Err(io::Error::other(format!("{code}: {message}")));
                }
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unexpected reply {other:?}"),
                    ));
                }
            }
        }
    }

    pub fn status(&self) -> io::Result<ServerMsg> {
        self.conn.send(&ClientMsg::Status)?;
        loop {
            match self.recv()? {
                m @ ServerMsg::StatusReply { .. } => return Ok(m),
                ServerMsg::Results { .. } | ServerMsg::Cancelled { .. } => {}
                ServerMsg::Error { code, message } => {
                    return Err(io::Error::other(format!("{code}: {message}")));
                }
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unexpected reply {other:?}"),
                    ));
                }
            }
        }
    }
}

/// Starts the query server for `shared` on `name` with `sddl` access.
pub fn serve_index(
    name: &str,
    sddl: &str,
    shared: Arc<crate::server::Shared>,
) -> io::Result<ServerHandle> {
    serve(
        name,
        sddl,
        Arc::new(move |c: Arc<PipeConn>| crate::server::handle(c as Arc<dyn Conn>, shared.clone())),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_TYPE_BYTE, PIPE_WAIT,
    };

    /// One instance only: a second client finds the pipe busy until the
    /// first leaves.
    #[test]
    fn a_pipe_busy_for_a_moment_is_connected_once_free() {
        let name = format!(r"\\.\pipe\flint-test-busy-{}", std::process::id());
        let w = wide(&name);
        let (ready, go) = mpsc::channel::<()>();
        let (left, gone) = mpsc::channel::<()>();
        let server = std::thread::spawn(move || {
            // SAFETY: w is NUL-terminated; default security, one instance.
            let h = unsafe {
                CreateNamedPipeW(
                    w.as_ptr(),
                    windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_BYTE | PIPE_WAIT,
                    1,
                    4096,
                    4096,
                    0,
                    std::ptr::null(),
                )
            };
            assert_ne!(h, INVALID_HANDLE_VALUE);
            ready.send(()).unwrap();
            // SAFETY: h is the pipe made above; blocking (not overlapped).
            unsafe { ConnectNamedPipe(h, std::ptr::null_mut()) };
            gone.recv().unwrap();
            // SAFETY: as above.
            unsafe {
                DisconnectNamedPipe(h);
                ConnectNamedPipe(h, std::ptr::null_mut());
            }
            std::thread::sleep(Duration::from_millis(200));
            // SAFETY: owned here only.
            drop(unsafe { Handle::from_raw(h) });
        });
        go.recv().unwrap();
        let first = connect_raw_waiting(&name, 1, 500).expect("the free instance");
        let t = Instant::now();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(first);
            left.send(()).unwrap();
        });
        let second = connect_raw_waiting(&name, 1, 3000);
        assert!(second.is_ok(), "{:?} after {:?}", second.err(), t.elapsed());
        release.join().unwrap();
        server.join().unwrap();
    }
}
