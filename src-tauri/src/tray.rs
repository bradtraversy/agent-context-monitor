use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager};

use tauri_plugin_autostart::ManagerExt;

use crate::model::{Level, Snapshot};

const TRAY_ID: &str = "acm";
const ICON_SIZE: u32 = 64;
/// Passed by Start at login so the app starts quietly in the tray.
pub const HIDDEN_ARG: &str = "--hidden";
/// Clicking the tray takes focus from the window just before the click arrives, so a
/// window that lost focus this recently still counts as the one in front.
const CLICK_BLUR: Duration = Duration::from_millis(500);

static LAST_BLUR: Mutex<Option<Instant>> = Mutex::new(None);

pub fn create(app: &AppHandle) -> tauri::Result<()> {
    // Linux tray hosts only show the menu, so opening the window must be a menu item.
    let open = MenuItem::with_id(
        app,
        "open",
        "Open Agent Context Monitor",
        true,
        None::<&str>,
    )?;
    let at_login = app.autolaunch().is_enabled().unwrap_or(false);
    let login =
        CheckMenuItem::with_id(app, "login", "Start at login", true, at_login, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &login, &quit])?;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon(Level::Unknown))
        .tooltip("Agent Context Monitor")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| match event.id().as_ref() {
            "open" => show_window(app),
            "login" => {
                let autolaunch = app.autolaunch();
                let _ = if autolaunch.is_enabled().unwrap_or(false) {
                    autolaunch.disable()
                } else {
                    autolaunch.enable()
                };
                let _ = login.set_checked(autolaunch.is_enabled().unwrap_or(false));
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                toggle_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

pub fn update(app: &AppHandle, snapshot: &Snapshot) {
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        let _ = tray.set_icon(Some(icon(snapshot.level)));
        let _ = tray.set_tooltip(Some(snapshot.tooltip()));
    }
}

pub fn show_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// Called when the window loses focus.
pub fn window_blurred() {
    *LAST_BLUR.lock().unwrap() = Some(Instant::now());
}

/// Start at login entries made before the hidden flag existed open the full window, so
/// rewrite an existing one with the current arguments.
pub fn refresh_autostart(app: &AppHandle) {
    let autolaunch = app.autolaunch();
    if autolaunch.is_enabled().unwrap_or(false) {
        let _ = autolaunch.enable();
    }
}

fn toggle_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let just_blurred = LAST_BLUR
            .lock()
            .unwrap()
            .is_some_and(|t| t.elapsed() < CLICK_BLUR);
        let in_front = window.is_focused().unwrap_or(false) || just_blurred;
        if window.is_visible().unwrap_or(false) && in_front {
            let _ = window.hide();
        } else {
            show_window(app);
        }
    }
}

/// A filled dot in the level's color, drawn at runtime so no icon assets are needed.
fn icon(level: Level) -> Image<'static> {
    let [r, g, b] = match level {
        Level::Unknown => [0x8b, 0x94, 0x9e],
        Level::Ok => [0x3f, 0xb9, 0x50],
        Level::Warning => [0xe3, 0xa0, 0x08],
        Level::Critical => [0xf8, 0x51, 0x49],
    };
    let size = ICON_SIZE as f32;
    let center = size / 2.0;
    let radius = size * 0.4;
    let mut rgba = Vec::with_capacity((ICON_SIZE * ICON_SIZE * 4) as usize);
    for y in 0..ICON_SIZE {
        for x in 0..ICON_SIZE {
            let dx = x as f32 + 0.5 - center;
            let dy = y as f32 + 0.5 - center;
            let coverage = (radius - (dx * dx + dy * dy).sqrt() + 0.5).clamp(0.0, 1.0);
            rgba.extend_from_slice(&[r, g, b, (coverage * 255.0) as u8]);
        }
    }
    Image::new_owned(rgba, ICON_SIZE, ICON_SIZE)
}
