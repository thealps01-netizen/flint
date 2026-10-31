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
    /// Never shown to AI assistants (the MCP server, `flint-indexer
    /// query`); still shown in Flint's own results.
    pub ai_hidden_folders: Vec<PathBuf>,
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
            // the key stores in the user folder; the user may remove them
            ai_hidden_folders: flint_indexer::ask::default_ai_hidden().unwrap_or_default(),
            index_app_data: false,
            check_updates: true,
        }
    }
}

impl Settings {
    pub fn default_file() -> Option<PathBuf> {
        flint_indexer::ask::settings_file()
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

/// Whether a path lies in one of the excluded folders (whole folder
/// names, case-insensitive), and those folders in the form it compares.
pub use flint_indexer::ask::{folder_keys as exclusion_keys, is_within as is_excluded};

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
        assert_eq!(
            s.ai_hidden_folders,
            flint_indexer::ask::default_ai_hidden().unwrap()
        );
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
            ai_hidden_folders: vec![r"C:\Users\x\Private".into()],
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
    fn the_folders_hidden_from_assistants_are_read_from_what_the_app_saves() {
        // flint-indexer reads these two lists from the same file (the MCP
        // server and `flint-indexer query`): a renamed field would show
        // hidden folders to assistants
        let f = std::env::temp_dir()
            .join(format!("flint-settings-ai-{}", std::process::id()))
            .join("settings.json");
        let s = Settings {
            excluded_folders: vec![r"C:\Data".into()],
            ai_hidden_folders: vec![r"C:\Users\X\Private".into()],
            ..Settings::default()
        };
        s.save(&f).unwrap();
        let hidden = flint_indexer::ask::hidden_from_assistants(Some(&f)).unwrap();
        assert!(is_excluded(r"C:\Data\a.txt", &hidden));
        assert!(is_excluded(r"C:\Users\X\Private\b.txt", &hidden));
        // the app's own results leave out only the excluded folders
        assert_eq!(exclusion_keys(&s.excluded_folders), [r"c:\data"]);
    }
}
