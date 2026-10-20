//! Asks the installed Codex for live plan usage, the same way its /status screen does.
//!
//! Session files only update when Codex runs, so they miss resets (including early
//! ones). This runs `codex app-server` and makes one read-only `account/rateLimits/read`
//! request. Codex uses its own login; ACM never reads Codex's token.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use crate::codex::{RateReading, RateWindow};

pub const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);
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
    #[serde(default)]
    rate_limits_by_limit_id: std::collections::HashMap<String, Snapshot>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
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

pub fn fetch(now: SystemTime) -> Option<RateReading> {
    let bin = find_codex()?;
    let mut child = Command::new(&bin)
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
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
            r#"{"method":"initialize","id":0,"params":{"clientInfo":{"name":"agent-context-monitor","title":"Agent Context Monitor","version":"0.1.0"}}}"#,
            r#"{"method":"initialized"}"#,
            format_args!(r#"{{"method":"account/rateLimits/read","id":{REQUEST_ID}}}"#),
        );
        let _ = stdin.write_all(requests.as_bytes());
        // Keep stdin open until the reply arrives; closing it can end the server early.
        let result = rx.recv_timeout(RESPONSE_TIMEOUT).ok().flatten();
        drop(stdin);
        let _ = child.kill();
        let _ = child.wait();
        return result.and_then(|r| reading_from(r, now));
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

fn reading_from(mut result: ReadResult, now: SystemTime) -> Option<RateReading> {
    let snapshot = result
        .rate_limits_by_limit_id
        .remove("codex")
        .or(result.rate_limits)?;
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
    let exe = if cfg!(windows) { "codex.exe" } else { "codex" };
    let on_path = std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|dir| dir.join(exe));
    let bundled = [
        "/opt/codex-desktop/resources/codex",
        "/usr/lib/chatgpt/resources/codex",
        "/Applications/Codex.app/Contents/Resources/codex",
        "/Applications/ChatGPT.app/Contents/Resources/codex",
    ]
    .into_iter()
    .map(PathBuf::from);
    on_path.chain(bundled).find(|p| p.is_file())
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
}
