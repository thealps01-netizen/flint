//! Flint's updates. A release build is made with the update address and
//! the public key of the release signing key in its Tauri configuration
//! (`plugins.updater.endpoints` and `.pubkey`, passed with `tauri build
//! --config` by the release workflow; the CLI signs the installer with the
//! matching private key). A build without them (a development build,
//! someone's own build) never checks. Nothing in the repository names a
//! repository or holds a key.
//!
//! A check runs a minute after start and then once a day while the
//! setting is on (looked at hourly, so turning it on takes effect soon); a failed check (offline, the address unreachable) is
//! only logged. Installing downloads the signed installer, checks its
//! signature (and that it was signed for the version announced) and runs
//! it (one administrator prompt); Flint then exits.

use crate::log;
use serde::Serialize;
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::UpdaterExt;

const FIRST_CHECK: Duration = Duration::from_secs(60);
const EVERY: Duration = Duration::from_secs(24 * 3600);
/// How often the background thread looks whether a check is due (so
/// turning the setting on takes effect within this time).
const TICK: Duration = Duration::from_secs(3600);
/// A check that gets no answer in this time fails (a stalled connection
/// must not leave "Check now" busy forever).
const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
/// The whole download of the installer (a few MB).
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// Set when Flint has saved and stopped its indexers for the installer.
static EXITING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the updater configuration has both an address and a key.
pub fn configured(updater: Option<&serde_json::Value>) -> bool {
    let Some(u) = updater else { return false };
    let key = u
        .get("pubkey")
        .and_then(|k| k.as_str())
        .is_some_and(|k| !k.trim().is_empty());
    let address = u
        .get("endpoints")
        .and_then(|e| e.as_array())
        .is_some_and(|e| {
            e.iter()
                .any(|x| x.as_str().is_some_and(|x| !x.trim().is_empty()))
        });
    key && address
}

/// Whether this build checks for updates.
pub fn built_in(app: &AppHandle) -> bool {
    configured(app.config().plugins.0.get("updater"))
}

/// A newer version found by the last check.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UpdateInfo {
    pub version: String,
    pub notes: String,
}

static FOUND: Mutex<Option<UpdateInfo>> = Mutex::new(None);

pub fn found() -> Option<UpdateInfo> {
    FOUND.lock().ok().and_then(|f| f.clone())
}

fn updater(app: &AppHandle, timeout: Duration) -> Result<tauri_plugin_updater::Updater, String> {
    if !built_in(app) {
        return Err("this build does not check for updates".into());
    }
    let handle = app.clone();
    app.updater_builder()
        .timeout(timeout)
        // the installer starts and Flint exits at once (no Exit event):
        // the in-process indexers save here, as on a normal quit
        .on_before_exit(move || {
            EXITING.store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(s) = handle.state::<crate::AppState>().sources.get() {
                s.shutdown();
            }
            handle.cleanup_before_exit();
        })
        .build()
        .map_err(|e| e.to_string())
}

/// Asks the update address for a newer version (and remembers it).
pub async fn check(app: &AppHandle) -> Result<Option<UpdateInfo>, String> {
    let update = updater(app, CHECK_TIMEOUT)?
        .check()
        .await
        .map_err(|e| e.to_string())?;
    let info = update.map(|u| UpdateInfo {
        version: u.version.clone(),
        notes: u.body.clone().unwrap_or_default(),
    });
    if let Ok(mut f) = FOUND.lock() {
        f.clone_from(&info);
    }
    Ok(info)
}

/// Downloads the newer version, checks its signature and runs its
/// installer; on Windows Flint exits while the installer runs.
pub async fn install(app: &AppHandle) -> Result<(), String> {
    let Some(update) = updater(app, DOWNLOAD_TIMEOUT)?
        .check()
        .await
        .map_err(|e| e.to_string())?
    else {
        if let Ok(mut f) = FOUND.lock() {
            *f = None;
        }
        return Err("no newer version is available".into());
    };
    log::line(format!("installing Flint {}", update.version));
    let r = update
        .download_and_install(|_, _| {}, || {})
        .await
        .map_err(|e| e.to_string());
    if let Err(e) = &r
        && EXITING.load(std::sync::atomic::Ordering::SeqCst)
    {
        // the indexers were stopped for the installer, which did not run
        // (its administrator prompt was declined): start Flint afresh
        log::line(format!("the update did not start ({e}); restarting Flint"));
        app.restart();
    }
    r
}

/// Checks in the background (see the module comment) while `wanted()`.
/// A found version is sent to the launcher as `flint://update`.
pub fn start(app: AppHandle, wanted: impl Fn() -> bool + Send + 'static) {
    if !built_in(&app) {
        log::line("updates: this build has no update address");
        return;
    }
    std::thread::spawn(move || {
        std::thread::sleep(FIRST_CHECK);
        let mut last: Option<std::time::Instant> = None;
        loop {
            if wanted() && due(last.map(|t| t.elapsed())) {
                last = Some(std::time::Instant::now());
                match tauri::async_runtime::block_on(check(&app)) {
                    Ok(Some(info)) => {
                        log::line(format!("update available: Flint {}", info.version));
                        let _ = app.emit("flint://update", info);
                    }
                    Ok(None) => {}
                    Err(e) => log::line(format!("update check failed: {e}")),
                }
            }
            std::thread::sleep(TICK);
        }
    });
}

/// Whether a background check is due: never checked yet (and the first
/// delay has passed), or the last one is a day old.
pub fn due(since_last: Option<Duration>) -> bool {
    since_last.is_none_or(|d| d >= EVERY)
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    #[test]
    fn a_check_is_due_once_a_day() {
        assert!(due(None), "never checked");
        assert!(!due(Some(Duration::from_secs(3600))));
        assert!(!due(Some(EVERY - Duration::from_secs(1))));
        assert!(due(Some(EVERY)));
    }

    #[test]
    fn no_update_source_without_both_values() {
        let url = "https://example.org/releases/latest/download/latest.json";
        assert!(!configured(None));
        // the repository's own configuration: an empty entry
        assert!(!configured(Some(&json!({"pubkey": "", "endpoints": []}))));
        assert!(!configured(Some(
            &json!({"pubkey": "KEY", "endpoints": []})
        )));
        assert!(!configured(Some(
            &json!({"pubkey": "  ", "endpoints": [url]})
        )));
        assert!(!configured(Some(&json!({"endpoints": [url]}))));
        assert!(!configured(Some(
            &json!({"pubkey": "KEY", "endpoints": [""]})
        )));
        assert!(configured(Some(
            &json!({"pubkey": "KEY", "endpoints": [url]})
        )));
    }
}
