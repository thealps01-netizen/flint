//! Per-user privacy: which folders belong to which
//! Windows account, so the service never shows one user another user's
//! profile, recycle bin or system-only folders.
//!
//! Expressed as name-based owner rules (`flint_core::OwnerRule`), so
//! folders created later are covered at once:
//! - the profiles folder (`C:\Users`): each profile's folder belongs to
//!   its account, `Public` is public, any other folder (a deleted
//!   account, `Default`, a profile not yet in the list) belongs to nobody;
//! - a profile outside the profiles folder: that folder only;
//! - `<drive>\$Recycle.Bin\<SID>`: that account; unknown SIDs: nobody;
//! - `<drive>\System Volume Information`, `<Windows>\CSC`,
//!   `<drive>\Windows.old\Users\*` and the indexer's orphans folder
//!   (records whose parent is unknown): nobody.
//!
//! Owner numbers come from an append-only [`OwnerRegistry`]: a number is
//! never reused for another SID, so a viewer resolved earlier can never
//! end up seeing someone else's folder after the rules are rebuilt.

use flint_core::{ANY_DRIVE, Index, OWNER_NOBODY, OWNER_NONE, Owner, OwnerRule};
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    /// String SID, e.g. `S-1-5-21-…-1001`.
    pub sid: String,
    /// Profile folder, e.g. `C:\Users\deniz`.
    pub path: String,
}

/// What the rules are built from (registry `ProfileList` on Windows).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Machine {
    pub profiles: Vec<Profile>,
    /// `ProfilesDirectory`, e.g. `C:\Users`.
    pub profiles_dir: String,
    /// Other spellings of `profiles_dir` (its final path when it is a
    /// junction or a mount; see [`with_final_paths`]).
    pub profiles_dir_aliases: Vec<String>,
    /// `Public`, e.g. `C:\Users\Public`.
    pub public_dir: Option<String>,
    /// Other spellings of `public_dir`.
    pub public_dir_aliases: Vec<String>,
    /// e.g. `C:\Windows`.
    pub windows_dir: String,
}

/// SID → owner number, append-only (numbers are never reused).
#[derive(Default, Debug)]
pub struct OwnerRegistry {
    by_sid: HashMap<String, Owner>,
}

impl OwnerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The SID's number, assigning the next free one to a new SID;
    /// `None` when every number is taken.
    pub fn assign(&mut self, sid: &str) -> Option<Owner> {
        let key = sid.to_ascii_uppercase();
        if let Some(&o) = self.by_sid.get(&key) {
            return Some(o);
        }
        let next = self.by_sid.len() + 1;
        // NOBODY (65535) and NONE (0) are reserved
        let o = Owner::try_from(next).ok().filter(|&o| o < OWNER_NOBODY)?;
        self.by_sid.insert(key, o);
        Some(o)
    }

    /// Owner a client SID views as; an unknown SID sees public entries
    /// only.
    pub fn viewer(&self, sid: &str) -> Owner {
        self.by_sid
            .get(&sid.to_ascii_uppercase())
            .copied()
            .unwrap_or(OWNER_NOBODY)
    }
}

/// `C:\Users\` → `["C:", "Users"]`.
fn parts(path: &str) -> Vec<String> {
    path.split(['\\', '/'])
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn same_path(a: &[String], b: &[String]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x.to_lowercase() == y.to_lowercase())
}

/// `m` plus the final path of each profile, of the profiles folder and
/// of the public folder where it differs (`final_path`: the path with
/// junctions, mounts and short names resolved). A folder reached under
/// another spelling then gets the same owner.
pub fn with_final_paths(mut m: Machine, final_path: impl Fn(&str) -> Option<String>) -> Machine {
    let differs = |a: &str, b: &str| !same_path(&parts(a), &parts(b));
    let mut extra = Vec::new();
    for p in &m.profiles {
        if let Some(f) = final_path(&p.path)
            && differs(&f, &p.path)
            && !m
                .profiles
                .iter()
                .chain(&extra)
                .any(|q: &Profile| !differs(&q.path, &f))
        {
            extra.push(Profile {
                sid: p.sid.clone(),
                path: f,
            });
        }
    }
    m.profiles.extend(extra);
    if let Some(f) = final_path(&m.profiles_dir)
        && differs(&f, &m.profiles_dir)
        && !m.profiles_dir_aliases.iter().any(|a| !differs(a, &f))
    {
        m.profiles_dir_aliases.push(f);
    }
    if let Some(public) = m.public_dir.clone()
        && let Some(f) = final_path(&public)
        && differs(&f, &public)
        && !m.public_dir_aliases.iter().any(|a| !differs(a, &f))
    {
        m.public_dir_aliases.push(f);
    }
    m
}

