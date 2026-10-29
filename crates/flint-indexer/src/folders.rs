//! Windows' system folders, from the shell's known-folder API. Not from
//! %ProgramFiles% / %ProgramData%: a user can define those for themselves,
//! and a program run as administrator for that user would then use a
//! folder the user chose.

use std::io;
use std::path::PathBuf;
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::UI::Shell::{
    FOLDERID_ProgramData, FOLDERID_ProgramFiles, SHGetKnownFolderPath,
};
use windows_sys::core::{GUID, PWSTR};

fn known(id: &GUID) -> io::Result<PathBuf> {
    let mut p: PWSTR = std::ptr::null_mut();
    // SAFETY: `p` receives a string the shell allocates; it is read up to
    // its NUL and freed with CoTaskMemFree, also on failure.
    unsafe {
        let hr = SHGetKnownFolderPath(id, 0, std::ptr::null_mut(), &mut p);
        let path = (hr >= 0 && !p.is_null()).then(|| {
            let len = (0..).take_while(|&i| *p.add(i) != 0).count();
            PathBuf::from(String::from_utf16_lossy(std::slice::from_raw_parts(p, len)))
        });
        CoTaskMemFree(p as *const _);
        path.ok_or_else(|| {
            io::Error::other(format!("known folder not available (HRESULT {hr:#x})"))
        })
    }
}

/// `C:\Program Files` (64-bit programs).
pub fn program_files() -> io::Result<PathBuf> {
    known(&FOLDERID_ProgramFiles)
}

/// `C:\ProgramData`.
pub fn program_data() -> io::Result<PathBuf> {
    known(&FOLDERID_ProgramData)
}
