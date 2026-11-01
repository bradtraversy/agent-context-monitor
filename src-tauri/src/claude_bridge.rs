//! Claude plan usage and context windows, from Claude's own apps. ACM never touches
//! Claude's login.
//!
//! Claude Code in a terminal runs a status line command after each reply and passes it
//! the session's usage limits and context window as JSON. ACM sets that command to
//! itself, run with `--claude-statusline`: it saves the few numbers it needs and prints
//! nothing, so the status line stays empty. If a status line was already set up, ACM
//! runs first and hands the same JSON on to it, so that status line keeps working. The
//! old setting is also saved in ACM's data folder as `previous-statusline.json`.
//!
//! The Claude desktop app doesn't run status lines, but it records the same two
//! percentages every 15 minutes for its own usage meter, without reset times. Whichever
//! reading is newer wins.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::time::UNIX_EPOCH;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::files;
use crate::model::{Limit, LimitWindow, ProviderUsage, unix_secs};

const PROVIDER: &str = "claude";
const NAME: &str = "Claude";
/// The argument that tells ACM it was started by Claude Code's status line.
pub const ARG: &str = "--claude-statusline";
/// Also passes the status line JSON on, to the status line that was there before.
const PASS_ARG: &str = "--pass";
/// The status line that was there before, kept so it can be put back by hand.
const PREVIOUS_FILE: &str = "previous-statusline.json";
const WAITING_HINT: &str = "Claude usage appears after you use Claude Code or the Claude app.";
/// The desktop app's usage meter history, in its config folder.
const DESKTOP_FILE: &str = "plan-usage-history.json";
/// Sessions not heard from in this long are forgotten.
const KEEP_SESSIONS: Duration = Duration::from_secs(8 * 24 * 60 * 60);
/// Status line JSON is a few kilobytes; anything far bigger isn't it.
const MAX_INPUT: u64 = 1024 * 1024;

/// The part of Claude Code's status line JSON that ACM keeps.
#[derive(Deserialize)]
struct Input {
    session_id: String,
    context_window: Option<InputContext>,
    rate_limits: Option<RateLimits>,
}

#[derive(Deserialize)]
struct InputContext {
    context_window_size: Option<u64>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
struct RateLimits {
    five_hour: Option<Window>,
    seven_day: Option<Window>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
struct Window {
    used_percentage: f64,
    resets_at: Option<i64>,
}

/// What is saved for each Claude Code session.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
struct Record {
    /// Unix seconds when Claude Code last ran the status line for this session.
    received_at: i64,
    context_window: Option<u64>,
    rate_limits: Option<RateLimits>,
}

/// The desktop app's usage history: one sample every 15 minutes.
#[derive(Deserialize)]
struct DesktopHistory {
    #[serde(default)]
    samples: Vec<DesktopSample>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
struct DesktopSample {
    /// Unix milliseconds.
    t: i64,
    u: DesktopUsage,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
struct DesktopUsage {
    /// Percent of the 5-hour window used; left out while no window is running.
    fh: Option<f64>,
    /// Percent of the weekly window used.
    sd: Option<f64>,
}

/// Run by Claude Code as its status line. Returns false when ACM wasn't started that way.
pub fn run_from_args() -> bool {
    let args: Vec<String> = std::env::args().collect();
    let Some(pos) = args.iter().position(|a| a == ARG) else {
        return false;
    };
    let Some(dir) = args.get(pos + 1) else {
        return true;
    };
    let pass = args.iter().any(|a| a == PASS_ARG);
    let mut input = Vec::new();
    let _ = std::io::stdin().take(MAX_INPUT).read_to_end(&mut input);
    save(Path::new(dir), &input, SystemTime::now());
    if pass {
        let _ = std::io::stdout().write_all(&input);
    }
    true
}

fn sessions_dir(dir: &Path) -> PathBuf {
    dir.join("sessions")
}

/// Saves one status line update. Anything unexpected is skipped quietly, because the
/// status line must never show an error from ACM.
fn save(dir: &Path, input: &[u8], now: SystemTime) {
    let Ok(input) = serde_json::from_slice::<Input>(input) else {
        return;
    };
    // The id becomes a file name, so only plain ids are taken.
    let id = &input.session_id;
    if id.is_empty()
        || id.len() > 100
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return;
    }
    let record = Record {
        received_at: unix_secs(now),
        context_window: input.context_window.and_then(|c| c.context_window_size),
        rate_limits: input.rate_limits,
    };
    let sessions = sessions_dir(dir);
    let _ = fs::create_dir_all(&sessions);
    if let Ok(json) = serde_json::to_vec(&record) {
        let _ = files::replace(&sessions.join(format!("{id}.json")), &json);
    }
}

/// Claude usage and context windows from what the status line saved.
pub struct ClaudeBridge {
    dir: PathBuf,
    desktop_file: PathBuf,
    /// Each session's record, with the file's modified time to skip unchanged files.
    records: HashMap<String, (SystemTime, Record)>,
    /// The desktop app's newest sample, with the file's modified time.
    desktop: Option<(SystemTime, Option<DesktopSample>)>,
}

impl ClaudeBridge {
    /// `dir` is where ACM keeps what the status line saves; `desktop_dir` is the Claude
    /// desktop app's config folder.
    pub fn new(dir: PathBuf, desktop_dir: &Path) -> Self {
        ClaudeBridge {
            dir,
            desktop_file: desktop_dir.join(DESKTOP_FILE),
            records: HashMap::new(),
            desktop: None,
        }
    }

