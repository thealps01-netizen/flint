//! Windows' system folders, from the shell's known-folder API. Not from
//! %ProgramFiles% / %ProgramData%: a user can define those for themselves,
//! and a program run as administrator for that user would then use a
//! folder the user chose.

use std::io;
use std::path::PathBuf;
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::UI::Shell::{
    FOLDERID_LocalAppData, FOLDERID_LocalAppDataLow, FOLDERID_ProgramData, FOLDERID_ProgramFiles,
    FOLDERID_Programs, FOLDERID_RoamingAppData, SHGetKnownFolderPath,
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

/// What a user-mode indexer walks: `roots`, listing the `skip` folders
/// without entering them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scope {
    pub roots: Vec<PathBuf>,
    pub skip: Vec<PathBuf>,
}

/// The user's own files: `profile` without the hidden application-data
/// folders (AppData\Local, \Roaming, \LocalLow). They hold program caches
/// and settings rather than files people look for, and most of a
/// profile's entries; Windows' own search leaves them out by default too.
/// The Start menu shortcuts kept there stay, so an app is found by its
/// name. `include_app_data` walks the whole profile.
pub fn user_scope(profile: PathBuf, include_app_data: bool) -> Scope {
    let mut s = Scope {
        roots: vec![profile],
        skip: Vec::new(),
    };
    if include_app_data {
        return s;
    }
    for id in [
        &FOLDERID_LocalAppData,
        &FOLDERID_RoamingAppData,
        &FOLDERID_LocalAppDataLow,
    ] {
        match known(id) {
            Ok(p) => s.skip.push(p),
            Err(e) => crate::log::line(format!("an app data folder is walked: {e}")),
        }
    }
    match known(&FOLDERID_Programs) {
        Ok(p) if p.is_dir() => s.roots.push(p),
        Ok(_) => {}
        Err(e) => crate::log::line(format!("the Start menu is not indexed: {e}")),
    }
    s
}

impl Scope {
    /// Also walks `extra`. A skipped folder inside one of them becomes a
    /// root as well, so a folder the user added is indexed in full.
    pub fn with_extra(mut self, extra: Vec<PathBuf>) -> Scope {
        let held: Vec<PathBuf> = self
            .skip
            .iter()
            .filter(|d| extra.iter().any(|f| crate::walk::is_within(d, f)))
            .cloned()
            .collect();
        self.roots.extend(extra);
        self.roots.extend(held);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::UI::Shell::{
        FOLDERID_LocalAppData, FOLDERID_LocalAppDataLow, FOLDERID_Profile, FOLDERID_Programs,
        FOLDERID_RoamingAppData,
    };

    #[test]
    fn user_scope_skips_app_data_but_keeps_the_start_menu() {
        let home = known(&FOLDERID_Profile).unwrap();
        let s = user_scope(home.clone(), false);
        assert_eq!(s.roots[0], home);
        for id in [
            &FOLDERID_LocalAppData,
            &FOLDERID_RoamingAppData,
            &FOLDERID_LocalAppDataLow,
        ] {
            assert!(s.skip.contains(&known(id).unwrap()), "{:?}", s.skip);
        }
        assert!(s.roots.contains(&known(&FOLDERID_Programs).unwrap()));

        let all = user_scope(home.clone(), true);
        assert_eq!(all.roots, vec![home]);
        assert!(all.skip.is_empty());
    }

    #[test]
    fn an_extra_folder_holding_a_skipped_folder_is_indexed_in_full() {
        let home = PathBuf::from(r"C:\Users\Deniz");
        let local = home.join(r"AppData\Local");
        let roaming = home.join(r"AppData\Roaming");
        let s = Scope {
            roots: vec![home.clone()],
            skip: vec![local.clone(), roaming.clone()],
        }
        .with_extra(vec![
            home.join("appdata"),
            home.join(r"AppData\Roaming\Oyun"),
        ]);
        assert_eq!(
            s.roots,
            vec![
                home.clone(),
                home.join("appdata"),
                home.join(r"AppData\Roaming\Oyun"),
                local.clone(),
                roaming.clone(),
            ]
        );
        assert_eq!(
            s.skip,
            vec![local, roaming],
            "the profile walk still lists them only"
        );
    }
}