/// The owner rules for `m`; new SIDs get numbers in `reg`.
pub fn rules(m: &Machine, reg: &mut OwnerRegistry) -> Vec<OwnerRule> {
    // every spelling of the profiles folder, each with its own children
    type Children = Vec<(String, Owner)>;
    let mut pdirs: Vec<(Vec<String>, Children)> = Vec::new();
    for d in std::iter::once(&m.profiles_dir).chain(&m.profiles_dir_aliases) {
        let pp = parts(d);
        if !pp.is_empty() && !pdirs.iter().any(|(q, _)| same_path(q, &pp)) {
            pdirs.push((pp, Vec::new()));
        }
    }
    let mut recycle: Vec<(String, Owner)> = Vec::new();
    let mut out = Vec::new();
    for p in &m.profiles {
        let Some(owner) = reg.assign(&p.sid) else {
            crate::log::line(format!(
                "too many accounts: profile {} gets no owner and stays hidden",
                p.sid
            ));
            continue;
        };
        let key = p.sid.to_ascii_uppercase();
        if !recycle.iter().any(|(s, _)| *s == key) {
            recycle.push((key, owner));
        }
        let mut pp = parts(&p.path);
        let Some(leaf) = pp.pop() else { continue };
        if pp.is_empty() {
            continue; // a drive root as a profile: not a folder rule
        }
        match pdirs.iter_mut().find(|(q, _)| same_path(q, &pp)) {
            Some((_, children)) => children.push((leaf, owner)),
            None => out.push(OwnerRule {
                path: pp,
                children: vec![(leaf, owner)],
                default: OWNER_NONE,
            }),
        }
    }
    for public in m.public_dir.iter().chain(&m.public_dir_aliases) {
        let mut pp = parts(public);
        if let Some(leaf) = pp.pop()
            && let Some((_, children)) = pdirs.iter_mut().find(|(q, _)| same_path(q, &pp))
        {
            children.push((leaf, OWNER_NONE));
        }
    }
    for (path, children) in pdirs {
        out.push(OwnerRule {
            path,
            children,
            default: OWNER_NOBODY,
        });
    }
    let any = |rest: &[&str]| -> Vec<String> {
        std::iter::once(ANY_DRIVE)
            .chain(rest.iter().copied())
            .map(str::to_string)
            .collect()
    };
    out.push(OwnerRule {
        path: any(&["$Recycle.Bin"]),
        children: recycle,
        default: OWNER_NOBODY,
    });
    out.push(OwnerRule {
        path: any(&[]),
        children: vec![
            ("System Volume Information".into(), OWNER_NOBODY),
            (crate::volindex::ORPHANS.into(), OWNER_NOBODY),
        ],
        default: OWNER_NONE,
    });
    out.push(OwnerRule {
        path: any(&["Windows.old", "Users"]),
        children: vec![],
        default: OWNER_NOBODY,
    });
    let wdir = parts(&m.windows_dir);
    if !wdir.is_empty() {
        out.push(OwnerRule {
            path: wdir,
            children: vec![("CSC".into(), OWNER_NOBODY)],
            default: OWNER_NONE,
        });
    }
    out
}

/// Sets the rules for `m` on `ix`.
pub fn apply(ix: &mut Index, m: &Machine, reg: &mut OwnerRegistry) {
    ix.set_owner_rules(&rules(m, reg));
}

