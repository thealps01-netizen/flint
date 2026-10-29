//! The commands the launcher page calls. Thin: the work is in `shell`,
//! `history`, `icons` and `sources`.

use crate::history::History;
use crate::shell::ShellError;
use std::sync::Mutex;

/// Opens (or reveals) `path`; an open (`record`) is remembered for ranking,
/// a reveal is not. A file that is gone is forgotten and reported. Errors
/// are what the launcher shows.
pub fn act(
    path: &str,
    history: &Mutex<History>,
    run: impl FnOnce(&str) -> Result<(), ShellError>,
    now: u32,
    record: bool,
) -> Result<(), String> {
    let r = run(path);
    let Ok(mut h) = history.lock() else {
        return r.map_err(|e| e.to_string());
    };
    match &r {
        Ok(()) if record => h.record_open(path, now),
        Ok(()) => return Ok(()),
        Err(ShellError::Missing) => h.forget(path),
        Err(ShellError::Failed(why)) => crate::log::line(format!("could not open {path}: {why}")),
    }
    if let Err(e) = h.save() {
        crate::log::line(format!("history not saved: {e}"));
    }
    r.map_err(|e| e.to_string())
}

/// `flinticon://localhost/<url-encoded path>?dir=1&px=32` → the path, the
/// folder flag and the size.
pub fn parse_icon_uri(path_and_query: &str) -> Option<(String, bool, u32)> {
    let (p, q) = path_and_query
        .split_once('?')
        .unwrap_or((path_and_query, ""));
    let path = percent_decode(p.trim_start_matches('/'))?;
    let mut is_dir = false;
    let mut px = 32;
    for kv in q.split('&') {
        match kv.split_once('=') {
            Some(("dir", v)) => is_dir = v == "1",
            Some(("px", v)) => px = v.parse().ok()?,
            _ => {}
        }
    }
    (!path.is_empty()).then_some((path, is_dir, px))
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// How "Enable full indexing" ended.
#[derive(Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Enable {
    /// The service is installed and started.
    Done,
    /// The user said no to the administrator prompt (not an error).
    Declined,
}

/// Whether `dir` lies inside `program_files` (compared without case).
pub fn admin_only(dir: &std::path::Path, program_files: &std::path::Path) -> bool {
    let low = |p: &std::path::Path| p.to_string_lossy().trim_end_matches('\\').to_lowercase();
    low(dir).starts_with(&format!("{}\\", low(program_files)))
}

/// Windows' "The operation was cancelled by the user" (a declined UAC prompt).
pub const ERROR_CANCELLED: i32 = 1223;

