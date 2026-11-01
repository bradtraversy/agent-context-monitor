use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt;

use crate::alerts::Alerts;

use crate::claude_bridge::{self, ClaudeBridge};
use crate::claude_sessions::ClaudeSessions;
use crate::codex::CodexSource;
use crate::codex_live::CodexLive;
use crate::copilot::CopilotSource;
use crate::cursor::CursorSource;
use crate::history::Recorder;
use crate::model::{ProviderInfo, ProviderUsage, Snapshot, UsageStatus};
use crate::opencode::OpenCodeChats;
use crate::settings::Settings;
use crate::sources::{self, FirstChecks, Latest, UsageSource};
use crate::tray;

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
/// How long the window keeps its loading spinner waiting for the first live checks, so it
/// opens on fresh numbers instead of old ones from session files.
const FIRST_CHECK_WAIT: Duration = Duration::from_secs(15);

#[derive(Default)]
pub struct SharedSnapshot(pub Mutex<Snapshot>);

/// Counts window openings, so each live check notices one and re-checks right away.
#[derive(Default)]
pub struct CheckNow(pub Arc<AtomicU64>);

/// Where usage readings are kept so the History tab can chart them.
pub struct HistoryFile(pub PathBuf);

/// Claude Code's config folder, and where its status line hands usage to ACM.
pub struct ClaudePaths {
    pub claude_home: PathBuf,
    pub bridge_dir: PathBuf,
}

pub fn start(app: AppHandle) {
    let home = std::env::home_dir().unwrap_or_default();
    let claude_home = env_dir("CLAUDE_CONFIG_DIR").unwrap_or_else(|| home.join(".claude"));
    // The Claude desktop app's record of its Code chats, in its own config folder.
    let desktop_sessions = app
        .path()
        .config_dir()
        .unwrap_or_else(|_| home.join(".config"))
        .join("Claude")
        .join("claude-code-sessions");
    let codex_home = env_dir("CODEX_HOME").unwrap_or_else(|| home.join(".codex"));
    let data_dir = app
        .path()
        .app_data_dir()
        .unwrap_or_else(|_| home.join(".agent-context-monitor"));
    let history_path = data_dir.join("usage-history.jsonl");
    app.manage(HistoryFile(history_path.clone()));

    let bridge_dir = data_dir.join("claude-statusline");
    app.manage(ClaudePaths {
        claude_home: claude_home.clone(),
        bridge_dir: bridge_dir.clone(),
    });

    // Live usage checks are slow, so they run apart from the file loop and only when
    // the newest reading is about to go stale.
    let started = Instant::now();
    let first_checks = Arc::new(FirstChecks::default());
    let opened = Arc::clone(&app.state::<CheckNow>().0);
    let newest_codex_file = Arc::new(AtomicI64::new(i64::MIN));
    let codex_live = sources::spawn(
        CodexLive {
            newest_file_reading: Arc::clone(&newest_codex_file),
        },
        Arc::clone(&opened),
        Arc::clone(&first_checks),
    );

    let config_dir = app
        .path()
        .config_dir()
        .unwrap_or_else(|_| home.join(".config"));
    let live_sources = [
        spawn_usage(CopilotSource::new(home.clone()), &opened, &first_checks),
        spawn_usage(
            CursorSource::new(home.clone(), config_dir),
            &opened,
            &first_checks,
        ),
    ];

    thread::spawn(move || {
        let mut opencode = OpenCodeChats::new(home);
        let mut alerts = Alerts::load(data_dir.join("alerts-sent.json"));
        let mut codex = CodexSource::new(codex_home);
        claude_bridge::keep_connected_here(&claude_home, &bridge_dir);
        let mut claude = ClaudeBridge::new(bridge_dir, &claude_home);
        let mut claude_sessions = ClaudeSessions::new(claude_home, desktop_sessions);
        let mut history = Recorder::open(history_path, SystemTime::now());
        loop {
            if !first_checks.all_done() && started.elapsed() < FIRST_CHECK_WAIT {
                thread::sleep(Duration::from_millis(200));
                continue;
            }
            let now = SystemTime::now();
            claude.refresh(now);
            codex.refresh(now);
            if let Some(newest) = codex.newest_file_reading() {
                newest_codex_file.store(newest, Ordering::Relaxed);
            }
            let live = codex_live.lock().unwrap().clone().unwrap_or_default();
            let mut usage = vec![
                // Claude's numbers only change while Claude Code is in use, and it hands
                // them over after every reply, so the last reading is still the current one.
                claude.usage(now),
                with_problem(
                    codex
                        .usage(now, live.last_good.as_ref())
                        .with_staleness(now),
                    live.problem.as_deref(),
                ),
            ];
            // Readings age between checks, so staleness is worked out again each time.
            usage.extend(live_sources.iter().filter_map(|(info, latest)| {
                let readings = latest.lock().unwrap().clone()?;
                Some(readings.usage(info).with_staleness(now))
            }));
            history.record(&usage, now);
            let limit = app.state::<Settings>().context_limit();
            let mut chats = claude_sessions.chats(now);
            for chat in &mut chats {
                if let Some(window) = claude.context_window(&chat.id) {
                    chat.context = chat.context.take().map(|c| c.with_window(window));
                }
            }
            chats.extend(codex.chats(now));
            chats.extend(opencode.chats(now));
            for chat in &mut chats {
                chat.context = chat.context.take().map(|c| c.with_limit(limit));
            }
            chats.sort_by_key(|c| std::cmp::Reverse(c.last_activity));
            let mut providers: Vec<ProviderInfo> = usage
                .iter()
                .map(|u| ProviderInfo {
                    id: u.provider,
                    name: u.name,
                    // Claude and Codex chats are read above; the others only report usage.
                    chats: matches!(u.provider, "claude" | "codex"),
                })
                .collect();
            if opencode.installed() {
                providers.push(OpenCodeChats::INFO);
            }
            let mut snapshot = Snapshot::new(usage, chats, now);
            snapshot.providers = providers;
            snapshot.context_limit = limit;
            for alert in alerts.check(&snapshot, now) {
                let _ = app
                    .notification()
                    .builder()
                    .title(alert.title)
                    .body(alert.body)
                    .show();
            }
            publish(&app, snapshot);
            thread::sleep(REFRESH_INTERVAL);
        }
    });
}