#[cfg(windows)]
pub mod os {
    use super::{Machine, Profile};
    use crate::pipe::{Sddl, parse_sddl, sid_string};
    use crate::volume::Handle;
    use std::collections::HashMap;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use windows_sys::Win32::Foundation::{ERROR_NO_MORE_ITEMS, ERROR_SUCCESS};
    use windows_sys::Win32::Foundation::{INVALID_HANDLE_VALUE, LUID};
    use windows_sys::Win32::Security::Authorization::{
        AUTHZ_ACCESS_REPLY, AUTHZ_ACCESS_REQUEST, AUTHZ_CLIENT_CONTEXT_HANDLE,
        AUTHZ_RESOURCE_MANAGER_HANDLE, AUTHZ_RM_FLAG_NO_AUDIT, AUTHZ_SKIP_TOKEN_GROUPS,
        AuthzAccessCheck, AuthzAddSidsToContext, AuthzFreeContext, AuthzFreeResourceManager,
        AuthzInitializeContextFromSid, AuthzInitializeResourceManager, ConvertStringSidToSidW,
        GetSecurityInfo, SE_FILE_OBJECT, SetSecurityInfo,
    };
    use windows_sys::Win32::Security::{
        ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetSecurityDescriptorOwner,
        OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR,
        PSID, SECURITY_ATTRIBUTES, SID_AND_ATTRIBUTES,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateDirectoryW, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_LIST_DIRECTORY,
        FILE_NAME_NORMALIZED, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        GetFinalPathNameByHandleW, OPEN_EXISTING, READ_CONTROL, VOLUME_NAME_DOS, WRITE_DAC,
        WRITE_OWNER,
    };
    use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RRF_NOEXPAND, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ,
        RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW,
    };
    use windows_sys::Win32::System::SystemInformation::GetWindowsDirectoryW;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    fn wide_path(p: &Path) -> Vec<u16> {
        p.as_os_str().encode_wide().chain([0]).collect()
    }

    /// Opens a file or folder for its security descriptor (and, with
    /// `write`, to change owner and DACL); never for its contents.
    fn open_meta(p: &Path, write: bool) -> io::Result<Handle> {
        let w = wide_path(p);
        let access = READ_CONTROL | if write { WRITE_DAC | WRITE_OWNER } else { 0 };
        // SAFETY: w is NUL-terminated; null security attributes and
        // template are allowed; BACKUP_SEMANTICS opens folders.
        let h = unsafe {
            CreateFileW(
                w.as_ptr(),
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: a new handle, owned here only.
            Ok(unsafe { Handle::from_raw(h) })
        }
    }

    /// `p` with junctions, mounts and short names resolved (`None` when
    /// it cannot be opened or lies on a network share).
    pub fn final_path(p: &str) -> Option<String> {
        let w = wide(p);
        // SAFETY: as in open_meta; no access rights are asked for.
        let h = unsafe {
            CreateFileW(
                w.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return None;
        }
        // SAFETY: a new handle, owned here only.
        let h = unsafe { Handle::from_raw(h) };
        let mut buf = vec![0u16; 1024];
        // SAFETY: buf is writable for its length.
        let n = unsafe {
            GetFinalPathNameByHandleW(
                h.raw(),
                buf.as_mut_ptr(),
                buf.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        } as usize;
        if n == 0 || n >= buf.len() {
            return None;
        }
        let s = String::from_utf16_lossy(&buf[..n]);
        let s = s.strip_prefix(r"\\?\").unwrap_or(&s);
        (!s.starts_with(r"UNC\") && !s.starts_with(r"\\")).then(|| s.to_string())
    }

    /// The machine from the registry, with the final paths of its
    /// profile folders added (see [`super::with_final_paths`]).
    pub fn machine_resolved() -> io::Result<Machine> {
        Ok(super::with_final_paths(machine()?, final_path))
    }

    /// What the access check learned about one folder; `listable` means
    /// an ordinary user (Users, Authenticated Users, Interactive,
    /// Everyone) may list it.
    pub use crate::access::Verdict as Folder;

    /// Decides whether an ordinary user may list a folder, from its
    /// security descriptor alone: an Authz client context holding only the
    /// groups every interactive user has (no real token is needed).
    /// Verdicts are cached by DACL bytes. One per thread (not `Sync`).
    pub struct AccessChecker {
        rm: AUTHZ_RESOURCE_MANAGER_HANDLE,
        base: AUTHZ_CLIENT_CONTEXT_HANDLE,
        ctx: AUTHZ_CLIENT_CONTEXT_HANDLE,
        cache: HashMap<Vec<u8>, bool>,
    }

    // SAFETY: the Authz handles are used by one thread at a time (&mut
    // self), and may move between threads.
    unsafe impl Send for AccessChecker {}

    impl Drop for AccessChecker {
        fn drop(&mut self) {
            // SAFETY: handles created in new(), each freed once.
            unsafe {
                AuthzFreeContext(self.ctx);
                AuthzFreeContext(self.base);
                AuthzFreeResourceManager(self.rm);
            }
        }
    }

    /// `SE_GROUP_ENABLED`.
    const GROUP_ENABLED: u32 = 0x4;

    struct LocalSid(PSID);
    impl Drop for LocalSid {
        fn drop(&mut self) {
            // SAFETY: LocalAlloc'd by ConvertStringSidToSidW; freed once.
            unsafe { windows_sys::Win32::Foundation::LocalFree(self.0 as _) };
        }
    }

    fn sid(s: &str) -> io::Result<LocalSid> {
        let w = wide(s);
        let mut p: PSID = std::ptr::null_mut();
        // SAFETY: w is NUL-terminated; p receives a LocalAlloc'd SID.
        if unsafe { ConvertStringSidToSidW(w.as_ptr(), &mut p) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(LocalSid(p))
    }

    impl AccessChecker {
        pub fn new() -> io::Result<AccessChecker> {
            let mut rm: AUTHZ_RESOURCE_MANAGER_HANDLE = std::ptr::null_mut();
            // SAFETY: no callbacks, no name; rm receives a handle.
            if unsafe {
                AuthzInitializeResourceManager(
                    AUTHZ_RM_FLAG_NO_AUDIT,
                    None,
                    None,
                    None,
                    std::ptr::null(),
                    &mut rm,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            let mut c = AccessChecker {
                rm,
                base: std::ptr::null_mut(),
                ctx: std::ptr::null_mut(),
                cache: HashMap::new(),
            };
            // the context's "user" is the Users group; the other groups an
            // interactive user always has are added to it
            let users = sid("S-1-5-32-545")?;
            // SAFETY: users is a valid SID; skip token groups (no account
            // lookup); c.base receives a handle freed by Drop.
            if unsafe {
                AuthzInitializeContextFromSid(
                    AUTHZ_SKIP_TOKEN_GROUPS,
                    users.0,
                    c.rm,
                    std::ptr::null(),
                    LUID {
                        LowPart: 0,
                        HighPart: 0,
                    },
                    std::ptr::null(),
                    &mut c.base,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            let groups = [
                sid("S-1-1-0")?,
                sid("S-1-5-11")?,
                sid("S-1-5-4")?,
                sid("S-1-2-0")?,
            ];
            let list: Vec<SID_AND_ATTRIBUTES> = groups
                .iter()
                .map(|g| SID_AND_ATTRIBUTES {
                    Sid: g.0,
                    Attributes: GROUP_ENABLED,
                })
                .collect();
            // SAFETY: list holds valid SIDs alive across the call (the
            // context copies them); c.ctx receives a handle freed by Drop.
            if unsafe {
                AuthzAddSidsToContext(
                    c.base,
                    list.as_ptr(),
                    list.len() as u32,
                    std::ptr::null(),
                    0,
                    &mut c.ctx,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(c)
        }

        /// Checks the folder `h` is open on (with `READ_CONTROL`).
        pub fn check_handle(&mut self, h: &Handle) -> io::Result<Folder> {
            let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            let mut owner: PSID = std::ptr::null_mut();
            let mut dacl: *mut ACL = std::ptr::null_mut();
            // SAFETY: h is open with READ_CONTROL; owner and dacl point
            // into sd, LocalAlloc'd and freed when `_sd` drops after use.
            let r = unsafe {
                GetSecurityInfo(
                    h.raw(),
                    SE_FILE_OBJECT,
                    OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                    &mut owner,
                    std::ptr::null_mut(),
                    &mut dacl,
                    std::ptr::null_mut(),
                    &mut sd,
                )
            };
            if r != 0 {
                return Err(io::Error::from_raw_os_error(r as i32));
            }
            let _sd = Sddl(sd);
            let owner = sid_string(owner)?;
            let listable = self.check_sd(sd, dacl)?;
            Ok(Folder { listable, owner })
        }

        /// Checks the folder at `p` (opened for its descriptor only).
        pub fn check_path(&mut self, p: &Path) -> io::Result<Folder> {
            let h = open_meta(p, false)?;
            self.check_handle(&h)
        }

        fn check_sd(&mut self, sd: PSECURITY_DESCRIPTOR, dacl: *const ACL) -> io::Result<bool> {
            // a null DACL grants everyone everything; the key is the DACL
            // bytes (owner-dependent ACEs never name our context's SIDs)
            let key = if dacl.is_null() {
                Vec::new()
            } else {
                // SAFETY: dacl points to a valid ACL of AclSize bytes in sd.
                unsafe { std::slice::from_raw_parts(dacl as *const u8, (*dacl).AclSize as usize) }
                    .to_vec()
            };
            if let Some(&v) = self.cache.get(&key) {
                return Ok(v);
            }
            let req = AUTHZ_ACCESS_REQUEST {
                DesiredAccess: FILE_LIST_DIRECTORY,
                PrincipalSelfSid: std::ptr::null_mut(),
                ObjectTypeList: std::ptr::null_mut(),
                ObjectTypeListLength: 0,
                OptionalArguments: std::ptr::null_mut(),
            };
            let mut granted = 0u32;
            let mut err = 0u32;
            let mut reply = AUTHZ_ACCESS_REPLY {
                ResultListLength: 1,
                GrantedAccessMask: &mut granted,
                SaclEvaluationResults: std::ptr::null_mut(),
                Error: &mut err,
            };
            // SAFETY: ctx is a live context; sd a valid descriptor; reply
            // points at one result slot each.
            if unsafe {
                AuthzAccessCheck(
                    0,
                    self.ctx,
                    &req,
                    std::ptr::null_mut(),
                    sd,
                    std::ptr::null(),
                    0,
                    &mut reply,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            let ok = err == 0 && granted & FILE_LIST_DIRECTORY != 0;
            self.cache.insert(key, ok);
            Ok(ok)
        }
    }

    /// Owner SID string from a descriptor.
    fn sd_owner(sd: PSECURITY_DESCRIPTOR) -> io::Result<String> {
        let mut owner: PSID = std::ptr::null_mut();
        let mut defaulted = 0;
        // SAFETY: sd is a valid descriptor; owner points into it.
        if unsafe { GetSecurityDescriptorOwner(sd, &mut owner, &mut defaulted) } == 0 {
            return Err(io::Error::last_os_error());
        }
        sid_string(owner)
    }

    /// Creates folder `p` with the descriptor `sddl`, or, when it exists:
    /// with an owner in `trusted_owners`, sets that DACL on it again; with
    /// any other owner (an ordinary user made it first, and could read
    /// what is written there), deletes it with everything inside and
    /// creates it anew. Links inside are removed, never followed.
    pub fn secure_dir(p: &Path, sddl: &str, trusted_owners: &[&str]) -> io::Result<()> {
        let sd = parse_sddl(sddl)?;
        if let Ok(md) = std::fs::symlink_metadata(p) {
            let owner = {
                let h = open_meta(p, false)?;
                let mut raw: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
                // SAFETY: h has READ_CONTROL; raw receives a LocalAlloc'd
                // descriptor freed by Sddl.
                let r = unsafe {
                    GetSecurityInfo(
                        h.raw(),
                        SE_FILE_OBJECT,
                        OWNER_SECURITY_INFORMATION,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        &mut raw,
                    )
                };
                if r != 0 {
                    return Err(io::Error::from_raw_os_error(r as i32));
                }
                let raw = Sddl(raw);
                sd_owner(raw.0)?
            };
            let trusted = md.is_dir()
                && !md.file_type().is_symlink()
                && trusted_owners
                    .iter()
                    .any(|t| t.eq_ignore_ascii_case(&owner));
            if trusted {
                let h = open_meta(p, true)?;
                let mut dacl: *mut ACL = std::ptr::null_mut();
                let (mut present, mut defaulted) = (0, 0);
                // SAFETY: sd is valid; dacl points into it.
                if unsafe {
                    GetSecurityDescriptorDacl(sd.0, &mut present, &mut dacl, &mut defaulted)
                } == 0
                {
                    return Err(io::Error::last_os_error());
                }
                // SAFETY: h has WRITE_DAC; dacl lives in sd for the call.
                let r = unsafe {
                    SetSecurityInfo(
                        h.raw(),
                        SE_FILE_OBJECT,
                        DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        dacl,
                        std::ptr::null(),
                    )
                };
                return if r == 0 {
                    Ok(())
                } else {
                    Err(io::Error::from_raw_os_error(r as i32))
                };
            }
            crate::log::line(format!(
                "{} was made by {owner}, not by a trusted account: replaced",
                p.display()
            ));
            // std removes links without following them
            if md.is_dir() && !md.file_type().is_symlink() {
                std::fs::remove_dir_all(p)?;
            } else if md.is_dir() {
                std::fs::remove_dir(p)?;
            } else {
                std::fs::remove_file(p)?;
            }
        }
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let w = wide_path(p);
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: 0,
        };
        // SAFETY: w is NUL-terminated; sa points to a valid descriptor.
        if unsafe { CreateDirectoryW(w.as_ptr(), &sa) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    struct Key(HKEY);
    impl Drop for Key {
        fn drop(&mut self) {
            // SAFETY: an open key owned by this value; closed once.
            unsafe { RegCloseKey(self.0) };
        }
    }

    /// `%SystemDrive%\Users` → `C:\Users`.
    fn expand(s: &str) -> Result<String, u32> {
        let w = wide(s);
        let mut out = vec![0u16; 2048];
        // SAFETY: w is NUL-terminated; out is writable for its length.
        let n = unsafe { ExpandEnvironmentStringsW(w.as_ptr(), out.as_mut_ptr(), out.len() as u32) }
            as usize;
        if n == 0 || n > out.len() {
            return Err(n as u32);
        }
        Ok(String::from_utf16_lossy(&out[..n - 1]))
    }

    /// A string value of `key\sub`, environment strings expanded (the
    /// ProfileList values are REG_EXPAND_SZ, and RegGetValueW returned
    /// them unexpanded with RRF_RT_REG_SZ on Windows 11: a rule on
    /// `%SystemDrive%\Users` would never match and fail open).
    fn string(key: &Key, sub: Option<&str>, value: &str) -> Result<String, u32> {
        let sub = sub.map(wide);
        let value = wide(value);
        let mut buf = vec![0u16; 1024];
        let mut bytes = (buf.len() * 2) as u32;
        // SAFETY: strings NUL-terminated (or null for no subkey); buf is
        // writable for `bytes`.
        let r = unsafe {
            RegGetValueW(
                key.0,
                sub.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
                value.as_ptr(),
                RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND,
                std::ptr::null_mut(),
                buf.as_mut_ptr() as *mut _,
                &mut bytes,
            )
        };
        if r != ERROR_SUCCESS {
            return Err(r);
        }
        let n = (bytes as usize / 2).min(buf.len());
        expand(String::from_utf16_lossy(&buf[..n]).trim_end_matches('\0'))
    }

    /// The profile list, profiles folder, public folder and Windows folder
    /// from `HKLM\…\ProfileList` (environment strings expanded).
    pub fn machine() -> io::Result<Machine> {
        let path = wide(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList");
        let mut k: HKEY = std::ptr::null_mut();
        // SAFETY: path is NUL-terminated; k receives an open key.
        let r = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, path.as_ptr(), 0, KEY_READ, &mut k) };
        if r != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(r as i32));
        }
        let key = Key(k);
        let profiles_dir = string(&key, None, "ProfilesDirectory")
            .map_err(|r| io::Error::from_raw_os_error(r as i32))?;
        let public_dir = string(&key, None, "Public").ok();
        let mut out = Vec::new();
        for i in 0.. {
            let mut name = [0u16; 256];
            let mut len = name.len() as u32;
            // SAFETY: name is writable for len units; unused outputs null.
            let r = unsafe {
                RegEnumKeyExW(
                    key.0,
                    i,
                    name.as_mut_ptr(),
                    &mut len,
                    std::ptr::null(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if r == ERROR_NO_MORE_ITEMS {
                break;
            }
            if r != ERROR_SUCCESS {
                return Err(io::Error::from_raw_os_error(r as i32));
            }
            let sid = String::from_utf16_lossy(&name[..len as usize]);
            if !sid.starts_with("S-1-") {
                continue;
            }
            match string(&key, Some(&sid), "ProfileImagePath") {
                Ok(path) => out.push(Profile { sid, path }),
                Err(r) => crate::log::line(format!(
                    "profile {sid}: no readable ProfileImagePath (error {r})"
                )),
            }
        }
        let mut w = [0u16; 260];
        // SAFETY: w is writable for its length.
        let n = unsafe { GetWindowsDirectoryW(w.as_mut_ptr(), w.len() as u32) } as usize;
        if n == 0 || n > w.len() {
            return Err(io::Error::last_os_error());
        }
        Ok(Machine {
            profiles: out,
            profiles_dir,
            profiles_dir_aliases: vec![],
            public_dir,
            public_dir_aliases: vec![],
            windows_dir: String::from_utf16_lossy(&w[..n]),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flint_core::{Boosts, EntryId, EntryInput, SearchOptions, parse, search};
    use std::sync::atomic::AtomicBool;

    fn dir(ix: &mut Index, n: &str, p: Option<EntryId>) -> EntryId {
        ix.insert(EntryInput {
            name: n,
            parent: p,
            is_dir: true,
            last_change: 0,
        })
    }
    fn file(ix: &mut Index, n: &str, p: EntryId) -> EntryId {
        ix.insert(EntryInput {
            name: n,
            parent: Some(p),
            is_dir: false,
            last_change: 0,
        })
    }

    const ALICE: &str = "S-1-5-21-1-2-3-1001";
    const BOB: &str = "S-1-5-21-1-2-3-1002";

    fn machine_desc() -> Machine {
        Machine {
            profiles: vec![
                Profile {
                    sid: ALICE.into(),
                    path: r"C:\Users\alice".into(),
                },
                // case and a trailing slash differ from the index
                Profile {
                    sid: BOB.to_lowercase().replace("s-", "S-"),
                    path: r"c:\users\bob\".into(),
                },
                Profile {
                    sid: "S-1-5-19".into(),
                    path: r"C:\Windows\ServiceProfiles\LocalService".into(),
                },
            ],
            profiles_dir: r"C:\Users".into(),
            profiles_dir_aliases: vec![],
            public_dir: Some(r"C:\Users\Public".into()),
            public_dir_aliases: vec![],
            windows_dir: r"C:\Windows".into(),
        }
    }

    fn disk() -> Index {
        let mut ix = Index::new();
        let c = dir(&mut ix, "C:", None);
        let d = dir(&mut ix, "D:", None);
        let users = dir(&mut ix, "Users", Some(c));
        for (n, f) in [
            ("alice", "rapor-a.docx"),
            ("Bob", "rapor-b.docx"),
            ("Public", "rapor-ortak.docx"),
        ] {
            let u = dir(&mut ix, n, Some(users));
            file(&mut ix, f, u);
        }
        let gone = dir(&mut ix, "carol", Some(users)); // not in the profile list
        file(&mut ix, "rapor-c.docx", gone);
        for drive in [c, d] {
            let rb = dir(&mut ix, "$Recycle.Bin", Some(drive));
            let rb_bob = dir(&mut ix, BOB, Some(rb));
            file(&mut ix, "rapor-eski.docx", rb_bob);
            let svi = dir(&mut ix, "System Volume Information", Some(drive));
            file(&mut ix, "rapor.dat", svi);
        }
        let win = dir(&mut ix, "Windows", Some(c));
        let csc = dir(&mut ix, "CSC", Some(win));
        file(&mut ix, "rapor.tmp", csc);
        let sp = dir(&mut ix, "ServiceProfiles", Some(win));
        let ls = dir(&mut ix, "LocalService", Some(sp));
        file(&mut ix, "rapor-ls.txt", ls);
        let wold = dir(&mut ix, "Windows.old", Some(c));
        let wou = dir(&mut ix, "Users", Some(wold));
        let woa = dir(&mut ix, "alice", Some(wou));
        file(&mut ix, "rapor-eski-windows.docx", woa);
        let shared = dir(&mut ix, "Shared", Some(d));
        file(&mut ix, "rapor.pdf", shared);
        let orphans = dir(&mut ix, crate::volindex::ORPHANS, Some(c));
        file(&mut ix, "rapor-yetim.txt", orphans);
        ix
    }

    fn visible(ix: &Index, q: &str, viewer: Owner) -> Vec<String> {
        let b = Boosts::new();
        let cancel = AtomicBool::new(false);
        let r = search(
            ix,
            &parse(q),
            &SearchOptions {
                limit: 50,
                now: 0,
                boosts: &b,
                cancel: &cancel,
                viewer,
            },
        );
        let mut v: Vec<String> = r.hits.iter().map(|h| ix.path(h.id)).collect();
        v.sort();
        v
    }

    #[test]
    fn maps_profiles_recycle_and_system() {
        let mut ix = disk();
        let mut reg = OwnerRegistry::new();
        apply(&mut ix, &machine_desc(), &mut reg);
        let alice = reg.viewer(ALICE);
        let bob = reg.viewer(&BOB.to_lowercase());
        assert_eq!((alice, bob), (1, 2));
        assert_eq!(
            visible(&ix, "rapor", alice),
            vec![
                r"C:\Users\Public\rapor-ortak.docx",
                r"C:\Users\alice\rapor-a.docx",
                r"D:\Shared\rapor.pdf"
            ]
        );
        assert_eq!(
            visible(&ix, "rapor", bob),
            vec![
                r"C:\$Recycle.Bin\S-1-5-21-1-2-3-1002\rapor-eski.docx",
                r"C:\Users\Bob\rapor-b.docx",
                r"C:\Users\Public\rapor-ortak.docx",
                r"D:\$Recycle.Bin\S-1-5-21-1-2-3-1002\rapor-eski.docx",
                r"D:\Shared\rapor.pdf",
            ]
        );
        // a service profile outside C:\Users: only that folder is private
        let ls = reg.viewer("S-1-5-19");
        assert!(
            visible(&ix, "rapor", ls)
                .contains(&r"C:\Windows\ServiceProfiles\LocalService\rapor-ls.txt".to_string())
        );
        assert!(
            !visible(&ix, "rapor", alice)
                .iter()
                .any(|p| p.contains("LocalService"))
        );
    }

    #[test]
    fn unknown_sid_sees_only_public() {
        let mut ix = disk();
        let mut reg = OwnerRegistry::new();
        apply(&mut ix, &machine_desc(), &mut reg);
        let v = reg.viewer("S-1-5-21-9-9-9-500");
        assert_eq!(v, OWNER_NOBODY);
        assert_eq!(
            visible(&ix, "rapor", v),
            vec![r"C:\Users\Public\rapor-ortak.docx", r"D:\Shared\rapor.pdf"]
        );
        // nothing hidden without rules: the filter is off, not inverted
        assert_eq!(visible(&disk(), "rapor", OWNER_NONE).len(), 13);
    }

    #[test]
    fn unlisted_profile_and_windows_old_are_nobodys() {
        let mut ix = disk();
        let mut reg = OwnerRegistry::new();
        apply(&mut ix, &machine_desc(), &mut reg);
        for v in [reg.viewer(ALICE), reg.viewer(BOB)] {
            let p = visible(&ix, "rapor", v);
            assert!(
                !p.iter().any(|x| x.contains("carol")
                    || x.contains("Windows.old")
                    || x.contains("Orphans")),
                "{p:?}"
            );
        }
    }

    /// Positional numbers shifted when a profile left
    /// the list, so an open connection's viewer pointed at another account.
    #[test]
    fn registry_numbers_are_never_reused() {
        let mut reg = OwnerRegistry::new();
        let mut m = machine_desc();
        rules(&m, &mut reg);
        let (a, b) = (reg.viewer(ALICE), reg.viewer(BOB));
        m.profiles.remove(0); // alice's account deleted
        m.profiles.push(Profile {
            sid: "S-1-5-21-1-2-3-1003".into(),
            path: r"C:\Users\dave".into(),
        });
        rules(&m, &mut reg);
        assert_eq!(reg.viewer(BOB), b);
        assert_eq!(reg.viewer(ALICE), a, "a known SID keeps its number");
        let dave = reg.viewer("S-1-5-21-1-2-3-1003");
        assert!(dave != a && dave != b && dave != OWNER_NOBODY);
    }

    #[test]
    fn registry_refuses_when_full() {
        let mut reg = OwnerRegistry::new();
        for i in 0..(OWNER_NOBODY as u32 - 1) {
            assert!(reg.assign(&format!("S-1-5-21-{i}")).is_some());
        }
        assert_eq!(reg.assign("S-1-5-21-one-more"), None);
        assert_eq!(reg.viewer("S-1-5-21-one-more"), OWNER_NOBODY);
    }

    #[cfg(windows)]
    /// Profile data reached under another spelling (the
    /// profiles folder junctioned or mounted to D:, a short name in the
    /// registry) is covered by rules on the final paths too.
    #[test]
    fn final_paths_get_the_same_rules() {
        let mut ix = Index::new();
        let d = dir(&mut ix, "D:", None);
        let users = dir(&mut ix, "Users", Some(d));
        let a = dir(&mut ix, "alice", Some(users));
        let b = dir(&mut ix, "Bob", Some(users));
        let p = dir(&mut ix, "Public", Some(users));
        let other = dir(&mut ix, "eski-hesap", Some(users));
        let prof = dir(&mut ix, "Profiles", Some(d));
        let eve = dir(&mut ix, "eve.ofis", Some(prof));
        file(&mut ix, "rapor-a.txt", a);
        file(&mut ix, "rapor-b.txt", b);
        file(&mut ix, "rapor-ortak.txt", p);
        file(&mut ix, "rapor-eski.txt", other);
        file(&mut ix, "rapor-eve.txt", eve);
        let mut m = machine_desc();
        m.profiles.push(Profile {
            sid: "S-1-5-21-1-2-3-1005".into(),
            path: r"D:\Profiles\EVE~1".into(),
        });
        let m = with_final_paths(m, |p| {
            let l = p.to_lowercase();
            l.strip_prefix(r"c:\users")
                .map(|rest| format!(r"D:\Users{rest}"))
                .or_else(|| {
                    (l == r"d:\profiles\eve~1").then(|| r"D:\Profiles\eve.ofis".to_string())
                })
        });
        let mut reg = OwnerRegistry::new();
        apply(&mut ix, &m, &mut reg);
        let alice = reg.viewer(ALICE);
        assert_eq!(
            visible(&ix, "rapor", alice),
            vec![
                r"D:\Users\Public\rapor-ortak.txt",
                r"D:\Users\alice\rapor-a.txt"
            ]
        );
        let eve_v = reg.viewer("S-1-5-21-1-2-3-1005");
        assert_eq!(
            visible(&ix, "rapor", eve_v),
            vec![
                r"D:\Profiles\eve.ofis\rapor-eve.txt",
                r"D:\Users\Public\rapor-ortak.txt"
            ]
        );
        // unchanged paths add nothing
        let same = with_final_paths(machine_desc(), |p| Some(p.to_string()));
        assert_eq!(same, machine_desc());
    }

    /// An ordinary user's right to list a folder, read from
    /// its security descriptor without that user's token.
    #[cfg(windows)]
    #[test]
    fn access_check_reads_the_folder_acl() {
        let me = crate::pipe::current_user_sid().unwrap();
        let base = std::env::temp_dir().join(format!("flint-acl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let mine = base.join("yalniz-ben");
        let users = base.join("herkes");
        os::secure_dir(&mine, &format!("O:{me}D:P(A;OICI;FA;;;{me})"), &[&me]).unwrap();
        os::secure_dir(
            &users,
            &format!("O:{me}D:P(A;OICI;FA;;;{me})(A;OICI;0x1200a9;;;BU)"),
            &[&me],
        )
        .unwrap();
        let mut c = os::AccessChecker::new().unwrap();
        let f = c.check_path(&mine).unwrap();
        assert!(!f.listable, "only I may list it");
        assert!(f.owner.eq_ignore_ascii_case(&me), "{}", f.owner);
        assert!(c.check_path(&users).unwrap().listable, "Users may list it");
        // a folder with a deny for Users is not listable either
        let deny = base.join("yasak");
        os::secure_dir(
            &deny,
            &format!("O:{me}D:P(D;OICI;0x1;;;BU)(A;OICI;FA;;;WD)"),
            &[&me],
        )
        .unwrap();
        assert!(!c.check_path(&deny).unwrap().listable);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The service's data folder is created with its own
    /// ACL; one found with an owner not trusted (made beforehand by an
    /// ordinary user, who could read the snapshots) is emptied and
    /// recreated, and a trusted one gets its ACL set again.
    #[cfg(windows)]
    #[test]
    fn secure_dir_replaces_a_folder_made_by_someone_else() {
        let me = crate::pipe::current_user_sid().unwrap();
        let base = std::env::temp_dir().join(format!("flint-sec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("Flint")).unwrap();
        std::fs::write(base.join(r"Flint\tuzak.bin"), b"x").unwrap();
        let dir = base.join("Flint");
        let sddl = format!("O:{me}D:P(A;OICI;FA;;;{me})(A;OICI;FA;;;SY)");
        // owner "me" is not trusted here: the folder is replaced
        os::secure_dir(&dir, &sddl, &["S-1-5-18"]).unwrap();
        assert!(!dir.join("tuzak.bin").exists(), "planted file removed");
        let mut c = os::AccessChecker::new().unwrap();
        assert!(
            !c.check_path(&dir).unwrap().listable,
            "ordinary users cannot list it"
        );
        // trusted owner: kept, ACL set again
        std::fs::write(dir.join("index.bin"), b"x").unwrap();
        os::secure_dir(&dir, &sddl, &[&me]).unwrap();
        assert!(dir.join("index.bin").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn reads_this_machines_profile_list() {
        let m = os::machine().unwrap();
        let me = crate::pipe::current_user_sid().unwrap();
        let mine = m
            .profiles
            .iter()
            .find(|x| x.sid == me)
            .expect("the current user has a profile");
        let home = std::env::var("USERPROFILE").unwrap();
        assert_eq!(mine.path.to_lowercase(), home.to_lowercase());
        assert!(
            !mine.path.contains('%') && !m.profiles_dir.contains('%'),
            "environment strings expanded"
        );
        assert!(
            home.to_lowercase()
                .starts_with(&m.profiles_dir.to_lowercase())
        );
        assert!(
            m.windows_dir.to_lowercase().ends_with("windows"),
            "{}",
            m.windows_dir
        );
    }
}
