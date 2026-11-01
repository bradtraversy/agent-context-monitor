//! Claude plan usage and context windows, handed over by Claude Code itself.
//!
//! Claude Code runs a status line command after each reply and passes it the session's
//! usage limits and context window as JSON. With Claude usage turned on, that command is
//! ACM, run with `--claude-statusline`: it saves the few numbers it needs and prints
//! nothing, so the status line stays empty. ACM never touches Claude's login.
//!
//! If a status line was already set up, ACM runs first and hands the same JSON on to it,
//! so that status line keeps working. The old setting is also saved in ACM's data folder
//! as `previous-statusline.json`.

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
use crate::model::{Limit, LimitWindow, ProviderUsage, UsageAction, unix_secs};

const PROVIDER: &str = "claude";
const NAME: &str = "Claude";
/// The argument that tells ACM it was started by Claude Code's status line.
pub const ARG: &str = "--claude-statusline";
/// Also passes the status line JSON on, to the status line that was there before.
const PASS_ARG: &str = "--pass";
/// The status line that was there before, kept so it can be put back by hand.
const PREVIOUS_FILE: &str = "previous-statusline.json";
const TURN_ON_HINT: &str =
    "Connect to let Claude Code share your usage with ACM. Your status line stays the same.";
const WAITING_HINT: &str = "Claude usage appears after your next reply in Claude Code.";
/// Sessions not heard from in this long are forgotten.
const KEEP_SESSIONS: Duration = Duration::from_secs(8 * 24 * 60 * 60);
/// Status line JSON is a few kilobytes; anything far bigger isn't it.
const MAX_INPUT: u64 = 1024 * 1024;
/// How often to look at Claude Code's settings to see if the status line is still ACM's.
const SETTINGS_CHECK_SECS: i64 = 10;

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
    settings_path: PathBuf,
    /// Each session's record, with the file's modified time to skip unchanged files.
    records: HashMap<String, (SystemTime, Record)>,
    turned_on: bool,
    last_settings_check: i64,
}

impl ClaudeBridge {
    /// `dir` is where ACM keeps what the status line saves; `claude_home` is Claude
    /// Code's config folder.
    pub fn new(dir: PathBuf, claude_home: &Path) -> Self {
        ClaudeBridge {
            dir,
            settings_path: claude_home.join("settings.json"),
            records: HashMap::new(),
            turned_on: false,
            last_settings_check: i64::MIN,
        }
    }

    pub fn refresh(&mut self, now: SystemTime) {
        let secs = unix_secs(now);
        if secs.saturating_sub(self.last_settings_check) >= SETTINGS_CHECK_SECS {
            self.last_settings_check = secs;
            self.turned_on = is_turned_on(&self.settings_path);
        }
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
        let newest = self
            .records
            .values()
            .map(|(_, r)| r)
            .filter(|r| r.rate_limits.is_some())
            .max_by_key(|r| r.received_at);
        let Some(record) = newest else {
            let mut usage = ProviderUsage::unknown(
                PROVIDER,
                NAME,
                if self.turned_on {
                    WAITING_HINT
                } else {
                    TURN_ON_HINT
                },
            );
            usage.action = (!self.turned_on).then_some(UsageAction::TurnOn);
            return usage;
        };
        let now = unix_secs(now);
        let limits = record.rate_limits.clone().unwrap_or_default();
        let to_window = |w: Option<Window>| {
            w.map_or(LimitWindow::NotReported, |w| {
                LimitWindow::from_reading(w.used_percentage, w.resets_at, now)
            })
        };
        let mut usage = ProviderUsage::from_reading(
            PROVIDER,
            NAME,
            vec![
                Limit::session(to_window(limits.five_hour)),
                Limit::weekly(to_window(limits.seven_day)),
            ],
            None,
            rfc3339(record.received_at),
            "Claude reported no plan limits.",
        );
        if !self.turned_on {
            usage.note =
                Some("Claude Code isn't connected anymore, so this is the last reading.".into());
            usage.action = Some(UsageAction::TurnOn);
        }
        usage
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

fn is_turned_on(settings_path: &Path) -> bool {
    read_settings(settings_path).is_ok_and(|s| status_command(&s).is_some_and(is_ours))
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

/// Points Claude Code's status line at ACM, keeping any status line already set up.
pub fn turn_on(claude_home: &Path, dir: &Path) -> Result<(), String> {
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
    fn asks_to_turn_on_until_it_is() {
        let dir = temp("ask");
        let home = temp("ask-home");
        let mut bridge = ClaudeBridge::new(dir.clone(), &home);
        bridge.refresh(SystemTime::now());
        let usage = bridge.usage(SystemTime::now());
        assert_eq!(usage.action, Some(UsageAction::TurnOn));
        turn_on(&home, &dir).unwrap();
        bridge.last_settings_check = i64::MIN;
        bridge.refresh(SystemTime::now());
        let usage = bridge.usage(SystemTime::now());
        assert_eq!(usage.action, None);
        assert_eq!(usage.note.as_deref(), Some(WAITING_HINT));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&home);
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