    pub fn refresh(&mut self, now: SystemTime) {
        self.refresh_desktop();
        self.refresh_sessions(now);
    }

    fn refresh_desktop(&mut self) {
        let Ok(modified) = fs::metadata(&self.desktop_file).and_then(|m| m.modified()) else {
            self.desktop = None;
            return;
        };
        if self.desktop.as_ref().is_some_and(|(at, _)| *at == modified) {
            return;
        }
        let newest = fs::read(&self.desktop_file)
            .ok()
            .and_then(|b| serde_json::from_slice::<DesktopHistory>(&b).ok())
            .and_then(|h| h.samples.into_iter().max_by_key(|s| s.t));
        self.desktop = Some((modified, newest));
    }

    fn refresh_sessions(&mut self, now: SystemTime) {
        let secs = unix_secs(now);
        let Ok(entries) = fs::read_dir(sessions_dir(&self.dir)) else {
            self.records.clear();
            return;
        };
        let mut seen = HashMap::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "json") {
                continue;
            }
            let Some(id) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
                continue;
            };
            let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
                continue;
            };
            let record = match self.records.remove(&id) {
                Some((at, record)) if at == modified => record,
                _ => match fs::read(&path)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Record>(&b).ok())
                {
                    Some(record) => record,
                    None => continue,
                },
            };
            if secs.saturating_sub(record.received_at) > KEEP_SESSIONS.as_secs() as i64 {
                let _ = fs::remove_file(&path);
                continue;
            }
            seen.insert(id, (modified, record));
        }
        self.records = seen;
    }

    /// The context window Claude Code reported for a session.
    pub fn context_window(&self, session_id: &str) -> Option<u64> {
        self.records
            .get(session_id)
            .and_then(|(_, r)| r.context_window)
            .filter(|&w| w > 0)
    }

    pub fn usage(&self, now: SystemTime) -> ProviderUsage {
        let status_line = self
            .records
            .values()
            .map(|(_, r)| r)
            .filter_map(|r| Some((r.received_at, r.rate_limits.clone()?)))
            .max_by_key(|(at, _)| *at);
        let desktop = self
            .desktop
            .as_ref()
            .and_then(|(_, s)| s.clone())
            .filter(|s| s.u.fh.is_some() || s.u.sd.is_some());
        let now = unix_secs(now);
        let observed = match (&status_line, &desktop) {
            (None, None) => return ProviderUsage::unknown(PROVIDER, NAME, WAITING_HINT),
            (Some((at, _)), None) => *at,
            (None, Some(d)) => d.t / 1000,
            (Some((at, _)), Some(d)) => (*at).max(d.t / 1000),
        };
        let pick = |from_line: Option<&Window>, from_desktop: Option<f64>| {
            pick_window(
                status_line.as_ref().map(|(at, _)| *at),
                from_line,
                desktop.as_ref().map(|d| d.t / 1000),
                from_desktop,
                now,
            )
        };
        let limits = status_line.as_ref().map(|(_, l)| l);
        // With a weekly reading but no 5-hour one, no 5-hour window is running.
        let desktop_session = desktop
            .as_ref()
            .and_then(|d| d.u.fh.or(d.u.sd.map(|_| 0.0)));
        ProviderUsage::from_reading(
            PROVIDER,
            NAME,
            vec![
                Limit::session(pick(
                    limits.and_then(|l| l.five_hour.as_ref()),
                    desktop_session,
                )),
                Limit::weekly(pick(
                    limits.and_then(|l| l.seven_day.as_ref()),
                    desktop.as_ref().and_then(|d| d.u.sd),
                )),
            ],
            None,
            rfc3339(observed),
            "Claude reported no plan limits.",
        )
    }
}

