//! Asks the installed Codex for live plan usage, the same way its /status screen does.
//!
//! Session files only update when Codex runs, so they miss resets (including early
//! ones). This runs `codex app-server` and makes one read-only `account/rateLimits/read`
//! request. Codex uses its own login; ACM never reads Codex's token.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use crate::codex::{RateReading, RateWindow};
use crate::programs;

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
const REQUEST_ID: u64 = 1;

#[derive(Deserialize)]
struct Response {
    id: Option<u64>,
    result: Option<ReadResult>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReadResult {
    rate_limits: Option<Snapshot>,
    #[serde(default, deserialize_with = "crate::jsonl::null_as_default")]
    rate_limits_by_limit_id: std::collections::HashMap<String, Snapshot>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    limit_id: Option<String>,
    primary: Option<Window>,
    secondary: Option<Window>,
    plan_type: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Window {
    used_percent: f64,
    window_duration_mins: Option<u64>,
    resets_at: Option<i64>,
}

/// A fresh reading, or why there isn't one, said for the usage card.
pub fn fetch(now: SystemTime) -> Result<RateReading, &'static str> {
    let bin = find_codex().ok_or("Couldn't find Codex to ask it for fresh usage.")?;
    let mut command = Command::new(&bin);
    command
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // An npm install of Codex is a Node script, and an app opened from the Dock or
    // Start menu may not have Node on its PATH. Node usually sits next to it.
    if let Some(path) = bin.parent().and_then(path_with) {
        command.env("PATH", path);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Without this, Windows opens a console window for Codex on every check.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    const NOT_STARTED: &str = "Couldn't start Codex to ask it for fresh usage.";
    let mut child = command.spawn().map_err(|_| NOT_STARTED)?;
    let stdout = child.stdout.take().ok_or(NOT_STARTED)?;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(resp) = serde_json::from_str::<Response>(&line)
                && resp.id == Some(REQUEST_ID)
            {
                let _ = tx.send(resp.result);
                return;
            }
        }
    });
    if let Some(mut stdin) = child.stdin.take() {
        let requests = format!(
            "{}\n{}\n{}\n",
            format_args!(
                r#"{{"method":"initialize","id":0,"params":{{"clientInfo":{{"name":"agent-context-monitor","title":"Agent Context Monitor","version":"{}"}}}}}}"#,
                env!("CARGO_PKG_VERSION")
            ),
            r#"{"method":"initialized"}"#,
            format_args!(r#"{{"method":"account/rateLimits/read","id":{REQUEST_ID}}}"#),
        );
        let _ = stdin.write_all(requests.as_bytes());
        // Keep stdin open until the reply arrives; closing it can end the server early.
        let result = rx.recv_timeout(RESPONSE_TIMEOUT).ok().flatten();
        drop(stdin);
        let _ = child.kill();
        let _ = child.wait();
        return result
            .ok_or("Codex didn't answer when asked for fresh usage.")
            .and_then(|r| reading_from(r, now).ok_or("Codex's usage check had no Codex limits."));
    }
    let _ = child.kill();
    let _ = child.wait();
    Err(NOT_STARTED)
}

fn reading_from(mut result: ReadResult, now: SystemTime) -> Option<RateReading> {
    // The plain field holds whichever limit Codex picked, so only take it as Codex's
    // own when it says so or doesn't say.
    let snapshot = result.rate_limits_by_limit_id.remove("codex").or(result
        .rate_limits
        .filter(|s| s.limit_id.as_deref().is_none_or(|id| id == "codex")))?;
    let windows = [snapshot.primary, snapshot.secondary]
        .into_iter()
        .flatten()
        .map(|w| RateWindow {
            used_percent: w.used_percent,
            window_minutes: w.window_duration_mins,
            resets_at: w.resets_at,
        })
        .collect();
    Some(RateReading {
        observed_at: chrono::DateTime::<chrono::Utc>::from(now)
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        plan: snapshot.plan_type,
        windows,
    })
}

/// Codex ships inside the desktop app as well as on PATH.
fn find_codex() -> Option<PathBuf> {
    let names: &[&str] = if cfg!(windows) {
        &["codex.exe", "codex.cmd"]
    } else {
        &["codex"]
    };
    programs::find(names).or_else(|| BUNDLED.iter().map(PathBuf::from).find(|p| p.is_file()))
}

const BUNDLED: &[&str] = &[
    "/opt/codex-desktop/resources/codex",
    "/usr/lib/chatgpt/resources/codex",
    "/Applications/Codex.app/Contents/Resources/codex",
    "/Applications/ChatGPT.app/Contents/Resources/codex",
];

/// The current PATH with `dir` in front, so Codex's own folder is searched first.
fn path_with(dir: &Path) -> Option<std::ffi::OsString> {
    let current = std::env::var_os("PATH").unwrap_or_default();
    let dirs = std::iter::once(dir.to_path_buf()).chain(std::env::split_paths(&current));
    std::env::join_paths(dirs).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_codex_bucket_from_a_live_response() {
        let line = r#"{"id":1,"result":{"rateLimits":{"limitId":"other","primary":{"usedPercent":50,"windowDurationMins":300,"resetsAt":1}},
            "rateLimitsByLimitId":{"codex":{"limitId":"codex","primary":{"usedPercent":0,"windowDurationMins":10080,"resetsAt":1793084668},"secondary":null,"planType":"pro"}}}}"#;
        let resp: Response = serde_json::from_str(line).unwrap();
        let reading = reading_from(resp.result.unwrap(), SystemTime::UNIX_EPOCH).unwrap();
        assert_eq!(reading.plan.as_deref(), Some("pro"));
        assert_eq!(
            reading.windows,
            vec![RateWindow {
                used_percent: 0.0,
                window_minutes: Some(10080),
                resets_at: Some(1793084668)
            }]
        );
        assert_eq!(reading.observed_at, "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn another_limit_is_never_taken_for_codex() {
        let line = r#"{"id":1,"result":{"rateLimits":{"limitId":"other","primary":{"usedPercent":50,"windowDurationMins":10080,"resetsAt":1}},"rateLimitsByLimitId":null}}"#;
        let resp: Response = serde_json::from_str(line).unwrap();
        assert!(reading_from(resp.result.unwrap(), SystemTime::UNIX_EPOCH).is_none());
    }
}
