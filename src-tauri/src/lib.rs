mod alerts;
mod claude;
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .manage(SharedSnapshot::default())
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            get_history,
            set_context_limit
        ])
        .setup(|app| {
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