/// One window from the newer of the two readings. The desktop app's has no reset time,
/// so it borrows the status line's while that one is still ahead.
fn pick_window(
    line_at: Option<i64>,
    line: Option<&Window>,
    desktop_at: Option<i64>,
    desktop: Option<f64>,
    now: i64,
) -> LimitWindow {
    let from_line = |w: &Window| LimitWindow::from_reading(w.used_percentage, w.resets_at, now);
    match (line, desktop) {
        (None, None) => LimitWindow::NotReported,
        (Some(w), None) => from_line(w),
        (None, Some(used)) => LimitWindow::from_reading(used, None, now),
        (Some(w), Some(used)) => {
            if line_at >= desktop_at {
                from_line(w)
            } else {
                let reset = w.resets_at.filter(|&r| r > now);
                LimitWindow::from_reading(used, reset, now)
            }
        }
    }
}

fn rfc3339(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .unwrap_or_default()
        .to_rfc3339()
}

fn read_settings(path: &Path) -> Result<serde_json::Map<String, Value>, String> {
    match fs::read(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
        Err(_) => Err("Couldn't read Claude Code's settings file.".into()),
        Ok(bytes) if bytes.iter().all(u8::is_ascii_whitespace) => Ok(Default::default()),
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(Value::Object(map)) => Ok(map),
            _ => Err("Claude Code's settings file isn't plain JSON, so ACM left it alone.".into()),
        },
    }
}

fn status_command(settings: &serde_json::Map<String, Value>) -> Option<&str> {
    settings.get("statusLine")?.get("command")?.as_str()
}

fn is_ours(command: &str) -> bool {
    command.contains(ARG)
}

/// The status line command that runs ACM, handing the JSON on to `previous` if set.
fn bridge_command(exe: &Path, dir: &Path, previous: Option<&str>) -> String {
    let exe = quote(&exe.to_string_lossy());
    let dir = quote(&dir.to_string_lossy());
    match previous {
        None => format!("{exe} {ARG} {dir}"),
        // If ACM is ever removed, `cat` keeps the old status line working.
        Some(previous) => format!("({exe} {ARG} {dir} {PASS_ARG} 2>/dev/null || cat) | {previous}"),
    }
}

#[cfg(unix)]
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(not(unix))]
fn quote(s: &str) -> String {
    format!("\"{s}\"")
}

/// This copy of ACM's program file. An AppImage runs from a new temporary folder each
/// time, so it's named by its own path.
fn own_exe() -> Result<PathBuf, String> {
    std::env::var_os("APPIMAGE")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok())
        .ok_or_else(|| "Couldn't find ACM's own program file.".into())
}

/// When Claude Code is already connected, points it at this copy of ACM, so moving or
/// reinstalling the app doesn't leave the status line running an old program file.
pub fn keep_connected_here(claude_home: &Path, dir: &Path) {
    let settings_path = claude_home.join("settings.json");
    let Ok(mut settings) = read_settings(&settings_path) else {
        return;
    };
    let Some(current) = status_command(&settings).filter(|c| is_ours(c)) else {
        return;
    };
    let Ok(exe) = own_exe() else { return };
    let previous = fs::read(dir.join(PREVIOUS_FILE))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    let previous_command = previous
        .as_ref()
        .and_then(|p| p.get("command"))
        .and_then(Value::as_str);
    let wanted = bridge_command(&exe, dir, previous_command);
    if current == wanted {
        return;
    }
    if let Some(Value::Object(status_line)) = settings.get_mut("statusLine") {
        status_line.insert("command".into(), wanted.into());
        let _ = write_settings(&settings_path, &settings);
    }
}

/// Has Claude Code share its usage with ACM, once it's installed. Run at startup.
pub fn connect(claude_home: &Path, dir: &Path) {
    let settings = read_settings(&claude_home.join("settings.json"));
    if settings.is_ok_and(|s| status_command(&s).is_some_and(is_ours)) {
        keep_connected_here(claude_home, dir);
    } else if claude_home.is_dir() {
        let _ = turn_on(claude_home, dir);
    }
}

