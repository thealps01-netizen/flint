//! User settings: `%APPDATA%\Flint\settings.json`.
//!
//! Defaults are neutral (nothing taken from the machine that built the
//! app): Alt+Space, English, the Windows theme, no extra or excluded
//! folders. A missing, damaged or unreadable file gives the defaults and a
//! log line; a field missing from an older file takes its default.

use crate::hotkey::{self, Hotkey, HotkeyError};
use serde::{Deserialize, Serialize};
use std::io;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    System,
    Dark,
    Light,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    #[default]
    En,
    Tr,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub hotkey: String,
    pub launch_at_startup: bool,
    pub theme: Theme,
    pub language: Lang,
    pub use_windows_recent: bool,
    /// Indexed in user mode besides the profile folder (from the next start).
    pub extra_folders: Vec<PathBuf>,
    /// Never shown in results.
    pub excluded_folders: Vec<PathBuf>,
    /// Basic mode walks the hidden app data folders too (from the next
    /// start). Off: they hold program caches, not the user's files.
    pub index_app_data: bool,
    /// Look for a new version once a day (release builds only).
    pub check_updates: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            hotkey: hotkey::DEFAULT.to_string(),
            launch_at_startup: false,
            theme: Theme::System,
            language: Lang::En,
            use_windows_recent: true,
            extra_folders: Vec::new(),
            excluded_folders: Vec::new(),
            index_app_data: false,
            check_updates: true,
        }
    }
}

impl Settings {
    pub fn default_file() -> Option<PathBuf> {
        std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("Flint").join("settings.json"))
    }

    pub fn load(file: &Path) -> Settings {
        let text = match std::fs::read(file) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Settings::default(),
            Err(e) => {
                crate::log::line(format!("settings not read, defaults used: {e}"));
                return Settings::default();
            }
        };
        serde_json::from_slice(&text).unwrap_or_else(|e| {
            crate::log::line(format!("settings damaged, defaults used: {e}"));
            Settings::default()
        })
    }

    /// Writes a temporary file and renames it over the old one, so a
    /// failed write never leaves half a file.
    pub fn save(&self, file: &Path) -> io::Result<()> {
        if let Some(d) = file.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = file.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, file)
    }
}

/// Whether `path` lies in one of `excluded` (case-insensitive, whole
/// folder names only: `C:\Data` covers `C:\Data\x`, not `C:\Database`).
pub fn is_excluded(path: &str, excluded: &[String]) -> bool {
    if excluded.is_empty() {
        return false;
    }
    let p = path.to_lowercase();
    excluded.iter().any(|ex| {
        p.strip_prefix(ex.as_str())
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('\\'))
    })
}

/// Excluded folders in the form `is_excluded` compares.
pub fn exclusion_keys(folders: &[PathBuf]) -> Vec<String> {
    folders
        .iter()
        .map(|f| f.to_string_lossy().trim_end_matches('\\').to_lowercase())
        .filter(|f| !f.is_empty())
        .collect()
}

/// Registers and unregisters global hotkeys (the shortcut plugin in the
/// app, a fake in tests).
pub trait Registrar {
    fn register(&mut self, hk: &Hotkey) -> Result<(), HotkeyError>;
    fn unregister(&mut self, hk: &Hotkey);
}