/// Starts the live check of an agent that only reports usage, kept with its name.
fn spawn_usage<S: UsageSource<Reading = ProviderUsage>>(
    source: S,
    opened: &Arc<AtomicU64>,
    first_checks: &Arc<FirstChecks>,
) -> (ProviderInfo, Latest<ProviderUsage>) {
    let info = source.info();
    let latest = sources::spawn(source, Arc::clone(opened), Arc::clone(first_checks));
    (info, latest)
}

/// Says why a live check failed on a card whose reading is out of date or missing.
fn with_problem(mut usage: ProviderUsage, problem: Option<&str>) -> ProviderUsage {
    if let Some(problem) = problem
        && (usage.stale || usage.status == UsageStatus::Unknown)
    {
        usage.note = Some(problem.to_string());
    }
    usage
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_live_check_explains_only_an_old_or_missing_reading() {
        let problem = Some("Codex didn't answer.");
        let mut fresh = ProviderUsage::unknown("codex", "Codex", "");
        fresh.status = UsageStatus::Available;
        fresh.note = None;
        assert_eq!(with_problem(fresh.clone(), problem).note, None);
        let mut old = fresh;
        old.stale = true;
        assert_eq!(with_problem(old, problem).note.as_deref(), problem);
        let missing = ProviderUsage::unknown("codex", "Codex", "No Codex usage.");
        assert_eq!(with_problem(missing, problem).note.as_deref(), problem);
    }
}
