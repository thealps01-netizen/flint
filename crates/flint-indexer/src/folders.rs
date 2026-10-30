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

/// The user's app data folders (AppData\Local, \Roaming, \LocalLow)
/// that exist.
pub fn app_data_folders() -> Vec<PathBuf> {
    [
        &FOLDERID_LocalAppData,
        &FOLDERID_RoamingAppData,
        &FOLDERID_LocalAppDataLow,
    ]
    .into_iter()
    .filter_map(|id| known(id).ok())
    .filter(|p| p.is_dir())
    .collect()
}

/// What the app walks itself beside the service, which leaves out the
/// contents of every `app_data` folder: all of them when `include` (the
/// "Search app data folders" setting), else the `extra` folders (Settings)
/// inside one of them and the ones inside an `extra` folder, so a folder
/// the user added is indexed in full, as in Basic mode.
pub fn beside_service(app_data: &[PathBuf], include: bool, extra: &[PathBuf]) -> Vec<PathBuf> {
    if include {
        return app_data.to_vec();
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for f in extra {
        if app_data.iter().any(|a| crate::walk::is_within(f, a)) {
            out.push(f.clone());
        }
    }
    for a in app_data {
        if extra.iter().any(|f| crate::walk::is_within(a, f)) {
            out.push(a.clone());
        }
    }
    // a folder inside another listed one is walked with it; of the same
    // folder spelled twice, the first is kept
    let mut kept: Vec<PathBuf> = Vec::new();
    for (i, f) in out.iter().enumerate() {
        let inside = out.iter().enumerate().any(|(j, g)| {
            let same = crate::walk::is_within(f, g) && crate::walk::is_within(g, f);
            if same {
                j < i
            } else {
                crate::walk::is_within(f, g)
            }
        });
        if !inside {
            kept.push(f.clone());
        }
    }
    kept
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

    #[test]
    fn beside_the_service_only_what_the_user_asked_for() {
        let ad: Vec<PathBuf> = [r"C:\U\x\AppData\Local", r"C:\U\x\AppData\Roaming"]
            .map(PathBuf::from)
            .to_vec();
        let p = |s: &[&str]| s.iter().map(PathBuf::from).collect::<Vec<_>>();
        assert_eq!(beside_service(&ad, true, &[]), ad);
        assert!(beside_service(&ad, false, &[]).is_empty());
        assert!(beside_service(&ad, false, &p(&[r"D:\Notes", r"C:\U\x\Documents"])).is_empty());
        assert_eq!(
            beside_service(
                &ad,
                false,
                &p(&[r"c:\u\x\appdata\roaming\.minecraft", r"D:\Notes"])
            ),
            p(&[r"c:\u\x\appdata\roaming\.minecraft"])
        );
        assert_eq!(
            beside_service(&ad, false, &p(&[r"C:\U\x"])),
            ad,
            "a folder holding app data"
        );
        assert_eq!(
            beside_service(
                &ad,
                false,
                &p(&[r"C:\U\x\AppData\Local\Game", r"C:\U\x\AppData"])
            ),
            ad,
            "nothing twice"
        );
        assert_eq!(
            beside_service(
                &ad,
                false,
                &p(&[r"C:\U\x\AppData\Local", r"C:\U\x\AppData\Roaming"])
            ),
            p(&[r"C:\U\x\AppData\Local", r"C:\U\x\AppData\Roaming"]),
            "an app data folder added itself"
        );
        assert_eq!(
            beside_service(&ad, false, &p(&[r"c:\u\x\appdata\local"])),
            p(&[r"c:\u\x\appdata\local"]),
            "spelled another way, still once"
        );
    }
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
