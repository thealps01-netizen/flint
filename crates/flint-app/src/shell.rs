//! What happens to a chosen result, run as the signed-in user:
//! open it, show it in Explorer, read its details.

use std::path::Path;

#[derive(Debug, PartialEq)]
pub enum ShellError {
    /// The file is gone since it was indexed.
    Missing,
    /// Windows refused (no app for the type, access denied, ...).
    Failed(String),
}

impl std::fmt::Display for ShellError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => f.write_str("File no longer exists"),
            Self::Failed(why) => f.write_str(why),
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Details {
    /// Bytes; `None` for folders.
    pub size: Option<u64>,
    /// Unix seconds.
    pub modified: Option<i64>,
    /// "Microsoft Excel Worksheet", "File folder", ...
    pub type_name: String,
    /// Entries in a folder (counted up to `MAX_ITEMS`); `None` for files.
    pub items: Option<u64>,
}

/// A folder's entries are counted up to this many (then shown as "10,000+").
pub const MAX_ITEMS: u64 = 10_000;

/// Size, date and type of `path`, read as the user (`GetFileAttributesExW`
/// and the shell's type name); `None` when it no longer exists.
pub fn details(path: &str) -> Option<Details> {
    let (attrs, size, modified) = os::attributes(path)?;
    let is_dir = attrs & os::DIRECTORY != 0;
    // a folder that cannot be listed (no access) shows no count, not "0"
    let items = is_dir
        .then(|| {
            std::fs::read_dir(path)
                .ok()
                .map(|rd| rd.take(MAX_ITEMS as usize).count() as u64)
        })
        .flatten();
    Some(Details {
        size: (!is_dir).then_some(size),
        modified,
        type_name: os::type_name(path, attrs),
        items,
    })
}

