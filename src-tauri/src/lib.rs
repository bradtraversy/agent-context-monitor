mod claude;
mod claude_sessions;
mod codex;
mod codex_live;
mod history;
mod jsonl;
mod model;
mod monitor;
mod tray;

use std::time::SystemTime;

use tauri::WindowEvent;

use crate::history::ProviderHistory;
use crate::model::Snapshot;
use crate::monitor::{HistoryFile, SharedSnapshot};

#[tauri::command]
fn get_snapshot(state: tauri::State<SharedSnapshot>) -> Snapshot {
    state.0.lock().unwrap().clone()
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
        .manage(SharedSnapshot::default())
        .invoke_handler(tauri::generate_handler![get_snapshot, get_history])
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
