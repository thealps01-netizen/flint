//! flint.exe: the launcher. A pre-created hidden window on a global hotkey.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod autostart;
mod commands;
mod history;
mod hotkey;
mod icons;
mod log;
mod recent;
mod settings;
mod shell;
mod sources;
mod updates;
mod window;

use std::sync::{Arc, OnceLock};

use tauri::Manager;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

/// Registers `hk` as the show/hide hotkey, replacing none: the caller
/// unregisters the old one. A key another program owns is reported, never
/// swallowed.
pub fn register_hotkey(
    app: &tauri::AppHandle,
    hk: &hotkey::Hotkey,
) -> Result<(), hotkey::HotkeyError> {
    let acc = hk.accelerator();
    app.global_shortcut()
        .on_shortcut(acc.as_str(), |app, _sc, ev| {
            if ev.state == ShortcutState::Pressed {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis() as u64);
                window::LAST_HOTKEY_MS.store(now, std::sync::atomic::Ordering::Relaxed);
                window::toggle(app);
            }
        })
        .map_err(|e| {
            log::line(format!("hotkey {hk} not registered: {e}"));
            hotkey::HotkeyError::InUse(hk.to_string())
        })
}

/// App-wide state the commands share. `sources` is set once the
/// in-process indexers have started (in the background, so the window is
/// ready at once).
#[derive(Clone)]
pub struct AppState {
    pub sources: Arc<OnceLock<sources::Sources>>,
    pub history: Arc<std::sync::Mutex<history::History>>,
    pub icons: Arc<icons::Icons>,
    pub settings: Arc<std::sync::Mutex<settings::Settings>>,
    pub settings_file: Arc<std::path::PathBuf>,
    /// The show/hide hotkey that is registered now (`None`: none could be).
    pub hotkey: Arc<std::sync::Mutex<Option<hotkey::Hotkey>>>,
}

impl Default for AppState {
    fn default() -> Self {
        let file = history::History::default_file().unwrap_or_else(|| "history.json".into());
        let sfile = settings::Settings::default_file().unwrap_or_else(|| "settings.json".into());
        AppState {
            sources: Arc::default(),
            history: Arc::new(std::sync::Mutex::new(history::History::load(&file))),
            icons: Arc::default(),
            settings: Arc::new(std::sync::Mutex::new(settings::Settings::load(&sfile))),
            settings_file: Arc::new(sfile),
            hotkey: Arc::default(),
        }
    }
}

/// The global-shortcut plugin as `settings::Registrar`.
struct PluginRegistrar<'a>(&'a tauri::AppHandle);

impl settings::Registrar for PluginRegistrar<'_> {
    fn register(&mut self, hk: &hotkey::Hotkey) -> Result<(), hotkey::HotkeyError> {
        register_hotkey(self.0, hk)
    }
    fn unregister(&mut self, hk: &hotkey::Hotkey) {
        if let Err(e) = self
            .0
            .global_shortcut()
            .unregister(hk.accelerator().as_str())
        {
            log::line(format!("hotkey {hk} not released: {e}"));
        }
    }
}

pub fn unix_now() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as u32)
}

/// Adds Windows' recent items to the history ("Use Windows recent files",
/// on by default) and saves it.
fn seed_history(h: &std::sync::Mutex<history::History>) {
    let Some(dir) = recent::default_dir() else {
        return;
    };
    let items = recent::targets(&dir);
    let Ok(mut h) = h.lock() else { return };
    h.seed(&items);
    if let Err(e) = h.save() {
        log::line(format!("history not saved: {e}"));
    }
}

fn build_sources(extra_folders: &[std::path::PathBuf], index_app_data: bool) -> sources::Sources {
    // FLINT_OTHER_DRIVES replaces the detected list ("" = none), for test
    // runs on machines whose owner has not agreed to a drive being walked
    let detected = match std::env::var("FLINT_OTHER_DRIVES") {
        Ok(list) => {
            log::line(format!(
                "drives without NTFS taken from FLINT_OTHER_DRIVES: {list:?}"
            ));
            list.split(',')
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .map(String::from)
                .collect()
        }
        Err(_) => flint_indexer::volume::fixed_other_drives(),
    };
    let other: Vec<std::path::PathBuf> = detected
        .into_iter()
        .map(|d| std::path::PathBuf::from(format!("{d}\\")))
        .collect();
    let mut always = Vec::new();
    if !other.is_empty() {
        match sources::start_local("drives", other, Vec::new()) {
            Ok(b) => always.push(b),
            Err(e) => log::line(format!("drives without NTFS are not indexed: {e}")),
        }
    }
    // without the service: the profile folder (its app data folders only
    // when Settings asks) plus Settings' extra folders
    let extra = extra_folders.to_vec();
    let profile = Box::new(move || {
        let home = std::env::var_os("USERPROFILE").ok_or("USERPROFILE is not set")?;
        let scope = flint_indexer::folders::user_scope(home.into(), index_app_data)
            .with_extra(extra.iter().filter(|f| f.is_dir()).cloned().collect());
        sources::start_local("profile", scope.roots, scope.skip)
    });
    sources::Sources::new(Box::new(sources::ServicePipe::default()), always, profile)
}

