//! Asking every running indexer at once, as the app sees them: the
//! service, a `serve --user` indexer and the app's own in-process ones
//! (drives without NTFS, the profile folder, app data). Answers are merged
//! by score, one hit per path; folders the user hid are dropped first.
//! The app, `flint-indexer query` and the MCP server all search through
//! here, so they see the same files.

use crate::protocol::WireHit;
use std::path::{Path, PathBuf};

/// Tags of the app's in-process indexers (their pipe and data folder names).
pub const TAG_DRIVES: &str = "drives";
pub const TAG_PROFILE: &str = "profile";
pub const TAG_APP_DATA: &str = "app data";
pub const APP_TAGS: [&str; 3] = [TAG_DRIVES, TAG_PROFILE, TAG_APP_DATA];

/// The pipe the app's in-process indexer `tag` serves for user `sid`.
#[cfg(windows)]
pub fn app_pipe_name(sid: &str, tag: &str) -> String {
    format!("{}-app-{tag}", crate::pipe::user_pipe_name(sid))
}

/// Merges answers: best score first (ties: shorter path), one hit per path
/// (paths compare case-insensitively, as on Windows), at most `limit`.
pub fn merge(mut hits: Vec<WireHit>, limit: usize) -> Vec<WireHit> {
    hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.path.len().cmp(&b.path.len())));
    let mut seen = std::collections::HashSet::new();
    hits.retain(|h| seen.insert(h.path.to_lowercase()));
    hits.truncate(limit);
    hits
}

/// Whether `path` lies in one of `folders` (keys from [`folder_keys`]):
/// case-insensitive, whole folder names only (`C:\Data` covers
/// `C:\Data\x`, not `C:\Database`).
pub fn is_within(path: &str, folders: &[String]) -> bool {
    if folders.is_empty() {
        return false;
    }
    let p = path.to_lowercase();
    folders.iter().any(|f| {
        p.strip_prefix(f.as_str())
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('\\'))
    })
}

/// Folders in the form [`is_within`] compares.
pub fn folder_keys(folders: &[PathBuf]) -> Vec<String> {
    folders
        .iter()
        .map(|f| f.to_string_lossy().trim_end_matches('\\').to_lowercase())
        .filter(|f| !f.is_empty())
        .collect()
}

/// The app's settings file, `%APPDATA%\Flint\settings.json`.
pub fn settings_file() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|d| PathBuf::from(d).join("Flint").join("settings.json"))
}

/// Folders in the user folder hidden from assistants unless the user
/// removes them: where keys and sign-in data for servers and cloud
/// accounts are kept, whose file names alone tell what is stored where.
pub const DEFAULT_AI_HIDDEN: [&str; 6] = [".ssh", ".gnupg", ".aws", ".azure", ".kube", ".docker"];

/// [`DEFAULT_AI_HIDDEN`] in this user's folder (`%USERPROFILE%`, else
/// the shell's profile folder); an error when neither is known, so
/// nothing is shown rather than them.
pub fn default_ai_hidden() -> std::io::Result<Vec<PathBuf>> {
    let home = match std::env::var_os("USERPROFILE") {
        Some(h) => PathBuf::from(h),
        #[cfg(windows)]
        None => crate::folders::profile()?,
        #[cfg(not(windows))]
        None => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "the user folder is not known (USERPROFILE is not set)",
            ));
        }
    };
    Ok(DEFAULT_AI_HIDDEN
        .iter()
        .map(|d| PathBuf::from(&home).join(d))
        .collect())
}

/// The two lists of the settings file this module reads (the app's
/// `Settings` writes them; a test there checks the names still match).
/// A file without the second list (or no file) gets the defaults.
#[derive(serde::Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Hidden {
    excluded_folders: Vec<PathBuf>,
    ai_hidden_folders: Option<Vec<PathBuf>>,
}

fn invalid(m: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, m)
}