/// Installs the FlintIndexer service with `flint-indexer.exe` from the
/// app's own folder, through one administrator prompt. `run_elevated`
/// starts a program as administrator and returns its exit code.
///
/// The program run as administrator (and installed as a LocalSystem
/// service) is whatever sits next to `flint.exe`, so it is run only from
/// inside `program_files`, which only administrators can change (the
/// installer refuses any other folder). Elsewhere (an unzipped copy, a
/// download) any program of the user could swap the file before the
/// prompt. `None` skips the check: debug builds only.
pub fn enable_full_indexing(
    app_exe: &std::path::Path,
    program_files: Option<&std::path::Path>,
    run_elevated: impl FnOnce(&std::path::Path, &str) -> std::io::Result<u32>,
) -> Result<Enable, String> {
    let dir = app_exe.parent().unwrap_or(std::path::Path::new("."));
    if let Some(pf) = program_files
        && !admin_only(dir, pf)
    {
        return Err(format!(
            "Full indexing can be turned on only for Flint installed in {} (this copy runs from {}). Install Flint with its installer.",
            pf.display(),
            dir.display()
        ));
    }
    let indexer = dir.join("flint-indexer.exe");
    if !indexer.is_file() {
        return Err(format!(
            "flint-indexer.exe was not found in {}. Reinstall Flint to enable full indexing.",
            dir.display()
        ));
    }
    match run_elevated(&indexer, "install-service") {
        Ok(0) => Ok(Enable::Done),
        Ok(code) => Err(format!(
            "The service could not be installed (exit code {code}). Details: %LOCALAPPDATA%\\Flint\\logs"
        )),
        Err(e) if e.raw_os_error() == Some(ERROR_CANCELLED) => Ok(Enable::Declined),
        Err(e) => Err(format!("The service could not be installed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_indexing_runs_nothing_from_a_folder_users_can_change() {
        // the program run as administrator must sit where only
        // administrators can write: Program Files
        let pf = std::path::Path::new(r"C:\Program Files");
        let mut ran = false;
        for exe in [
            r"D:\Apps\Flint\flint.exe",
            r"C:\Users\x\Downloads\Flint\flint.exe",
            r"C:\Program Files Evil\Flint\flint.exe",
            r"C:\flint.exe",
        ] {
            let r = enable_full_indexing(std::path::Path::new(exe), Some(pf), |_, _| {
                ran = true;
                Ok(0)
            });
            assert!(r.unwrap_err().contains("Program Files"), "{exe}");
        }
        assert!(!ran);
        assert!(admin_only(
            std::path::Path::new(r"c:\PROGRAM FILES\Flint"),
            pf
        ));
        assert!(admin_only(
            std::path::Path::new(r"C:\Program Files\Tools\Flint"),
            pf
        ));
        assert!(!admin_only(std::path::Path::new(r"C:\Program Files"), pf));
    }

    #[test]
    fn enable_full_indexing_without_indexer_reports() {
        let dir = std::env::temp_dir().join(format!("flint-enable-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("flint.exe");
        let mut ran = false;
        let err = enable_full_indexing(&exe, Some(&std::env::temp_dir()), |_, _| {
            ran = true;
            Ok(0)
        })
        .unwrap_err();
        assert!(!ran, "nothing is started without the indexer");
        assert!(
            err.contains("flint-indexer.exe") && err.contains(&dir.display().to_string()),
            "{err}"
        );
        // with the indexer beside the app: the outcomes
        std::fs::write(dir.join("flint-indexer.exe"), b"").unwrap();
        let mut seen = None;
        let r = enable_full_indexing(&exe, Some(&std::env::temp_dir()), |p, args| {
            seen = Some((p.to_path_buf(), args.to_string()));
            Ok(0)
        });
        assert_eq!(r, Ok(Enable::Done));
        assert_eq!(
            seen,
            Some((dir.join("flint-indexer.exe"), "install-service".to_string()))
        );
        let declined = enable_full_indexing(&exe, Some(&std::env::temp_dir()), |_, _| {
            Err(std::io::Error::from_raw_os_error(ERROR_CANCELLED))
        });
        assert_eq!(declined, Ok(Enable::Declined));
        let failed =
            enable_full_indexing(&exe, Some(&std::env::temp_dir()), |_, _| Ok(2)).unwrap_err();
        assert!(failed.contains("exit code 2"), "{failed}");
        let other = enable_full_indexing(&exe, Some(&std::env::temp_dir()), |_, _| {
            Err(std::io::Error::from_raw_os_error(2))
        })
        .unwrap_err();
        assert!(!other.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn history(tag: &str) -> Mutex<History> {
        let d = std::env::temp_dir().join(format!("flint-cmd-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        Mutex::new(History::load(&d.join("h.json")))
    }

    #[test]
    fn open_records_the_file() {
        let h = history("open");
        assert_eq!(
            act(r"C:\a.txt", &h, |_| Ok(()), 1_800_000_000, true),
            Ok(())
        );
        assert_eq!(h.lock().unwrap().len(), 1);
    }

    #[test]
    fn reveal_is_not_counted_as_an_open() {
        let h = history("reveal");
        assert_eq!(
            act(r"C:\a.txt", &h, |_| Ok(()), 1_800_000_000, false),
            Ok(())
        );
        assert_eq!(h.lock().unwrap().len(), 0);
    }

    #[test]
    fn open_missing_file_reports_and_forgets() {
        let h = history("missing");
        h.lock().unwrap().record_open(r"C:\gone.txt", 1_799_000_000);
        let r = act(
            r"C:\GONE.txt",
            &h,
            |_| Err(ShellError::Missing),
            1_800_000_000,
            true,
        );
        assert_eq!(r, Err("File no longer exists".into()));
        assert_eq!(h.lock().unwrap().len(), 0, "forgotten");
    }

    #[test]
    fn a_failed_open_is_reported_and_not_recorded() {
        let h = history("failed");
        let r = act(
            r"C:\a.xyz",
            &h,
            |_| Err(ShellError::Failed("no app".into())),
            1,
            true,
        );
        assert_eq!(r, Err("no app".into()));
        assert_eq!(h.lock().unwrap().len(), 0);
    }

    #[test]
    fn icon_uris() {
        assert_eq!(
            parse_icon_uri("/C%3A%5CUsers%5Cx%5Cb%C3%BCt%C3%A7e%202026.xlsx?dir=0&px=32"),
            Some((r"C:\Users\x\bütçe 2026.xlsx".into(), false, 32))
        );
        assert_eq!(
            parse_icon_uri("/C%3A%5CData?dir=1&px=64"),
            Some((r"C:\Data".into(), true, 64))
        );
        assert_eq!(
            parse_icon_uri("/C%3A%5Ca.txt"),
            Some((r"C:\a.txt".into(), false, 32))
        );
        assert_eq!(parse_icon_uri("/"), None);
        assert_eq!(parse_icon_uri("/C%3"), None, "a cut escape");
        assert_eq!(parse_icon_uri("/C%3A%5Ca.txt?px=big"), None);
    }
}