/// Points Claude Code's status line at ACM, keeping any status line already set up.
fn turn_on(claude_home: &Path, dir: &Path) -> Result<(), String> {
    let exe = own_exe()?;
    let settings_path = claude_home.join("settings.json");
    let mut settings = read_settings(&settings_path)?;
    let previous = settings.get("statusLine").cloned();
    if status_command(&settings).is_some_and(is_ours) {
        keep_connected_here(claude_home, dir);
        return Ok(());
    }
    let previous_command = status_command(&settings).map(str::to_string);
    if previous_command.is_some() && cfg!(windows) {
        return Err(
            "You already have a Claude Code status line, which ACM can't share on Windows yet."
                .into(),
        );
    }
    let mut status_line = match &previous {
        // Keeps the old status line's padding and refresh settings too.
        Some(Value::Object(old)) if previous_command.is_some() => old.clone(),
        _ => serde_json::Map::new(),
    };
    status_line.insert("type".into(), "command".into());
    status_line.insert(
        "command".into(),
        bridge_command(&exe, dir, previous_command.as_deref()).into(),
    );
    fs::create_dir_all(dir).map_err(|_| "Couldn't create ACM's data folder.")?;
    let saved = serde_json::to_vec_pretty(&previous.unwrap_or(Value::Null)).unwrap_or_default();
    files::replace(&dir.join(PREVIOUS_FILE), &saved)
        .map_err(|_| "Couldn't save your current status line.")?;
    settings.insert("statusLine".into(), Value::Object(status_line));
    write_settings(&settings_path, &settings)
}