/// A folder as the index spells paths: `\` separators, no `\\?\` prefix
/// (`\\?\UNC\srv\x` is `\\srv\x`). Only absolute folders (`X:\...` or
/// `\\server\...`) without `.` or `..` parts are accepted: anything else
/// would match nothing. The error does not repeat the entry (it may name
/// what the user hides, and an assistant reads it).
pub(crate) fn normalized(entry: &Path) -> Result<String, std::io::Error> {
    let raw = entry.to_string_lossy().replace('/', "\\");
    let s = if let Some(unc) = raw.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else {
        raw.strip_prefix(r"\\?\").unwrap_or(&raw).to_string()
    };
    let b = s.as_bytes();
    let drive = b.len() >= 2
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && (b.len() == 2 || b[2] == b'\\');
    let unc =
        s.len() > 2 && s.starts_with(r"\\") && !s.starts_with(r"\\.\") && !s.starts_with(r"\\?\");
    let dots = s.split('\\').any(|part| part == "." || part == "..");
    if (drive || unc) && !dots {
        Ok(s)
    } else {
        Err(invalid(
            "a folder in Flint's settings is not a full path such as C:\\Users\\me\\Private".into(),
        ))
    }
}

/// Whether `path` is on a local disk, where its real location can be
/// looked up at once (a network share may keep a lookup waiting for
/// tens of seconds).
#[cfg(windows)]
fn on_local_disk(path: &str) -> bool {
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
    let b = path.as_bytes();
    if b.len() < 2 || b[1] != b':' {
        return false;
    }
    let root: Vec<u16> = format!("{}:\\", b[0] as char)
        .encode_utf16()
        .chain([0])
        .collect();
    // SAFETY: root is NUL-terminated. 2 is DRIVE_REMOVABLE, 3 DRIVE_FIXED.
    matches!(unsafe { GetDriveTypeW(root.as_ptr()) }, 2 | 3)
}

/// What an AI assistant must never be shown, from the settings `file`:
/// the folders excluded from every result and those hidden from
/// assistants, each also under its real location when it was picked
/// through a junction, link or mounted folder. A missing file hides
/// the default folders ([`DEFAULT_AI_HIDDEN`]); no settings folder (`None`), an unreadable or damaged file, or
/// an entry that is not a full path is an error, so a search that cannot
/// tell what is hidden shows nothing.
pub fn hidden_from_assistants(file: Option<&Path>) -> std::io::Result<Vec<String>> {
    let Some(file) = file else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Flint's settings folder cannot be found (APPDATA is not set)",
        ));
    };
    let h: Hidden = match std::fs::read(file) {
        // only where: serde's text can quote the value, a folder name
        Ok(t) => serde_json::from_slice(&t).map_err(|e| {
            invalid(format!(
                "settings.json is damaged at line {}, column {}",
                e.line(),
                e.column()
            ))
        })?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Hidden::default(),
        Err(e) => return Err(e),
    };
    let ai = match h.ai_hidden_folders {
        Some(f) => f,
        None => default_ai_hidden()?,
    };
    let mut folders = Vec::new();
    for entry in h.excluded_folders.iter().chain(&ai) {
        let n = normalized(entry)?;
        #[cfg(windows)]
        if on_local_disk(&n)
            && let Some(real) = crate::privacy::os::final_path(&n)
        {
            folders.push(PathBuf::from(real));
        }
        folders.push(PathBuf::from(n));
    }
    let mut keys = folder_keys(&folders);
    keys.sort();
    keys.dedup();
    Ok(keys)
}

/// [`hidden_from_assistants`] for this user's settings file. When the
/// process was given another `%APPDATA%` than the shell's own (a sandbox,
/// a redirect), the file in both is read and both lists are hidden.
pub fn hidden_for_assistants() -> std::io::Result<Vec<String>> {
    let mut keys = hidden_from_assistants(settings_file().as_deref())?;
    #[cfg(windows)]
    if let Ok(d) = crate::folders::roaming_app_data() {
        let shell = d.join("Flint").join("settings.json");
        if settings_file().as_deref() != Some(shell.as_path()) {
            keys.extend(hidden_from_assistants(Some(&shell))?);
            keys.sort();
            keys.dedup();
        }
    }
    Ok(keys)
}

