//! Windows' own recent items (%APPDATA%\Microsoft\Windows\Recent), read
//! as (target path, shortcut time) to seed the history on first run
//! (the "Use Windows recent files" setting).

use std::path::{Path, PathBuf};

/// `%APPDATA%\Microsoft\Windows\Recent`.
pub fn default_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|d| {
        PathBuf::from(d)
            .join("Microsoft")
            .join("Windows")
            .join("Recent")
    })
}

fn unix_secs(t: std::time::SystemTime) -> u32 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as u32)
}

/// Every `.lnk` in `dir` whose target is a file or folder that still
/// exists, with the shortcut's modified time. Links to URLs, to nothing,
/// or to deleted files are skipped; a link that cannot be read is logged.
pub fn targets(dir: &Path) -> Vec<(String, u32)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let links: Vec<(PathBuf, u32)> = rd
        .flatten()
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("lnk"))
        })
        .filter_map(|e| {
            let t = e.metadata().ok()?.modified().ok()?;
            Some((e.path(), unix_secs(t)))
        })
        .collect();
    let mut out = Vec::new();
    os::with_com(|| {
        for (lnk, t) in links {
            match os::link_target(&lnk) {
                Ok(Some(target)) if !target.starts_with(r"\\") && Path::new(&target).exists() => {
                    out.push((target, t))
                }
                Ok(_) => {}
                Err(e) => {
                    crate::log::line(format!("recent item not read ({}): {e}", lnk.display()))
                }
            }
        }
    });
    out
}

#[cfg(windows)]
pub mod os {
    use std::path::Path;
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
        CoUninitialize, IPersistFile, STGM_READ,
    };
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    use windows::core::{HSTRING, Interface};

    /// Runs `f` with COM initialised on this thread (single-threaded
    /// apartment, as the shell's objects expect).
    pub fn with_com<T>(f: impl FnOnce() -> T) -> T {
        // SAFETY: paired with CoUninitialize below when it succeeded
        // (S_OK or S_FALSE); a thread already in another apartment keeps it.
        let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let r = f();
        if hr.is_ok() {
            // SAFETY: balances the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
        r
    }

    /// The path a shell link points to; `None` for links without a file
    /// system target (URLs, shell folders such as "This PC").
    pub fn link_target(lnk: &Path) -> windows::core::Result<Option<String>> {
        // SAFETY: COM is initialised by the caller (`with_com`); the buffer
        // outlives the call and its length is passed with it.
        unsafe {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            let file: IPersistFile = link.cast()?;
            file.Load(&HSTRING::from(lnk.as_os_str()), STGM_READ)?;
            let mut buf = vec![0u16; 32_768];
            link.GetPath(&mut buf, std::ptr::null_mut(), 0)?;
            let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            Ok((n > 0).then(|| String::from_utf16_lossy(&buf[..n])))
        }
    }

    /// Writes a shell link to `target` (tests make their own Recent folder).
    #[cfg(test)]
    pub fn make_link(lnk: &Path, target: &Path) -> windows::core::Result<()> {
        // SAFETY: as in `link_target`.
        unsafe {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            link.SetPath(&HSTRING::from(target.as_os_str()))?;
            let file: IPersistFile = link.cast()?;
            file.Save(&HSTRING::from(lnk.as_os_str()), true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_from_recent() {
        let d = std::env::temp_dir().join(format!("flint-recent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("files")).unwrap();
        let keep = d.join(r"files\bütçe 2026.xlsx");
        let gone = d.join(r"files\silindi.txt");
        std::fs::write(&keep, b"x").unwrap();
        std::fs::write(&gone, b"x").unwrap();
        os::with_com(|| {
            os::make_link(&d.join("bütçe 2026.xlsx.lnk"), &keep).unwrap();
            os::make_link(&d.join("silindi.txt.lnk"), &gone).unwrap();
        });
        std::fs::remove_file(&gone).unwrap();
        std::fs::write(d.join("notes.txt"), b"not a link").unwrap();
        // a working link under another extension is not a recent item
        std::fs::copy(d.join("bütçe 2026.xlsx.lnk"), d.join("kopya.bin")).unwrap();
        let got = targets(&d);
        assert_eq!(got.len(), 1, "{got:?}");
        // the same file: Windows returns its long name, while the temp
        // folder may be given in short (8.3) form, as on GitHub's runners
        // (C:\Users\RUNNER~1\...)
        assert_eq!(
            std::fs::canonicalize(&got[0].0).unwrap(),
            std::fs::canonicalize(&keep).unwrap(),
            "{got:?}"
        );
        let now = unix_secs(std::time::SystemTime::now());
        assert!(now.abs_diff(got[0].1) < 120, "the shortcut's time");
        assert!(targets(&d.join("missing")).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }
}