/// Moves the show/hide hotkey from `old` to `new`. The new key is taken
/// first, so a key another program owns leaves the old one working and
/// the error is returned for display next to the field.
pub fn change_hotkey(
    reg: &mut dyn Registrar,
    old: Option<&Hotkey>,
    new: &Hotkey,
) -> Result<(), HotkeyError> {
    if old == Some(new) {
        return Ok(());
    }
    reg.register(new)?;
    if let Some(old) = old {
        reg.unregister(old);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("flint-settings-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn defaults_are_neutral() {
        let s = Settings::default();
        assert_eq!(s.hotkey, "Alt+Space");
        assert_eq!(s.language, Lang::En);
        assert_eq!(s.theme, Theme::System);
        assert!(!s.launch_at_startup);
        assert!(s.use_windows_recent);
        assert!(s.check_updates);
        assert!(s.extra_folders.is_empty() && s.excluded_folders.is_empty());
        assert!(!s.index_app_data);
        // a missing file is the defaults too
        assert_eq!(Settings::load(&temp("missing").join("settings.json")), s);
    }

    #[test]
    fn saved_settings_load_back() {
        let f = temp("roundtrip").join("sub").join("settings.json");
        let s = Settings {
            hotkey: "Ctrl+Alt+K".into(),
            launch_at_startup: true,
            theme: Theme::Light,
            language: Lang::Tr,
            use_windows_recent: false,
            extra_folders: vec![r"D:\Projeler".into()],
            excluded_folders: vec![r"C:\Users\x\node_modules".into()],
            index_app_data: true,
            check_updates: false,
        };
        s.save(&f).unwrap();
        assert_eq!(Settings::load(&f), s);
        let json = std::fs::read_to_string(&f).unwrap();
        assert!(json.contains("\"language\": \"tr\""), "{json}");
    }

    #[test]
    fn damaged_settings_fall_back() {
        let d = temp("damaged");
        let f = d.join("settings.json");
        std::fs::write(&f, b"{ not json").unwrap();
        assert_eq!(Settings::load(&f), Settings::default());
        // an older file without some fields keeps what it has
        std::fs::write(&f, br#"{"language":"tr"}"#).unwrap();
        let s = Settings::load(&f);
        assert_eq!(s.language, Lang::Tr);
        assert_eq!(s.hotkey, "Alt+Space");
        assert!(s.use_windows_recent);
        // a file from before update checks existed: they are on
        assert!(s.check_updates);
        // an unknown value in a field falls back as a whole
        std::fs::write(&f, br#"{"theme":"purple"}"#).unwrap();
        assert_eq!(Settings::load(&f), Settings::default());
    }

    #[derive(Default)]
    struct FakeReg {
        held: Vec<String>,
        taken_elsewhere: Vec<String>,
    }

    impl Registrar for FakeReg {
        fn register(&mut self, hk: &Hotkey) -> Result<(), HotkeyError> {
            let k = hk.to_string();
            if self.taken_elsewhere.contains(&k) || self.held.contains(&k) {
                return Err(HotkeyError::InUse(k));
            }
            self.held.push(k);
            Ok(())
        }
        fn unregister(&mut self, hk: &Hotkey) {
            self.held.retain(|k| *k != hk.to_string());
        }
    }

    #[test]
    fn hotkey_conflict_keeps_old() {
        let old = hotkey::parse("Alt+Space").unwrap();
        let new = hotkey::parse("Ctrl+Alt+K").unwrap();
        let mut reg = FakeReg {
            taken_elsewhere: vec!["Ctrl+Alt+K".into()],
            ..FakeReg::default()
        };
        reg.register(&old).unwrap();
        let err = change_hotkey(&mut reg, Some(&old), &new).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Ctrl+Alt+K is already in use by another program"
        );
        assert_eq!(reg.held, ["Alt+Space"]);
    }

    #[test]
    fn hotkey_change_moves_registration() {
        let old = hotkey::parse("Alt+Space").unwrap();
        let new = hotkey::parse("Ctrl+Alt+K").unwrap();
        let mut reg = FakeReg::default();
        reg.register(&old).unwrap();
        change_hotkey(&mut reg, Some(&old), &new).unwrap();
        assert_eq!(reg.held, ["Ctrl+Alt+K"]);
        // the same key again changes nothing
        change_hotkey(&mut reg, Some(&new), &new).unwrap();
        assert_eq!(reg.held, ["Ctrl+Alt+K"]);
        // with no working hotkey before (it was taken at start), the new one is set
        let mut reg = FakeReg::default();
        change_hotkey(&mut reg, None, &old).unwrap();
        assert_eq!(reg.held, ["Alt+Space"]);
    }

    #[test]
    fn excluded_folder_filters_at_boundary() {
        let ex = exclusion_keys(&[r"C:\Data\".into(), r"c:\users\x\node_modules".into()]);
        assert!(is_excluded(r"C:\Data", &ex));
        assert!(is_excluded(r"C:\Data\x", &ex));
        assert!(is_excluded(r"c:\DATA\x\y.txt", &ex));
        assert!(!is_excluded(r"C:\Database\x", &ex));
        assert!(!is_excluded(r"C:\Dat", &ex));
        assert!(is_excluded(r"C:\Users\X\node_modules\a\b.js", &ex));
        assert!(!is_excluded(r"C:\Users\X\node_modules2\b.js", &ex));
        // a whole drive
        let root = exclusion_keys(&[r"E:\".into()]);
        assert!(is_excluded(r"E:\a.txt", &root));
        assert!(!is_excluded(r"EE:\a.txt", &root));
        assert!(!is_excluded(r"C:\a.txt", &[]));
    }
}