#[tauri::command]
async fn search(
    state: tauri::State<'_, AppState>,
    seq: u64,
    text: String,
    limit: u32,
) -> Result<Option<sources::Answer>, String> {
    let s = state.sources.clone();
    let boosts = state
        .history
        .lock()
        .map(|h| h.boosts(unix_now(), history::MAX_BOOSTS))
        .unwrap_or_default();
    tauri::async_runtime::spawn_blocking(move || {
        s.get().ok_or("starting").map(|s| {
            s.query(seq, &text, limit, &boosts).map(|mut a| {
                a.opened = history::often(&a.hits, &boosts);
                a.icon_keys = a
                    .hits
                    .iter()
                    .map(|h| icons::icon_key(&h.path, h.is_dir))
                    .collect();
                a
            })
        })
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(str::to_string)
}

#[tauri::command]
async fn status(state: tauri::State<'_, AppState>) -> Result<sources::StatusInfo, String> {
    let s = state.sources.clone();
    tauri::async_runtime::spawn_blocking(move || s.get().ok_or("starting").map(|s| s.status()))
        .await
        .map_err(|e| e.to_string())?
        .map_err(str::to_string)
}

/// Opens the chosen result with its default app, as the user.
#[tauri::command]
async fn open(state: tauri::State<'_, AppState>, path: String) -> Result<(), String> {
    let h = state.history.clone();
    tauri::async_runtime::spawn_blocking(move || {
        commands::act(&path, &h, shell::open, unix_now(), true)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Shows the chosen result selected in Explorer.
#[tauri::command]
async fn reveal(state: tauri::State<'_, AppState>, path: String) -> Result<(), String> {
    let h = state.history.clone();
    tauri::async_runtime::spawn_blocking(move || {
        commands::act(&path, &h, shell::reveal, unix_now(), false)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
fn copy_path(app: tauri::AppHandle, path: String) -> Result<(), String> {
    use tauri_plugin_clipboard_manager::ClipboardExt;
    app.clipboard().write_text(path).map_err(|e| {
        log::line(format!("clipboard: {e}"));
        e.to_string()
    })
}

/// Size, date and type of a visible row, read as the user.
#[tauri::command]
async fn details(path: String) -> Result<Option<shell::Details>, String> {
    tauri::async_runtime::spawn_blocking(move || shell::details(&path))
        .await
        .map_err(|e| e.to_string())
}

/// The notice to show (a hotkey that could not be registered), for a page
/// that loaded after the event was sent.
#[tauri::command]
fn launcher_notice() -> Option<String> {
    window::notice()
}

/// A setting that could not be applied, shown next to its field.
#[derive(serde::Serialize)]
struct FieldError {
    field: &'static str,
    message: String,
}

/// What the Settings page shows about updates.
#[derive(serde::Serialize)]
struct UpdateState {
    supported: bool,
    version: String,
    found: Option<updates::UpdateInfo>,
}

#[tauri::command]
fn update_state(app: tauri::AppHandle) -> UpdateState {
    UpdateState {
        supported: updates::built_in(&app),
        version: app.package_info().version.to_string(),
        found: updates::found(),
    }
}

#[tauri::command]
async fn check_update(app: tauri::AppHandle) -> Result<Option<updates::UpdateInfo>, String> {
    let r = updates::check(&app).await;
    if let Err(e) = &r {
        log::line(format!("update check failed: {e}"));
    }
    r
}

#[tauri::command]
async fn install_update(app: tauri::AppHandle) -> Result<(), String> {
    let r = updates::install(&app).await;
    if let Err(e) = &r {
        log::line(format!("update not installed: {e}"));
    }
    r
}

#[tauri::command]
fn get_settings(state: tauri::State<'_, AppState>) -> settings::Settings {
    state.settings.lock().map(|s| s.clone()).unwrap_or_default()
}

/// Applies and saves `new`. A hotkey another program owns, or a startup
/// entry Windows refuses, keeps its old value; the rest still applies and
/// the error names the field.
#[tauri::command]
fn set_settings(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    mut new: settings::Settings,
) -> Result<settings::Settings, FieldError> {
    let mut err = None;
    let mut cur = state.settings.lock().map_err(|e| FieldError {
        field: "save",
        message: e.to_string(),
    })?;
    let applied = hotkey::parse(&new.hotkey).and_then(|hk| {
        let mut held = state
            .hotkey
            .lock()
            .map_err(|e| hotkey::HotkeyError::Invalid(e.to_string()))?;
        settings::change_hotkey(&mut PluginRegistrar(&app), held.as_ref(), &hk)?;
        new.hotkey = hk.to_string();
        *held = Some(hk);
        Ok(())
    });
    match applied {
        Ok(()) => window::clear_notice(&app),
        Err(e) => {
            new.hotkey = cur.hotkey.clone();
            err = Some(FieldError {
                field: "hotkey",
                message: e.to_string(),
            });
        }
    }
    if new.launch_at_startup != cur.launch_at_startup {
        let set = std::env::current_exe()
            .and_then(|exe| autostart::set(autostart::RUN_KEY, new.launch_at_startup, &exe));
        if let Err(e) = set {
            log::line(format!("startup entry not changed: {e}"));
            new.launch_at_startup = cur.launch_at_startup;
            err = Some(FieldError {
                field: "launchAtStartup",
                message: e.to_string(),
            });
        }
    }
    if let Some(s) = state.sources.get() {
        s.set_excluded(settings::exclusion_keys(&new.excluded_folders));
    }
    if let Err(e) = new.save(&state.settings_file) {
        log::line(format!("settings not saved: {e}"));
        err = Some(FieldError {
            field: "save",
            message: e.to_string(),
        });
    }
    *cur = new.clone();
    drop(cur);
    window::apply_settings(&app, &new);
    match err {
        Some(e) => Err(e),
        None => Ok(new),
    }
}

/// "Clear history": forgets every file opened through Flint.
#[tauri::command]
fn clear_history(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let mut h = state.history.lock().map_err(|e| e.to_string())?;
    h.clear(unix_now());
    h.save().map_err(|e| {
        log::line(format!("history not saved: {e}"));
        e.to_string()
    })
}

#[tauri::command]
async fn open_settings(app: tauri::AppHandle) {
    window::open_settings(&app);
}

/// "Enable full indexing" (Basic mode banner): one administrator prompt
/// installs and starts the service; Sources reconnects to it by itself.
#[tauri::command]
async fn enable_full_indexing(app: tauri::AppHandle) -> Result<commands::Enable, String> {
    let r = tauri::async_runtime::spawn_blocking(|| {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        // debug builds run from the build folder; release builds only
        // from Program Files (see commands::enable_full_indexing)
        let pf = if cfg!(debug_assertions) {
            None
        } else {
            Some(flint_indexer::folders::program_files().map_err(|e| e.to_string())?)
        };
        let r = commands::enable_full_indexing(&exe, pf.as_deref(), shell::run_elevated);
        log::line(format!("enable full indexing: {r:?}"));
        r
    })
    .await
    .map_err(|e| e.to_string())?;
    // Windows' prompt took the focus and hid the launcher: show it again,
    // with the outcome
    window::show(&app);
    r
}

/// When the show/hide hotkey was last pressed (ms since 1970), for the
/// hotkey-to-visible gate (tests/gate.mjs).
#[tauri::command]
fn gate_hotkey_ms() -> u64 {
    window::LAST_HOTKEY_MS.load(std::sync::atomic::Ordering::Relaxed)
}

fn main() {
    log::init();
    let built = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // a second start shows the running launcher
            window::show(app);
        }))
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .register_asynchronous_uri_scheme_protocol("flinticon", |ctx, req, responder| {
            // row icons load like images after the rows paint
            let icons = ctx.app_handle().state::<AppState>().icons.clone();
            let pq = req
                .uri()
                .path_and_query()
                .map(|p| p.as_str().to_string())
                .unwrap_or_default();
            std::thread::spawn(move || {
                let png =
                    commands::parse_icon_uri(&pq).and_then(|(p, dir, px)| icons.png(&p, dir, px));
                let res = match png {
                    Some(bytes) => tauri::http::Response::builder()
                        .header("Content-Type", "image/png")
                        .header("Cache-Control", "max-age=3600")
                        .body(bytes.to_vec()),
                    None => tauri::http::Response::builder()
                        .status(404)
                        .body(Vec::new()),
                };
                match res {
                    Ok(r) => responder.respond(r),
                    Err(e) => log::line(format!("icon response: {e}")),
                }
            });
        })
        .manage(AppState::default())
        .setup(|app| {
            let handle = app.handle().clone();
            let state: AppState = (*app.state::<AppState>()).clone();
            let st = state.settings.lock().map(|s| s.clone()).unwrap_or_default();
            {
                let state = state.clone();
                let st = st.clone();
                std::thread::spawn(move || {
                    // search first: reading Windows' recent items can wait
                    // on network paths, and must not hold up the first query
                    let _ = state
                        .sources
                        .set(build_sources(&st.extra_folders, st.index_app_data));
                    if st.use_windows_recent {
                        let state = state.clone();
                        std::thread::spawn(move || seed_history(&state.history));
                    }
                    // the current exclusions (Settings may have changed meanwhile)
                    if let (Some(s), Ok(cur)) = (state.sources.get(), state.settings.lock()) {
                        s.set_excluded(settings::exclusion_keys(&cur.excluded_folders));
                    }
                });
            }
            window::setup(&handle);
            let tr = st.language == settings::Lang::Tr;
            let label = |en: &'static str, tr_: &'static str| if tr { tr_ } else { en };
            let open = MenuItem::with_id(
                app,
                "open",
                label("Open Flint", "Flint'i aç"),
                true,
                None::<&str>,
            )?;
            let prefs = MenuItem::with_id(
                app,
                "settings",
                label("Settings…", "Ayarlar…"),
                true,
                None::<&str>,
            )?;
            let quit = MenuItem::with_id(
                app,
                "quit",
                label("Quit Flint", "Flint'ten çık"),
                true,
                None::<&str>,
            )?;
            let menu = Menu::with_items(app, &[&open, &prefs, &quit])?;
            app.manage(window::TrayItems([
                open.clone(),
                prefs.clone(),
                quit.clone(),
            ]));
            let mut tray = TrayIconBuilder::with_id("flint")
                .tooltip("Flint")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, ev| match ev.id().as_ref() {
                    "open" => window::show(app),
                    "settings" => {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move { window::open_settings(&app) });
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, ev| {
                    if let tauri::tray::TrayIconEvent::Click {
                        button: tauri::tray::MouseButton::Left,
                        button_state: tauri::tray::MouseButtonState::Up,
                        ..
                    } = ev
                    {
                        window::show(tray.app_handle());
                    }
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.build(app)?;
            let hk = hotkey::parse(&st.hotkey).unwrap_or_else(|e| {
                log::line(format!(
                    "hotkey {:?} in settings not usable ({e}), Alt+Space used",
                    st.hotkey
                ));
                hotkey::parse(hotkey::DEFAULT).expect("the default hotkey parses")
            });
            match register_hotkey(&handle, &hk) {
                Ok(()) => {
                    if let Ok(mut held) = state.hotkey.lock() {
                        *held = Some(hk);
                    }
                }
                // shown in the launcher until Settings picks another
                Err(e) => window::set_notice(&handle, &e.to_string()),
            }
            {
                let state = state.clone();
                updates::start(handle.clone(), move || {
                    state
                        .settings
                        .lock()
                        .map(|s| s.check_updates)
                        .unwrap_or(false)
                });
            }
            if st.launch_at_startup {
                // keeps the entry pointing here after Flint was moved
                if let Err(e) = std::env::current_exe()
                    .and_then(|exe| autostart::set(autostart::RUN_KEY, true, &exe))
                {
                    log::line(format!("startup entry not refreshed: {e}"));
                }
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            launcher_notice,
            search,
            status,
            open,
            reveal,
            copy_path,
            details,
            get_settings,
            set_settings,
            clear_history,
            open_settings,
            enable_full_indexing,
            update_state,
            check_update,
            install_update,
            gate_hotkey_ms
        ])
        .on_window_event(window::on_event)
        .build(tauri::generate_context!());
    match built {
        Ok(app) => app.run(|app, ev| {
            if let tauri::RunEvent::Exit = ev {
                // the in-process indexers save on stop
                if let Some(s) = app.state::<AppState>().sources.get() {
                    s.shutdown();
                }
            }
        }),
        Err(e) => log::line(format!("flint could not start: {e}")),
    }
}