/// Only a path Windows reports as not there is `Missing` (and forgotten);
/// one it cannot look up (drive gone, no access, bad name) is not.
fn missing(path: &str) -> bool {
    matches!(std::fs::metadata(path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
}

fn check(path: &str) -> Result<(), ShellError> {
    match std::fs::metadata(path) {
        Ok(_) => Ok(()),
        Err(_) if missing(path) => Err(ShellError::Missing),
        Err(e) => Err(ShellError::Failed(e.to_string())),
    }
}

pub fn open(path: &str) -> Result<(), ShellError> {
    check(path)?;
    os::open(path).map_err(ShellError::Failed)
}

/// Runs `program args` as administrator; see `os::run_elevated`.
#[cfg(windows)]
pub fn run_elevated(program: &Path, args: &str) -> std::io::Result<u32> {
    os::run_elevated(program, args)
}

pub fn reveal(path: &str) -> Result<(), ShellError> {
    check(path)?;
    os::reveal(path).map_err(ShellError::Failed)
}

#[cfg(windows)]
mod os {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_FLAGS_AND_ATTRIBUTES, GetFileAttributesExW,
        GetFileExInfoStandard, WIN32_FILE_ATTRIBUTE_DATA,
    };
    use windows::Win32::UI::Shell::{
        ILFree, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, SHFILEINFOW,
        SHGFI_TYPENAME, SHGFI_USEFILEATTRIBUTES, SHGetFileInfoW, SHOpenFolderAndSelectItems,
        SHParseDisplayName, ShellExecuteExW,
    };
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    use windows::core::{HSTRING, PCWSTR, w};

    pub const DIRECTORY: u32 = FILE_ATTRIBUTE_DIRECTORY.0;

    fn unix(ft: FILETIME) -> Option<i64> {
        let t = ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64;
        // 100 ns ticks since 1601
        (t != 0).then(|| (t / 10_000_000) as i64 - 11_644_473_600)
    }

    pub fn attributes(path: &str) -> Option<(u32, u64, Option<i64>)> {
        let mut d = WIN32_FILE_ATTRIBUTE_DATA::default();
        // SAFETY: d is a WIN32_FILE_ATTRIBUTE_DATA, as GetFileExInfoStandard expects.
        unsafe {
            GetFileAttributesExW(
                &HSTRING::from(path),
                GetFileExInfoStandard,
                (&mut d as *mut WIN32_FILE_ATTRIBUTE_DATA).cast(),
            )
            .ok()?;
        }
        let size = ((d.nFileSizeHigh as u64) << 32) | d.nFileSizeLow as u64;
        Some((d.dwFileAttributes, size, unix(d.ftLastWriteTime)))
    }

    /// The shell's type name from the extension and attributes only (no
    /// file access: `SHGFI_USEFILEATTRIBUTES`).
    pub fn type_name(path: &str, attrs: u32) -> String {
        let mut info = SHFILEINFOW::default();
        // SAFETY: info is an SHFILEINFOW and its size is passed.
        let ok = unsafe {
            SHGetFileInfoW(
                &HSTRING::from(path),
                FILE_FLAGS_AND_ATTRIBUTES(attrs),
                Some(&mut info),
                std::mem::size_of::<SHFILEINFOW>() as u32,
                SHGFI_TYPENAME | SHGFI_USEFILEATTRIBUTES,
            )
        };
        if ok == 0 {
            return String::new();
        }
        let n = info
            .szTypeName
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(info.szTypeName.len());
        String::from_utf16_lossy(&info.szTypeName[..n])
    }

    /// Runs the file's default action, as a double click in Explorer
    /// does (no verb: the default one, else "open"), on a COM thread, which
    /// some shell handlers need.
    pub fn open(path: &str) -> Result<(), String> {
        crate::recent::os::with_com(|| {
            let file = HSTRING::from(path);
            let mut sei = SHELLEXECUTEINFOW {
                cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
                // NOASYNC: the call has finished when it returns (we may exit);
                // FLAG_NO_UI: the error comes back to the launcher, which shows it
                fMask: SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
                lpVerb: PCWSTR::null(),
                lpFile: PCWSTR(file.as_ptr()),
                nShow: SW_SHOWNORMAL.0,
                ..Default::default()
            };
            // SAFETY: sei is filled for ShellExecuteExW; `file` outlives the call.
            unsafe { ShellExecuteExW(&mut sei) }.map_err(|e| e.message())
        })
    }

    pub fn reveal(path: &str) -> Result<(), String> {
        crate::recent::os::with_com(|| {
            let mut pidl = std::ptr::null_mut();
            // SAFETY: pidl receives an ITEMIDLIST freed with ILFree below.
            unsafe {
                SHParseDisplayName(&HSTRING::from(path), None, &mut pidl, 0, None)
                    .map_err(|e| e.message())?;
                let r = SHOpenFolderAndSelectItems(pidl, None, 0).map_err(|e| e.message());
                ILFree(Some(pidl));
                r
            }
        })
    }

    /// Starts `program args` as administrator (one UAC prompt), waits (at
    /// most `ELEVATED_WAIT_MS`) and returns its exit code. A declined
    /// prompt is the OS error 1223; a run that does not end in time is
    /// `TimedOut` (it keeps running; its log tells how it ended).
    pub fn run_elevated(program: &std::path::Path, args: &str) -> std::io::Result<u32> {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::Foundation::WAIT_TIMEOUT;
        use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
        const ELEVATED_WAIT_MS: u32 = 120_000;
        use windows::Win32::UI::Shell::SEE_MASK_NOCLOSEPROCESS;
        use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
        let file = HSTRING::from(program.as_os_str());
        let params = HSTRING::from(args);
        let mut sei = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
            lpVerb: w!("runas"),
            lpFile: PCWSTR(file.as_ptr()),
            lpParameters: PCWSTR(params.as_ptr()),
            nShow: SW_HIDE.0,
            ..Default::default()
        };
        // SAFETY: sei is filled for ShellExecuteExW; file and params outlive it.
        if let Err(e) = unsafe { ShellExecuteExW(&mut sei) } {
            // a Win32 error wrapped in an HRESULT (facility 7) keeps its code
            let hr = e.code().0 as u32;
            let code = if hr >> 16 == 0x8007 {
                (hr & 0xFFFF) as i32
            } else {
                hr as i32
            };
            return Err(std::io::Error::from_raw_os_error(code));
        }
        let mut code = 0u32;
        // SAFETY: hProcess is the started process (NOCLOSEPROCESS), closed here.
        unsafe {
            if WaitForSingleObject(sei.hProcess, ELEVATED_WAIT_MS) == WAIT_TIMEOUT {
                let _ = CloseHandle(sei.hProcess);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "the installer is still running after 2 minutes",
                ));
            }
            let r = GetExitCodeProcess(sei.hProcess, &mut code);
            let _ = CloseHandle(sei.hProcess);
            r.map_err(|e| std::io::Error::other(e.message()))?;
        }
        Ok(code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_path_that_is_not_there_is_missing() {
        let d = tmp("missing");
        assert!(matches!(
            reveal(&d.join("gone.txt").to_string_lossy()),
            Err(ShellError::Missing)
        ));
        // a name Windows cannot even look up (a drive gone, no access, a bad
        // name) is not "gone": the history keeps it
        assert!(matches!(
            reveal(r"C:\bad<name>.txt"),
            Err(ShellError::Failed(_))
        ));
        let _ = std::fs::remove_dir_all(&d);
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("flint-shell-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn details_of_a_temp_file() {
        let d = tmp("details");
        let f = d.join("bütçe 2026.txt");
        std::fs::write(&f, vec![b'x'; 48 * 1024]).unwrap();
        let got = details(&f.to_string_lossy()).unwrap();
        let meta = std::fs::metadata(&f).unwrap();
        assert_eq!(got.size, Some(meta.len()));
        let want = meta
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert_eq!(got.modified, Some(want));
        assert!(!got.type_name.is_empty(), "the shell names .txt files");
        assert_eq!(got.items, None);
        for i in 0..3 {
            std::fs::write(d.join(format!("{i}.txt")), b"").unwrap();
        }
        let folder = details(&d.to_string_lossy()).unwrap();
        assert_eq!((folder.size, folder.items), (None, Some(4)));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn details_of_missing_is_none() {
        assert_eq!(
            details(&tmp("details-missing").join("yok.txt").to_string_lossy()),
            None
        );
    }

    #[test]
    fn open_and_reveal_of_missing_say_so() {
        let p = tmp("open").join("yok.txt").to_string_lossy().into_owned();
        assert_eq!(open(&p), Err(ShellError::Missing));
        assert_eq!(reveal(&p), Err(ShellError::Missing));
        assert_eq!(ShellError::Missing.to_string(), "File no longer exists");
    }
}
