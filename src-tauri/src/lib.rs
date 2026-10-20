mod claude;
mod claude_sessions;
mod codex;
mod codex_live;
mod jsonl;
mod model;
mod monitor;
mod tray;

use tauri::WindowEvent;

use crate::model::Snapshot;
use crate::monitor::SharedSnapshot;

#[tauri::command]
fn get_snapshot(state: tauri::State<SharedSnapshot>) -> Snapshot {
    state.0.lock().unwrap().clone()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(SharedSnapshot::default())
        .invoke_handler(tauri::generate_handler![get_snapshot])
        .setup(|app| {
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