fn write_settings(path: &Path, settings: &serde_json::Map<String, Value>) -> Result<(), String> {
    let mut json = serde_json::to_vec_pretty(settings).unwrap_or_default();
    json.push(b'\n');
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    files::replace(path, &json).map_err(|_| "Couldn't save Claude Code's settings.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::UsageStatus;

    fn epoch(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("acm-bridge-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    const INPUT: &str = r#"{"session_id":"abc-123","transcript_path":"/x.jsonl","model":{"id":"claude-opus-5-5"},
        "context_window":{"context_window_size":1000000,"used_percentage":8},
        "rate_limits":{"five_hour":{"used_percentage":23.5,"resets_at":1900003600},
                       "seven_day":{"used_percentage":41.2,"resets_at":1900500000}}}"#;

    #[test]
    fn saves_usage_and_the_context_window_from_the_status_line() {
        let dir = temp("save");
        let home = temp("save-home");
        let now = epoch(1_900_000_000);
        save(&dir, INPUT.as_bytes(), now);
        let mut bridge = ClaudeBridge::new(dir.clone(), &home);
        bridge.refresh(now);
        assert_eq!(bridge.context_window("abc-123"), Some(1_000_000));
        let usage = bridge.usage(now);
        assert_eq!(usage.status, UsageStatus::Available);
        assert_eq!(
            usage.limit("weekly"),
            Some(&LimitWindow::Reported {
                used_percent: 41.2,
                resets_at: Some(1_900_500_000)
            })
        );
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn odd_session_ids_and_bad_input_are_skipped() {
        let dir = temp("odd");
        let now = epoch(1_900_000_000);
        save(&dir, br#"{"session_id":"../../etc/x"}"#, now);
        save(&dir, b"not json", now);
        assert_eq!(
            fs::read_dir(sessions_dir(&dir))
                .map(|d| d.count())
                .unwrap_or(0),
            0
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn connects_claude_code_once_it_is_installed() {
        let dir = temp("connect");
        let home = temp("connect-home");
        let missing = home.join("not-installed");
        connect(&missing, &dir);
        assert!(!missing.exists());
        connect(&home, &dir);
        let settings = read_settings(&home.join("settings.json")).unwrap();
        assert!(status_command(&settings).is_some_and(is_ours));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&home);
    }

    fn desktop_history(dir: &Path, samples: &str) {
        fs::write(
            dir.join(DESKTOP_FILE),
            format!(r#"{{"version":2,"samples":[{samples}]}}"#),
        )
        .unwrap();
    }

    #[test]
    fn reads_the_desktop_apps_usage_without_any_setup() {
        let dir = temp("desktop");
        let desktop = temp("desktop-app");
        desktop_history(
            &desktop,
            r#"{"t":1899999000000,"org":"o","u":{"fh":50,"sd":20}},
               {"t":1899999900000,"org":"o","u":{"sd":21}}"#,
        );
        let now = epoch(1_900_000_000);
        let mut bridge = ClaudeBridge::new(dir.clone(), &desktop);
        bridge.refresh(now);
        let usage = bridge.usage(now);
        assert_eq!(usage.status, UsageStatus::Available);
        assert_eq!(
            usage.limit("weekly"),
            Some(&LimitWindow::Reported {
                used_percent: 21.0,
                resets_at: None
            })
        );
        // No 5-hour figure next to a weekly one means no 5-hour window is running.
        assert_eq!(
            usage.limit("session"),
            Some(&LimitWindow::Reported {
                used_percent: 0.0,
                resets_at: None
            })
        );
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&desktop);
    }

    #[test]
    fn the_newer_reading_wins_and_keeps_a_known_reset_time() {
        let dir = temp("newer");
        let desktop = temp("newer-app");
        // Status line at 1_899_999_000, desktop app 500 s later.
        save(&dir, INPUT.as_bytes(), epoch(1_899_999_000));
        desktop_history(&desktop, r#"{"t":1899999500000,"u":{"fh":30,"sd":45}}"#);
        let now = epoch(1_900_000_000);
        let mut bridge = ClaudeBridge::new(dir.clone(), &desktop);
        bridge.refresh(now);
        let usage = bridge.usage(now);
        assert_eq!(
            usage.limit("weekly"),
            Some(&LimitWindow::Reported {
                used_percent: 45.0,
                resets_at: Some(1_900_500_000)
            })
        );
        // Then a terminal reply comes in, newer than the desktop sample.
        save(&dir, INPUT.as_bytes(), now);
        bridge.refresh(now);
        assert_eq!(
            bridge.usage(now).limit("weekly"),
            Some(&LimitWindow::Reported {
                used_percent: 41.2,
                resets_at: Some(1_900_500_000)
            })
        );
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&desktop);
    }

    #[test]
    fn turning_on_keeps_other_settings_and_the_old_status_line() {
        let dir = temp("settings");
        let home = temp("settings-home");
        let path = home.join("settings.json");
        fs::write(
            &path,
            r#"{"theme":"dark","statusLine":{"type":"command","command":"~/bin/mine.sh","padding":2},"model":"opus"}"#,
        )
        .unwrap();
        let result = turn_on(&home, &dir);
        if cfg!(windows) {
            assert!(result.is_err());
            return;
        }
        result.unwrap();
        let settings = read_settings(&path).unwrap();
        let keys: Vec<&str> = settings.keys().map(String::as_str).collect();
        assert_eq!(keys, ["theme", "statusLine", "model"]);
        let command = status_command(&settings).unwrap();
        assert!(is_ours(command));
        assert!(command.ends_with("| ~/bin/mine.sh"));
        assert_eq!(settings["statusLine"]["padding"], 2);
        // Turning on twice doesn't wrap it again.
        turn_on(&home, &dir).unwrap();
        assert_eq!(
            status_command(&read_settings(&path).unwrap()),
            Some(command)
        );
        let saved: Value =
            serde_json::from_slice(&fs::read(dir.join(PREVIOUS_FILE)).unwrap()).unwrap();
        assert_eq!(saved["command"], "~/bin/mine.sh");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn a_moved_app_points_claude_code_at_its_new_place() {
        let dir = temp("moved");
        let home = temp("moved-home");
        let path = home.join("settings.json");
        let old = bridge_command(Path::new("/old/place/acm"), &dir, None);
        fs::write(
            &path,
            serde_json::json!({"statusLine": {"type": "command", "command": old}}).to_string(),
        )
        .unwrap();
        keep_connected_here(&home, &dir);
        let settings = read_settings(&path).unwrap();
        let command = status_command(&settings).unwrap();
        assert!(is_ours(command));
        assert!(!command.contains("/old/place"));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn a_settings_file_that_isnt_json_is_left_alone() {
        let dir = temp("bad");
        let home = temp("bad-home");
        let path = home.join("settings.json");
        fs::write(&path, "{ // a comment\n}").unwrap();
        assert!(turn_on(&home, &dir).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{ // a comment\n}");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&home);
    }

    #[cfg(unix)]
    #[test]
    fn the_command_hands_the_json_on_and_survives_acm_being_removed() {
        let dir = temp("chain");
        let command = bridge_command(Path::new("/nonexistent/acm"), &dir, Some("wc -c"));
        let out = std::process::Command::new("sh")
            .args(["-c", &format!("printf hello | {command}")])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "5");
        let _ = fs::remove_dir_all(&dir);
    }
}
