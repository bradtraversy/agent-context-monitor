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

/// Resizes the window to a width and/or height in page pixels, leaving out either to keep it.
/// Page pixels can differ from window pixels, e.g. GNOME's large text setting zooms the page,
/// so the page's current size (`page_width`, `page_height`) gives the ratio between them.
#[tauri::command]
fn resize_window(
    window: tauri::WebviewWindow,
    width: Option<f64>,
    height: Option<f64>,
    page_width: f64,
    page_height: f64,
) -> tauri::Result<()> {
    if window.is_maximized()? {
        window.unmaximize()?;
    }
    let scale = |target: Option<f64>, page: f64, current: f64| match target {
        Some(t) if page > 0.0 => current * t / page,
        _ => current,
    };
    // Tauri's window size on Linux counts the title bar and shadow but its resize doesn't, so
    // reusing its size grows the window on every click. GTK's own size and resize agree.
    #[cfg(target_os = "linux")]
    {
        use gtk::prelude::GtkWindowExt;
        let win = window.clone();
        window.run_on_main_thread(move || {
            if let Ok(gtk) = win.gtk_window() {
                let (w, h) = gtk.size();
                gtk.resize(
                    scale(width, page_width, w as f64).round() as i32,
                    scale(height, page_height, h as f64).round() as i32,
                );
            }
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let size = window.inner_size()?.to_logical::<f64>(window.scale_factor()?);
        window.set_size(tauri::LogicalSize::new(
            scale(width, page_width, size.width),
            scale(height, page_height, size.height),
        ))
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
            resize_window
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
