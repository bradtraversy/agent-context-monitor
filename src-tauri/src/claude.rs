//! Claude plan usage from the login Claude Code saved locally.
//!
//! This is ACM's only network request. The token is sent only to Anthropic and is
//! never logged, stored, or shown. An expired login is not refreshed here, because
//! that would mean writing Claude's credentials file.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use crate::model::{LimitWindow, ProviderUsage, UsageStatus, unix_secs};

const PROVIDER: &str = "claude";
const NAME: &str = "Claude";
const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
pub const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);
const SIGN_IN_HINT: &str = "Sign in to Claude Code in a terminal to see Claude usage.";

#[derive(Deserialize)]
struct Credentials {
    #[serde(rename = "claudeAiOauth")]
    oauth: Option<OAuth>,
}

#[derive(Deserialize)]
struct OAuth {
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
    #[serde(rename = "expiresAt")]
    expires_at_ms: Option<i64>,
    #[serde(rename = "subscriptionType")]
    subscription: Option<String>,
}

#[derive(Deserialize)]
struct UsageResponse {
    five_hour: Option<UsageWindow>,
    seven_day: Option<UsageWindow>,
}

#[derive(Deserialize)]
struct UsageWindow {
    utilization: Option<f64>,
    resets_at: Option<String>,
}

#[derive(Clone)]
struct Reading {
    fetched_at: String,
    plan: Option<String>,
    session: Option<(f64, Option<i64>)>,
    weekly: Option<(f64, Option<i64>)>,
}

#[derive(Clone)]
pub struct ClaudeSource {
    credentials_path: PathBuf,
    last_good: Option<Reading>,
    problem: Option<String>,
}

impl ClaudeSource {
    pub fn new(claude_home: PathBuf) -> Self {
        ClaudeSource {
            credentials_path: claude_home.join(".credentials.json"),
            last_good: None,
            problem: None,
        }
    }

    pub fn refresh(&mut self, now: SystemTime) {
        match self.fetch(now) {
            Ok(reading) => {
                self.last_good = Some(reading);
                self.problem = None;
            }
            Err(Failure::NoLogin(hint)) => {
                // A missing or expired login must not leave an old number on screen.
                self.last_good = None;
                self.problem = Some(hint.to_string());
            }
            Err(Failure::Request(msg)) => self.problem = Some(msg),
        }
    }

    fn fetch(&self, now: SystemTime) -> Result<Reading, Failure> {
        let raw =
            std::fs::read(&self.credentials_path).map_err(|_| Failure::NoLogin(SIGN_IN_HINT))?;
        let oauth = serde_json::from_slice::<Credentials>(&raw)
            .ok()
            .and_then(|c| c.oauth)
            .ok_or(Failure::NoLogin(SIGN_IN_HINT))?;
        let token = oauth.access_token.ok_or(Failure::NoLogin(SIGN_IN_HINT))?;
        let now_ms = unix_secs(now) * 1000;
        if oauth.expires_at_ms.is_some_and(|exp| exp <= now_ms) {
            return Err(Failure::NoLogin(
                "Claude's saved login expired. Open Claude Code to refresh it.",
            ));
        }

        let response = ureq::get(USAGE_URL)
            .header("Authorization", &format!("Bearer {token}"))
            .header("anthropic-beta", "oauth-2025-04-20")
            .config()
            .timeout_global(Some(Duration::from_secs(15)))
            .build()
            .call();
        let body: UsageResponse = match response {
            Ok(mut r) => r
                .body_mut()
                .read_json()
                .map_err(|_| Failure::Request("Claude usage response was not readable.".into()))?,
            Err(ureq::Error::StatusCode(401 | 403)) => {
                return Err(Failure::NoLogin(SIGN_IN_HINT));
            }
            Err(ureq::Error::StatusCode(code)) => {
                return Err(Failure::Request(format!(
                    "Claude usage request failed ({code})."
                )));
            }
            Err(_) => {
                return Err(Failure::Request(
                    "Couldn't reach Claude to refresh usage.".into(),
                ));
            }
        };

        Ok(Reading {
            fetched_at: rfc3339_now(now),
            plan: oauth.subscription,
            session: body.five_hour.and_then(window_values),
            weekly: body.seven_day.and_then(window_values),
        })
    }

    pub fn usage(&self, now: SystemTime) -> ProviderUsage {
        let Some(reading) = &self.last_good else {
            return ProviderUsage::unknown(
                PROVIDER,
                NAME,
                self.problem
                    .clone()
                    .unwrap_or_else(|| "Checking Claude usage.".into()),
            );
        };
        let now = unix_secs(now);
        let to_window = |w: Option<(f64, Option<i64>)>| match w {
            None => LimitWindow::NotReported,
            Some((_, Some(reset))) if reset <= now => {
                LimitWindow::ResetSinceReading { resets_at: reset }
            }
            Some((used, resets_at)) => LimitWindow::Reported {
                used_percent: used,
                resets_at,
            },
        };
        let session = to_window(reading.session);
        let weekly = to_window(reading.weekly);
        let empty = session == LimitWindow::NotReported && weekly == LimitWindow::NotReported;
        ProviderUsage {
            provider: PROVIDER,
            name: NAME,
            status: if empty {
                UsageStatus::Unknown
            } else {
                UsageStatus::Available
            },
            note: self
                .problem
                .clone()
                .or_else(|| empty.then(|| "Claude reported no plan limits.".into())),
            plan: reading.plan.clone(),
            observed_at: Some(reading.fetched_at.clone()),
            session,
            weekly,
        }
    }
}

enum Failure {
    NoLogin(&'static str),
    Request(String),
}

fn window_values(w: UsageWindow) -> Option<(f64, Option<i64>)> {
    let used = w.utilization?.clamp(0.0, 100.0);
    let reset = w
        .resets_at
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
        .map(|d| d.timestamp());
    Some((used, reset))
}

fn rfc3339_now(now: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(now).to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_usage_windows_and_reset_times() {
        let body: UsageResponse = serde_json::from_str(
            r#"{"five_hour":{"utilization":6.0,"resets_at":"2026-10-19T14:10:00.875750+00:00"},
                "seven_day":{"utilization":2.0,"resets_at":"2026-10-25T00:00:00+00:00"},
                "seven_day_opus":null,"extra_usage":{"is_enabled":false}}"#,
        )
        .unwrap();
        let (used, reset) = window_values(body.five_hour.unwrap()).unwrap();
        assert_eq!(used, 6.0);
        assert_eq!(reset, Some(1_792_419_000));
        assert!(
            window_values(UsageWindow {
                utilization: None,
                resets_at: None
            })
            .is_none()
        );
    }

    #[test]
    fn missing_login_is_unknown_with_a_hint() {
        let mut source = ClaudeSource::new(PathBuf::from("/nonexistent-acm-test"));
        source.refresh(SystemTime::now());
        let usage = source.usage(SystemTime::now());
        assert_eq!(usage.status, UsageStatus::Unknown);
        assert_eq!(usage.note.as_deref(), Some(SIGN_IN_HINT));
    }
}
