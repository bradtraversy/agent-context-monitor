mod alerts;
mod claude;
mod claude_desktop;
mod claude_sessions;
mod codex;
mod codex_live;
mod history;
mod jsonl;
mod model;
mod monitor;
mod settings;
mod tray;

use std::time::SystemTime;

use tauri::{Manager, WindowEvent};

use crate::history::ProviderHistory;
use crate::model::Snapshot;
use crate::monitor::{HistoryFile, SharedSnapshot};
use crate::settings::Settings;

#[tauri::command]
fn get_snapshot(state: tauri::State<SharedSnapshot>) -> Snapshot {
    state.0.lock().unwrap().clone()
}

#[tauri::command]
fn set_context_limit(settings: tauri::State<Settings>, tokens: Option<u64>) {
    settings.set_context_limit(tokens);
}

#[tauri::command]
fn get_history(
    snapshot: tauri::State<SharedSnapshot>,
    file: tauri::State<HistoryFile>,
    utc_offset_minutes: i64,
    days: usize,
    weeks: usize,
) -> Vec<ProviderHistory> {
    let providers: Vec<(String, String)> = snapshot
        .0
        .lock()
        .unwrap()
        .usage
        .iter()
        .map(|u| (u.provider.to_string(), u.name.to_string()))
        .collect();
    history::load(
        &file.0,
        &providers,
        SystemTime::now(),
        utc_offset_minutes * 60,
        days,
        weeks,
    )
}

/// Changes only the window's width, for the button that switches between the compact and wide views.
/// `width` is in page pixels and `page_width` is the page's current width. They can differ from
/// window pixels, e.g. GNOME's large text setting zooms the page, so the ratio converts them.
#[tauri::command]
fn set_window_width(window: tauri::WebviewWindow, width: f64, page_width: f64) -> tauri::Result<()> {
    if window.is_maximized()? {
        window.unmaximize()?;
    }
    let ratio = if page_width > 0.0 { width / page_width } else { 1.0 };
    // Tauri's window size on Linux counts the title bar and shadow but its resize doesn't, so
    // reusing its height grows the window on every click. GTK's own size and resize agree.
    #[cfg(target_os = "linux")]
    {
        use gtk::prelude::GtkWindowExt;
        let win = window.clone();
        window.run_on_main_thread(move || {
            if let Ok(gtk) = win.gtk_window() {
                let (current, height) = gtk.size();
                gtk.resize((current as f64 * ratio).round() as i32, height);
            }
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let size = window.inner_size()?.to_logical::<f64>(window.scale_factor()?);
        window.set_size(tauri::LogicalSize::new(size.width * ratio, size.height))
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Must come first: opening the app again brings up the window that's already running.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            tray::show_window(app);
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .manage(SharedSnapshot::default())
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            get_history,
            set_context_limit,
            set_window_width
        ])
        .setup(|app| {
            // A menu bar app on macOS, so no Dock icon next to the tray dot.
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            let path = app
                .path()
                .app_config_dir()
                .ok()
                .map(|d| d.join("settings.json"));
            app.manage(Settings::load(path));
            tray::create(app.handle())?;
            monitor::start(app.handle().clone());
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window keeps the app in the tray; Quit lives in the tray menu.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running Agent Context Monitor");
}