/// A merged answer.
#[derive(Debug)]
pub struct Found {
    pub hits: Vec<WireHit>,
    /// Matches the indexers counted. With hidden folders, only the hits
    /// that were fetched and kept are counted, so the number tells nothing
    /// about the hidden ones.
    pub total: u64,
    /// Whether `total` is the count of all matches; else it is a lower
    /// bound (hidden folders, and an index returned a full window).
    pub exact: bool,
    /// An index returned as many hits as it was asked for: more matches
    /// were left out than the merged list shows.
    pub window_full: bool,
    /// Indexes that were there but did not answer in time or were still
    /// starting: the hits may be missing theirs.
    pub failed: usize,
}

/// With hidden folders, each indexer is asked for this many times the
/// limit (at most `MAX_ASK`), as the app does for its excluded folders.
#[cfg(windows)]
const HIDDEN_OVERFETCH: u32 = 4;
#[cfg(windows)]
const MAX_ASK: u32 = crate::protocol::MAX_LIMIT;

/// An index slower than this to connect, greet and answer counts as not
/// answering (answers take ~10 ms). Each read waits at most what is left
/// of it, so a slow index costs about this long.
#[cfg(windows)]
const READ_TIMEOUT_MS: u32 = 2000;
/// No index is asked after this long; one already asked may take its
/// [`READ_TIMEOUT_MS`] on top.
#[cfg(windows)]
const SEARCH_BUDGET: std::time::Duration = std::time::Duration::from_secs(4);

/// Asks each `(pipe, owner)` for `text`, drops hits inside `hidden`, and
/// merges the rest to at most `limit`. A pipe that is not there or belongs
/// to another account is skipped; one that is there but too slow is
/// skipped and counted in [`Found::failed`]. When none answers, the error
/// says whether Flint is not running, still starting or not answering.
#[cfg(windows)]
pub fn search_pipes(
    pipes: &[(String, String)],
    text: &str,
    limit: u32,
    hidden: &[String],
) -> std::io::Result<Found> {
    use crate::protocol::ServerMsg;
    use std::io::ErrorKind;
    let start = std::time::Instant::now();
    let (mut hits, mut total, mut answered, mut failed, mut starting) =
        (Vec::new(), 0u64, 0, 0usize, false);
    let (mut full, mut refused) = (false, None::<String>);
    // hidden hits are dropped after each indexer cut to its limit; ask for
    // more so the list still fills behind them
    let ask = if hidden.is_empty() {
        limit
    } else {
        limit.saturating_mul(HIDDEN_OVERFETCH).min(MAX_ASK)
    };
    for (name, owner) in pipes {
        let left = SEARCH_BUDGET.saturating_sub(start.elapsed()).as_millis() as u32;
        let wait = left.min(READ_TIMEOUT_MS);
        if wait == 0 {
            failed += 1;
            continue;
        }
        let asked_at = std::time::Instant::now();
        let c = match crate::pipe::Client::connect_within(name, owner, wait) {
            Ok(c) => c,
            // not running, or not Flint's (refused): not an index of ours
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::PermissionDenied) => {
                continue;
            }
            Err(e) => {
                starting |= e.to_string().starts_with("starting:");
                failed += 1;
                continue;
            }
        };
        // the answer gets what is left of this index's time
        let left = u128::from(wait).saturating_sub(asked_at.elapsed().as_millis()) as u32;
        if left == 0 {
            failed += 1;
            continue;
        }
        crate::server::Conn::set_read_timeout(&c.conn, Some(left));
        match c.query(text, ask, Vec::new()) {
            Ok(ServerMsg::Results {
                hits: h, total: t, ..
            }) => {
                answered += 1;
                full |= h.len() as u32 >= ask.min(crate::protocol::MAX_LIMIT);
                let kept: Vec<WireHit> = h
                    .into_iter()
                    .filter(|h| !is_within(&h.path, hidden))
                    .collect();
                total += if hidden.is_empty() {
                    u64::from(t)
                } else {
                    // the indexer's own count would include hidden matches
                    // beyond those fetched
                    kept.len() as u64
                };
                hits.extend(kept);
            }
            Ok(_) => answered += 1,
            Err(e) => {
                let e = e.to_string();
                if let Some(why) = e.strip_prefix("query: ") {
                    // the query itself was refused (each index would)
                    refused = Some(why.to_string());
                } else {
                    starting |= e.starts_with("starting:");
                    failed += 1;
                }
            }
        }
    }
    if answered == 0
        && let Some(why) = refused
    {
        return Err(std::io::Error::new(ErrorKind::InvalidInput, why));
    }
    if answered == 0 {
        return Err(std::io::Error::new(
            ErrorKind::NotConnected,
            if starting {
                "Flint's index is still starting; try again in a few seconds"
            } else if failed > 0 {
                "Flint's index did not answer in time; try again in a moment"
            } else {
                "Flint is not running (no index answered); start Flint and try again"
            },
        ));
    }
    Ok(Found {
        hits: merge(hits, limit as usize),
        total,
        exact: hidden.is_empty() || !full,
        window_full: full,
        failed,
    })
}

