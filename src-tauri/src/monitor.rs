use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_notification::NotificationExt;

use crate::alerts::Alerts;

use crate::claude::ClaudeSource;
use crate::claude_sessions::ClaudeSessions;
use crate::codex::CodexSource;
use crate::codex_live;
use crate::copilot::CopilotSource;
use crate::cursor::CursorSource;
use crate::history::Recorder;
use crate::model::{LIVE_REFRESH_AFTER_SECS, ProviderInfo, Snapshot, unix_secs};
use crate::opencode::OpenCodeChats;
use crate::settings::Settings;
use crate::sources;
use crate::tray;

const REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const LIVE_CHECK_INTERVAL: Duration = Duration::from_secs(30);
/// How often the Claude check looks for a reason to ask Claude for fresh usage.
const CLAUDE_TICK: Duration = Duration::from_secs(1);
/// While a Claude chat is being used, usage is checked this often so the numbers keep up.
const CLAUDE_ACTIVE_GAP_SECS: i64 = 20;
/// Opening the window re-checks usage unless it was checked this recently.
const CLAUDE_OPEN_GAP_SECS: i64 = 5;
/// How often to look for a new sign-in after the saved login stopped working.
const LOGIN_PROBE_SECS: i64 = 30;
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

    // Live usage checks are slow, so they run apart from the file loop and only when
    // the newest reading is about to go stale.
    let started = Instant::now();
    let first_checks_done = Arc::new(AtomicUsize::new(0));
    let claude = Arc::new(Mutex::new(ClaudeSource::new(claude_home.clone())));
    let check_now = Arc::clone(&app.state::<CheckNow>().0);
    // Unix seconds of the last write to any Claude chat, so usage follows Claude in use.
    let newest_claude_chat = Arc::new(AtomicI64::new(i64::MIN));
    {
        let claude = Arc::clone(&claude);
        let first_checks_done = Arc::clone(&first_checks_done);
        let newest_claude_chat = Arc::clone(&newest_claude_chat);
        thread::spawn(move || {
            let mut source = claude.lock().unwrap().clone();
            let mut last_check = i64::MIN;
            let mut last_login_probe = i64::MIN;
            let mut first = true;
            let mut seen_open = check_now.load(Ordering::Relaxed);
            loop {
                let now = SystemTime::now();
                let secs = unix_secs(now);
                let open_count = check_now.load(Ordering::Relaxed);
                let reason = CheckReason {
                    opened: std::mem::replace(&mut seen_open, open_count) != open_count,
                    chat_active: newest_claude_chat.load(Ordering::Relaxed) >= last_check,
                };
                let mut due = claude_check_due(secs, last_check, reason, source.backing_off(now));
                if !due && secs.saturating_sub(last_login_probe) >= LOGIN_PROBE_SECS {
                    last_login_probe = secs;
                    due = source.login_changed();
                }
                if due {
                    last_check = secs;
                    source.refresh(now);
                    *claude.lock().unwrap() = source.clone();
                }
                mark_first_check(&mut first, &first_checks_done);
                thread::sleep(CLAUDE_TICK);
            }
        });
    }

    let codex_live = Arc::new(Mutex::new(None));
    let newest_codex_file = Arc::new(AtomicI64::new(i64::MIN));
    {
        let codex_live = Arc::clone(&codex_live);
        let newest_codex_file = Arc::clone(&newest_codex_file);
        let first_checks_done = Arc::clone(&first_checks_done);
        thread::spawn(move || {
            let mut last_check = i64::MIN;
            let mut first = true;
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
                mark_first_check(&mut first, &first_checks_done);
                thread::sleep(LIVE_CHECK_INTERVAL);
            }
        });
    }

    let config_dir = app
        .path()
        .config_dir()
        .unwrap_or_else(|_| home.join(".config"));
    let opened = Arc::clone(&app.state::<CheckNow>().0);
    let live_sources = [
        sources::spawn(
            CopilotSource::new(home.clone()),
            Arc::clone(&opened),
            Arc::clone(&first_checks_done),
        ),
        sources::spawn(
            CursorSource::new(home.clone(), config_dir),
            Arc::clone(&opened),
            Arc::clone(&first_checks_done),
        ),
    ];
    // Claude and Codex have their own checks above.
    let live_source_count = live_sources.len() + 2;

    thread::spawn(move || {
        let mut opencode = OpenCodeChats::new(home);
        let mut alerts = Alerts::load(data_dir.join("alerts-sent.json"));
        let mut codex = CodexSource::new(codex_home);
        let mut claude_sessions = ClaudeSessions::new(claude_home, desktop_sessions);
        let mut history = Recorder::open(history_path, SystemTime::now());
        loop {
            if first_checks_done.load(Ordering::Relaxed) < live_source_count
                && started.elapsed() < FIRST_CHECK_WAIT
            {
                thread::sleep(Duration::from_millis(200));
                continue;
            }
            let now = SystemTime::now();
            codex.refresh(now);
            if let Some(newest) = codex.newest_file_reading() {
                newest_codex_file.store(newest, Ordering::Relaxed);
            }
            let live = codex_live.lock().unwrap().clone();
            let mut usage = vec![
                claude.lock().unwrap().usage(now).with_staleness(now),
                codex.usage(now, live.as_ref()).with_staleness(now),
            ];
            // Readings age between checks, so staleness is worked out again each time.
            usage.extend(
                live_sources
                    .iter()
                    .filter_map(|l| l.lock().unwrap().clone())
                    .map(|u| u.with_staleness(now)),
            );
            history.record(&usage, now);
            let limit = app.state::<Settings>().context_limit();
            let mut chats = claude_sessions.chats(now);
            if let Some(newest) = chats.iter().map(|c| c.last_activity).max() {
                newest_claude_chat.store(newest, Ordering::Relaxed);
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

#[derive(Clone, Copy, Default)]
struct CheckReason {
    /// The window was just opened.
    opened: bool,
    /// A Claude chat was written to since the last check.
    chat_active: bool,
}

/// Whether to ask Claude for fresh usage now. The regular check keeps readings under the
/// freshness limit; opening the window or using Claude checks sooner, unless Claude has
/// asked for fewer checks.
fn claude_check_due(now: i64, last_check: i64, reason: CheckReason, backing_off: bool) -> bool {
    let age = now.saturating_sub(last_check);
    if age >= LIVE_REFRESH_AFTER_SECS {
        return true;
    }
    if backing_off {
        return false;
    }
    (reason.opened && age >= CLAUDE_OPEN_GAP_SECS)
        || (reason.chat_active && age >= CLAUDE_ACTIVE_GAP_SECS)
}

/// Counts a live source's first check, done or failed, so the window can stop waiting on it.
fn mark_first_check(first: &mut bool, done: &AtomicUsize) {
    if std::mem::take(first) {
        done.fetch_add(1, Ordering::Relaxed);
    }
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

    const OPENED: CheckReason = CheckReason {
        opened: true,
        chat_active: false,
    };
    const ACTIVE: CheckReason = CheckReason {
        opened: false,
        chat_active: true,
    };
    const QUIET: CheckReason = CheckReason {
        opened: false,
        chat_active: false,
    };

    #[test]
    fn first_check_runs_right_away() {
        assert!(claude_check_due(1000, i64::MIN, QUIET, false));
    }

    #[test]
    fn quiet_usage_is_checked_before_it_goes_stale() {
        assert!(!claude_check_due(
            1000 + LIVE_REFRESH_AFTER_SECS - 1,
            1000,
            QUIET,
            false
        ));
        assert!(claude_check_due(
            1000 + LIVE_REFRESH_AFTER_SECS,
            1000,
            QUIET,
            false
        ));
    }

    #[test]
    fn opening_the_window_checks_unless_just_checked() {
        assert!(!claude_check_due(1003, 1000, OPENED, false));
        assert!(claude_check_due(
            1000 + CLAUDE_OPEN_GAP_SECS,
            1000,
            OPENED,
            false
        ));
    }

    #[test]
    fn claude_in_use_is_checked_often() {
        assert!(!claude_check_due(1010, 1000, ACTIVE, false));
        assert!(claude_check_due(
            1000 + CLAUDE_ACTIVE_GAP_SECS,
            1000,
            ACTIVE,
            false
        ));
    }

    #[test]
    fn backing_off_keeps_only_the_regular_check() {
        assert!(!claude_check_due(1100, 1000, OPENED, true));
        assert!(!claude_check_due(1100, 1000, ACTIVE, true));
        assert!(claude_check_due(
            1000 + LIVE_REFRESH_AFTER_SECS,
            1000,
            QUIET,
            true
        ));
    }
}
