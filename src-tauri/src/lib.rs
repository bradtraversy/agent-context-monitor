mod claude;
mod claude_sessions;
mod codex;
mod codex_live;
mod jsonl;
mod model;
mod monitor;
mod settings;
mod tray;

use tauri::{Manager, WindowEvent};

use crate::model::Snapshot;
use crate::monitor::SharedSnapshot;
use crate::settings::Settings;

#[tauri::command]
fn get_snapshot(state: tauri::State<SharedSnapshot>) -> Snapshot {
    state.0.lock().unwrap().clone()
}

#[tauri::command]
fn set_context_limit(settings: tauri::State<Settings>, tokens: Option<u64>) {
    settings.set_context_limit(tokens);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(SharedSnapshot::default())
        .invoke_handler(tauri::generate_handler![get_snapshot, set_context_limit])
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
