use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use tauri::{AppHandle, Emitter, Manager};

use crate::claude::{self, ClaudeSource};
use crate::claude_sessions::ClaudeSessions;
use crate::codex::CodexSource;
use crate::model::{ChatContext, Snapshot};
use crate::tray;

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Default)]
pub struct SharedSnapshot(pub Mutex<Snapshot>);

pub fn start(app: AppHandle) {
    let home = std::env::home_dir().unwrap_or_default();
    let claude_home = env_dir("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home.join(".claude"));
    let codex_home = env_dir("CODEX_HOME").unwrap_or_else(|| home.join(".codex"));

    // The Claude request is slow and infrequent, so it runs apart from the file loop.
    let claude = Arc::new(Mutex::new(ClaudeSource::new(claude_home.clone())));
    {
        let claude = Arc::clone(&claude);
        thread::spawn(move || {
            let mut source = claude.lock().unwrap().clone();
            loop {
                source.refresh(SystemTime::now());
                *claude.lock().unwrap() = source.clone();
                thread::sleep(claude::POLL_INTERVAL);
            }
        });
    }

    thread::spawn(move || {
        let mut codex = CodexSource::new(codex_home);
        let mut claude_sessions = ClaudeSessions::new(claude_home);
        loop {
            let now = SystemTime::now();
            codex.refresh(now);
            let usage = vec![claude.lock().unwrap().usage(now), codex.usage(now)];
            let mut chats = claude_sessions.chats(now);
            chats.extend(codex.chats(now));
            chats.sort_by(|a, b| {
                urgency(b)
                    .total_cmp(&urgency(a))
                    .then(b.last_activity.cmp(&a.last_activity))
            });
            publish(&app, Snapshot::new(usage, chats));
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

fn urgency(chat: &ChatContext) -> f64 {
    chat.context
        .as_ref()
        .and_then(|c| c.used_percent)
        .unwrap_or(-1.0)
}

fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}