/// Every pipe a Flint index of this user can answer on: the service
/// (owned by SYSTEM), `serve --user`, and the app's in-process indexers.
#[cfg(windows)]
pub fn running_pipes() -> std::io::Result<Vec<(String, String)>> {
    use crate::pipe::{SYSTEM_SID, current_user_sid, user_pipe_name};
    let me = current_user_sid()?;
    let mut pipes = vec![
        (
            crate::protocol::PIPE_NAME.to_string(),
            SYSTEM_SID.to_string(),
        ),
        (user_pipe_name(&me), me.clone()),
    ];
    pipes.extend(APP_TAGS.iter().map(|t| (app_pipe_name(&me, t), me.clone())));
    Ok(pipes)
}

/// [`search_pipes`] over [`running_pipes`].
#[cfg(windows)]
pub fn search_everywhere(text: &str, limit: u32, hidden: &[String]) -> std::io::Result<Found> {
    search_pipes(&running_pipes()?, text, limit, hidden)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(path: &str, score: i64) -> WireHit {
        WireHit {
            path: path.into(),
            name: path.rsplit('\\').next().unwrap().into(),
            is_dir: false,
            typo: false,
            highlights: vec![],
            last_change: 0,
            score,
        }
    }

    fn paths(h: &[WireHit]) -> Vec<&str> {
        h.iter().map(|h| h.path.as_str()).collect()
    }

    #[test]
    fn merge_orders_by_score_then_shorter_path_and_keeps_one_per_path() {
        let m = merge(
            vec![
                hit(r"C:\b\rapor-eski.txt", 40),
                hit(r"D:\x\rapor.txt", 60),
                hit(r"C:\a\rapor.txt", 90),
                hit(r"D:\rapor.txt", 60),
                hit(r"c:\A\RAPOR.TXT", 10),
            ],
            10,
        );
        assert_eq!(
            paths(&m),
            [
                r"C:\a\rapor.txt",
                r"D:\rapor.txt",
                r"D:\x\rapor.txt",
                r"C:\b\rapor-eski.txt"
            ]
        );
        assert_eq!(merge(m, 2).len(), 2);
    }

    #[test]
    fn within_matches_whole_folder_names_only() {
        let ex = folder_keys(&[r"C:\Data\".into(), r"C:\Users\X\node_modules".into()]);
        assert!(is_within(r"C:\Data", &ex));
        assert!(is_within(r"C:\Data\x", &ex));
        assert!(is_within(r"c:\DATA\x\y.txt", &ex));
        assert!(!is_within(r"C:\Database\x", &ex));
        assert!(!is_within(r"C:\Dat", &ex));
        assert!(is_within(r"C:\Users\X\node_modules\a\b.js", &ex));
        assert!(!is_within(r"C:\Users\X\node_modules2\b.js", &ex));
        let root = folder_keys(&[r"E:\".into()]);
        assert!(is_within(r"E:\a.txt", &root));
        assert!(!is_within(r"EE:\a.txt", &root));
        assert!(!is_within(r"C:\a.txt", &[]));
        // an empty entry hides nothing (it would match every path)
        assert!(folder_keys(&["".into(), r"\".into()]).is_empty());
    }

    fn settings(tag: &str, body: Option<&[u8]>) -> PathBuf {
        let d = std::env::temp_dir().join(format!("flint-ask-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("settings.json");
        if let Some(b) = body {
            std::fs::write(&f, b).unwrap();
        }
        f
    }

    #[test]
    fn hidden_folders_are_both_lists_from_the_settings_file() {
        let f = settings(
            "both",
            Some(br#"{"hotkey":"Alt+Space","excludedFolders":["C:\\Data"],"aiHiddenFolders":["C:\\Users\\X\\Private\\"]}"#),
        );
        let h = hidden_from_assistants(Some(&f)).unwrap();
        assert!(is_within(r"C:\Data\a.txt", &h));
        assert!(is_within(r"C:\Users\X\Private\b.txt", &h));
        assert!(!is_within(r"C:\Users\X\b.txt", &h));
    }

    #[test]
    fn a_missing_settings_file_hides_the_defaults_and_a_damaged_one_is_an_error() {
        let h = |f: PathBuf| hidden_from_assistants(Some(&f));
        let defaults = folder_keys(&default_ai_hidden().unwrap());
        assert_eq!(defaults.len(), DEFAULT_AI_HIDDEN.len());
        // no file, or one from before the setting: the default folders
        // (and their real locations, when one is reached through a link)
        for got in [
            h(settings("none", None)).unwrap(),
            h(settings("old", Some(br#"{"hotkey":"Alt+Space"}"#))).unwrap(),
        ] {
            assert!(defaults.iter().all(|d| got.contains(d)), "{}", got.len());
        }
        // the user removed them all: nothing
        assert!(
            h(settings("empty", Some(br#"{"aiHiddenFolders":[]}"#)))
                .unwrap()
                .is_empty()
        );
        assert!(h(settings("bad", Some(b"{\"excludedFolders\":["))).is_err());
        assert!(h(settings("type", Some(br#"{"aiHiddenFolders":"C:\\x"}"#))).is_err());
        // no settings folder at all (APPDATA unset): nothing is shown
        assert!(hidden_from_assistants(None).is_err());
    }

    #[test]
    fn the_default_folders_are_the_key_stores_in_the_user_folder() {
        let home = PathBuf::from(std::env::var_os("USERPROFILE").expect("set on Windows"));
        let d = default_ai_hidden().unwrap();
        assert!(d.contains(&home.join(".ssh")));
        assert!(d.contains(&home.join(".aws")));
        assert!(d.iter().all(|f| f.parent() == Some(home.as_path())));
    }

    #[test]
    fn hidden_entries_in_other_spellings_still_hide_and_relative_ones_are_errors() {
        let body = serde_json::json!({
            "aiHiddenFolders": ["C:/Users/X/Fwd", r"\\?\C:\Long\Path", r"\\?\UNC\srv\share\Gizli", r"\\srv2\s"]
        });
        let f = settings("spell", Some(body.to_string().as_bytes()));
        let h = hidden_from_assistants(Some(&f)).unwrap();
        assert!(is_within(r"C:\Users\X\Fwd\a.txt", &h));
        assert!(is_within(r"C:\Long\Path\a.txt", &h));
        assert!(is_within(r"\\srv\share\Gizli\a.txt", &h));
        assert!(is_within(r"\\srv2\s\a.txt", &h));
        for bad in ["Private", r"C:Private", r"\Private", ""] {
            let body = serde_json::json!({ "excludedFolders": [bad] });
            let f = settings("rel", Some(body.to_string().as_bytes()));
            assert!(hidden_from_assistants(Some(&f)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_settings_file_is_in_appdata_flint() {
        let f = settings_file().expect("APPDATA is set on Windows");
        assert!(f.ends_with(r"Flint\settings.json"));
    }

    #[cfg(windows)]
    #[test]
    fn app_pipes_are_per_user_and_per_tag() {
        assert_eq!(
            app_pipe_name("S-1-5-21-1", TAG_APP_DATA),
            r"\\.\pipe\flint-index-S-1-5-21-1-app-app data"
        );
    }
}
