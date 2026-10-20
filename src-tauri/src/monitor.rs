use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use tauri::{AppHandle, Emitter, Manager};

use crate::claude::ClaudeSource;
use crate::claude_sessions::ClaudeSessions;
use crate::codex::CodexSource;
use crate::codex_live;
use crate::model::{LIVE_REFRESH_AFTER_SECS, Snapshot, unix_secs};
use crate::settings::Settings;
use crate::tray;

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const LIVE_CHECK_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct SharedSnapshot(pub Mutex<Snapshot>);

pub fn start(app: AppHandle) {
    let home = std::env::home_dir().unwrap_or_default();
    let claude_home = env_dir("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home.join(".claude"));
    let codex_home = env_dir("CODEX_HOME").unwrap_or_else(|| home.join(".codex"));

    // Live usage checks are slow, so they run apart from the file loop and only when
    // the newest reading is about to go stale.
    let claude = Arc::new(Mutex::new(ClaudeSource::new(claude_home.clone())));
    {
        let claude = Arc::clone(&claude);
        thread::spawn(move || {
            let mut source = claude.lock().unwrap().clone();
            let mut last_check = i64::MIN;
            loop {
                let now = SystemTime::now();
                if unix_secs(now).saturating_sub(last_check) >= LIVE_REFRESH_AFTER_SECS {
                    last_check = unix_secs(now);
                    source.refresh(now);
                    *claude.lock().unwrap() = source.clone();
                }
                thread::sleep(LIVE_CHECK_INTERVAL);
            }
        });
    }

    let codex_live = Arc::new(Mutex::new(None));
    let newest_codex_file = Arc::new(AtomicI64::new(i64::MIN));
    {
        let codex_live = Arc::clone(&codex_live);
        let newest_codex_file = Arc::clone(&newest_codex_file);
        thread::spawn(move || {
            let mut last_check = i64::MIN;
            loop {
                let now = SystemTime::now();
                // Codex writes fresh readings to its session files while in use, so
                // starting Codex to ask is only needed when it has been quiet.
                let newest = last_check.max(newest_codex_file.load(Ordering::Relaxed));
                if unix_secs(now).saturating_sub(newest) >= LIVE_REFRESH_AFTER_SECS {
                    last_check = unix_secs(now);
                    if let Some(reading) = codex_live::fetch(now) {
                        *codex_live.lock().unwrap() = Some(reading);
                    }
                }
                thread::sleep(LIVE_CHECK_INTERVAL);
            }
        });
    }

    thread::spawn(move || {
        let mut codex = CodexSource::new(codex_home);
        let mut claude_sessions = ClaudeSessions::new(claude_home);
        loop {
            let now = SystemTime::now();
            codex.refresh(now);
            if let Some(newest) = codex.newest_file_reading() {
                newest_codex_file.store(newest, Ordering::Relaxed);
            }
            let live = codex_live.lock().unwrap().clone();
            let usage = vec![
                claude.lock().unwrap().usage(now).with_staleness(now),
                codex.usage(now, live.as_ref()).with_staleness(now),
            ];
            let limit = app.state::<Settings>().context_limit();
            let mut chats = claude_sessions.chats(now);
            chats.extend(codex.chats(now));
            for chat in &mut chats {
                chat.context = chat.context.take().map(|c| c.with_limit(limit));
            }
            chats.sort_by_key(|c| std::cmp::Reverse(c.last_activity));
            let mut snapshot = Snapshot::new(usage, chats, now);
            snapshot.context_limit = limit;
            publish(&app, snapshot);
            thread::sleep(REFRESH_INTERVAL);
        }
    });
}

fn publish(app: &AppHandle, snapshot: Snapshot) {
    let state = app.state::<SharedSnapshot>();
    {
        let mut current = state.0.lock().unwrap();
        if *current == snapshot {
            return;
        }
        *current = snapshot.clone();
    }
    tray::update(app, &snapshot);
    let _ = app.emit("snapshot", &snapshot);
}

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}
