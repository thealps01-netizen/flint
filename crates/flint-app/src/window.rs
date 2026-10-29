//! The launcher window: created once at startup, shown and hidden, never
//! destroyed (never cold-started).

use crate::log;
use std::sync::Mutex;
use tauri::window::{Effect, EffectsBuilder};
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, WebviewWindow, WindowEvent};

pub const LABEL: &str = "launcher";

/// When the show/hide hotkey was last pressed, ms since 1970 (gate runs).
pub static LAST_HOTKEY_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A message the launcher shows on its next appearance (a hotkey that
/// could not be registered), until it is cleared.
static NOTICE: Mutex<Option<String>> = Mutex::new(None);

pub fn set_notice(app: &AppHandle, text: &str) {
    if let Ok(mut n) = NOTICE.lock() {
        *n = Some(text.to_string());
    }
    let _ = app.emit_to(LABEL, "flint://notice", text);
}

pub fn notice() -> Option<String> {
    NOTICE.lock().ok().and_then(|n| n.clone())
}

fn launcher(app: &AppHandle) -> Option<WebviewWindow> {
    let w = app.get_webview_window(LABEL);
    if w.is_none() {
        log::line("the launcher window is missing");
    }
    w
}

pub fn setup(app: &AppHandle) {
    let Some(w) = launcher(app) else { return };
    apply_effects(&w, transparency_on());
}

/// The transparency the window's effects were last set for (0 = not yet,
/// 1 = off, 2 = on): the setting can change while Flint runs.
static EFFECTS_FOR: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Acrylic behind the translucent surface (Windows 11; Windows 10 gets
/// the same API). When Windows transparency effects are off, there is no
/// acrylic and the page draws an opaque surface (see `transparency_on`).
/// Only a change touches the window.
fn apply_effects(w: &WebviewWindow, on: bool) {
    use std::sync::atomic::Ordering;
    let want = if on { 2 } else { 1 };
    if EFFECTS_FOR.swap(want, Ordering::Relaxed) == want {
        return;
    }
    let r = if on {
        w.set_effects(EffectsBuilder::new().effect(Effect::Acrylic).build())
    } else {
        w.set_effects(None)
    };
    if let Err(e) = r {
        log::line(format!("window effects not set (transparency {on}): {e}"));
    }
}

/// Windows "Transparency effects" (Settings → Personalisation → Colours).
pub fn transparency_on() -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Registry::{
            HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW,
        };
        let key: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"
            .encode_utf16()
            .chain([0])
            .collect();
        let name: Vec<u16> = "EnableTransparency".encode_utf16().chain([0]).collect();
        let mut v: u32 = 1;
        let mut len = 4u32;
        // SAFETY: key and name are NUL-terminated; v is 4 writable bytes.
        let r = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_DWORD,
                std::ptr::null_mut(),
                (&mut v as *mut u32).cast(),
                &mut len,
            )
        };
        r != 0 || v != 0
    }
    #[cfg(not(windows))]
    true
}

/// Centres the window on the monitor under the mouse, a little above the
/// middle (the sketch's position), so it opens where the user is looking.
fn place(app: &AppHandle, w: &WebviewWindow) {
    let Ok(cursor) = app.cursor_position() else {
        return;
    };
    let mon = app
        .monitor_from_point(cursor.x, cursor.y)
        .ok()
        .flatten()
        .or_else(|| w.current_monitor().ok().flatten());
    let (Some(mon), Ok(size)) = (mon, w.outer_size()) else {
        return;
    };
    let area = mon.work_area();
    let x = area.position.x + (area.size.width as i32 - size.width as i32) / 2;
    let y = area.position.y + ((area.size.height as i32 - size.height as i32) as f64 * 0.32) as i32;
    let _ = w.set_position(PhysicalPosition::new(x, y));
}

pub fn show(app: &AppHandle) {
    let Some(w) = launcher(app) else { return };
    place(app, &w);
    if let Err(e) = w.show() {
        log::line(format!("launcher not shown: {e}"));
        return;
    }
    let _ = w.unminimize();
    let _ = w.set_focus();
    let on = transparency_on();
    apply_effects(&w, on);
    let _ = w.emit("flint://shown", on);
}

pub fn hide(app: &AppHandle) {
    if let Some(w) = launcher(app) {
        let _ = w.hide();
    }
}

pub fn toggle(app: &AppHandle) {
    let Some(w) = launcher(app) else { return };
    let visible = w.is_visible().unwrap_or(false);
    let focused = w.is_focused().unwrap_or(false);
    if visible && focused {
        hide(app);
    } else {
        show(app);
    }
}

/// Close hides (the window lives for the app's lifetime); losing focus
/// hides the launcher like Spotlight and Raycast.
pub fn on_event(w: &tauri::Window, ev: &WindowEvent) {
    if w.label() != LABEL {
        return;
    }
    match ev {
        WindowEvent::CloseRequested { api, .. } => {
            api.prevent_close();
            let _ = w.hide();
        }
        WindowEvent::Focused(false) => {
            let _ = w.hide();
        }
        _ => {}
    }
}

pub fn clear_notice(app: &AppHandle) {
    if let Ok(mut n) = NOTICE.lock() {
        *n = None;
    }
    let _ = app.emit_to(LABEL, "flint://notice", None::<String>);
}

pub const SETTINGS: &str = "settings";

/// The tray menu's items (open, settings, quit), relabelled when the
/// language changes.
pub struct TrayItems(pub [tauri::menu::MenuItem<tauri::Wry>; 3]);

fn window_theme(t: crate::settings::Theme) -> Option<tauri::Theme> {
    match t {
        crate::settings::Theme::System => None,
        crate::settings::Theme::Dark => Some(tauri::Theme::Dark),
        crate::settings::Theme::Light => Some(tauri::Theme::Light),
    }
}

fn settings_title(tr: bool) -> &'static str {
    if tr {
        "Flint Ayarları"
    } else {
        "Flint Settings"
    }
}

/// Tells every page the new settings (theme and language apply at once)
/// and relabels the tray menu.
pub fn apply_settings(app: &AppHandle, s: &crate::settings::Settings) {
    let _ = app.emit("flint://settings", s);
    let tr = s.language == crate::settings::Lang::Tr;
    if let Some(w) = app.get_webview_window(SETTINGS) {
        let _ = w.set_theme(window_theme(s.theme));
        let _ = w.set_title(settings_title(tr));
    }
    if let Some(items) = app.try_state::<TrayItems>() {
        let labels = if tr {
            ["Flint'i aç", "Ayarlar…", "Flint'ten çık"]
        } else {
            ["Open Flint", "Settings…", "Quit Flint"]
        };
        for (item, text) in items.0.iter().zip(labels) {
            let _ = item.set_text(text);
        }
    }
}

/// Shows the Settings window, creating it the first time.
pub fn open_settings(app: &AppHandle) {
    hide(app);
    if let Some(w) = app.get_webview_window(SETTINGS) {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let (theme, tr) = app
        .try_state::<crate::AppState>()
        .and_then(|s| {
            s.settings
                .lock()
                .ok()
                .map(|s| (s.theme, s.language == crate::settings::Lang::Tr))
        })
        .unwrap_or_default();
    let built = tauri::WebviewWindowBuilder::new(
        app,
        SETTINGS,
        tauri::WebviewUrl::App("settings.html".into()),
    )
    .title(settings_title(tr))
    .inner_size(680.0, 720.0)
    .min_inner_size(560.0, 480.0)
    .maximizable(false)
    .theme(window_theme(theme))
    .center()
    .build();
    if let Err(e) = built {
        log::line(format!("settings window not opened: {e}"));
    }
}
