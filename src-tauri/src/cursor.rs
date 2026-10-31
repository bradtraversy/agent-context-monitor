//! Cursor usage: how much of the month's included usage is gone, from the request Cursor's
//! own dashboard makes, signed in with the login Cursor saved on this computer.
//!
//! Cursor's chats sit in its editor database in a format that changes often, so Cursor adds
//! a usage card but no chats.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;

use crate::model::{Limit, LimitWindow, ProviderInfo, ProviderUsage, unix_secs};
use crate::sources::{Failure, UsageSource, plan_label};

const INFO: ProviderInfo = ProviderInfo {
    id: "cursor",
    name: "Cursor",
    chats: false,
};
const USAGE_URL: &str = "https://cursor.com/api/usage-summary";
const SIGN_IN_HINT: &str = "Open Cursor and sign in to see Cursor usage.";
const EXPIRED_HINT: &str = "Cursor's saved login has expired. Open Cursor to renew it.";

pub struct CursorSource {
    home: PathBuf,
    config_dir: PathBuf,
}

impl CursorSource {
    /// `config_dir` is where apps keep settings: `~/.config`, `~/Library/Application
    /// Support`, or `%APPDATA%`.
    pub fn new(home: PathBuf, config_dir: PathBuf) -> Self {
        CursorSource { home, config_dir }
    }

    fn editor_db(&self) -> PathBuf {
        self.config_dir
            .join("Cursor")
            .join("User")
            .join("globalStorage")
            .join("state.vscdb")
    }

    /// Where the Cursor CLI (cursor-agent) keeps its login.
    fn cli_login(&self) -> PathBuf {
        if cfg!(target_os = "macos") {
            self.home.join(".cursor").join("auth.json")
        } else if cfg!(windows) {
            self.config_dir.join("Cursor").join("auth.json")
        } else {
            let config = std::env::var_os("XDG_CONFIG_HOME")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| self.home.join(".config"));
            config.join("cursor").join("auth.json")
        }
    }

    fn token(&self) -> Option<String> {
        editor_token(&self.editor_db()).or_else(|| {
            let raw = std::fs::read(self.cli_login()).ok()?;
            let value: serde_json::Value = serde_json::from_slice(&raw).ok()?;
            value["accessToken"].as_str().map(str::to_owned)
        })
    }
}

/// The editor's login, read without writing to a database Cursor has open.
fn editor_token(db: &std::path::Path) -> Option<String> {
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    conn.query_row(
        "SELECT value FROM ItemTable WHERE key = 'cursorAuth/accessToken'",
        [],
        // Saved as text or as bytes, depending on the version.
        |row| {
            let value = row.get_ref(0)?;
            Ok(value
                .as_str()
                .ok()
                .or_else(|| {
                    value
                        .as_blob()
                        .ok()
                        .and_then(|b| std::str::from_utf8(b).ok())
                })
                .map(|t| t.trim_matches('"').to_string()))
        },
    )
    .ok()
    .flatten()
    .filter(|t| !t.is_empty())
}

#[derive(Deserialize)]
struct Claims {
    sub: Option<String>,
    exp: Option<i64>,
}

/// The user id and expiry inside Cursor's login token.
fn claims(token: &str) -> Option<Claims> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The dashboard's session cookie: the user id, then the token.
fn session_cookie(token: &str, sub: &str) -> Option<String> {
    let user = sub.rsplit('|').next().filter(|u| !u.is_empty())?;
    Some(format!("WorkosCursorSessionToken={user}%3A%3A{token}"))
}

impl UsageSource for CursorSource {
    fn info(&self) -> ProviderInfo {
        INFO
    }

    fn installed(&self) -> bool {
        self.editor_db().is_file() || self.cli_login().is_file()
    }

    fn fetch(&mut self, now: SystemTime) -> Result<ProviderUsage, Failure> {
        let token = self
            .token()
            .ok_or_else(|| Failure::NoLogin(SIGN_IN_HINT.into()))?;
        let claims = claims(&token).ok_or_else(|| Failure::NoLogin(SIGN_IN_HINT.into()))?;
        if claims.exp.is_some_and(|exp| exp <= unix_secs(now)) {
            return Err(Failure::NoLogin(EXPIRED_HINT.into()));
        }
        let cookie = claims
            .sub
            .as_deref()
            .and_then(|sub| session_cookie(&token, sub))
            .ok_or_else(|| Failure::NoLogin(SIGN_IN_HINT.into()))?;
        let response = ureq::get(USAGE_URL)
            .header("Cookie", &cookie)
            .header("Accept", "application/json")
            .header("Referer", "https://cursor.com/dashboard")
            .config()
            .timeout_global(Some(Duration::from_secs(15)))
            .build()
            .call();
        let body: Summary = match response {
            Ok(mut r) => r
                .body_mut()
                .read_json()
                .map_err(|_| Failure::Request("Cursor usage response was not readable.".into()))?,
            Err(ureq::Error::StatusCode(401 | 403)) => {
                return Err(Failure::NoLogin(EXPIRED_HINT.into()));
            }
            Err(ureq::Error::StatusCode(429)) => return Err(Failure::TooManyChecks),
            Err(ureq::Error::StatusCode(code)) => {
                return Err(Failure::Request(format!(
                    "Cursor usage request failed ({code})."
                )));
            }
            Err(_) => {
                return Err(Failure::Request(
                    "Couldn't reach Cursor to refresh usage.".into(),
                ));
            }
        };
        Ok(usage_from(body, now))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Summary {
    billing_cycle_end: Option<String>,
    membership_type: Option<String>,
    #[serde(default)]
    is_unlimited: bool,
    individual_usage: Option<Individual>,
}

#[derive(Deserialize)]
struct Individual {
    plan: Option<Plan>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Plan {
    total_percent_used: Option<f64>,
    /// Cursor's own models: Auto and Composer.
    auto_percent_used: Option<f64>,
    /// Every other model, such as Claude or GPT picked by name.
    api_percent_used: Option<f64>,
}

fn usage_from(body: Summary, now: SystemTime) -> ProviderUsage {
    let reset = body
        .billing_cycle_end
        .as_deref()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.timestamp());
    let window = |used: Option<f64>| {
        used.map_or(LimitWindow::NotReported, |used| {
            LimitWindow::from_reading(used, reset, unix_secs(now))
        })
    };
    let plan = body.individual_usage.and_then(|u| u.plan);
    let limits = match plan.filter(|_| !body.is_unlimited) {
        None => Vec::new(),
        Some(plan) => vec![
            Limit::monthly(window(plan.total_percent_used)),
            Limit {
                id: "auto",
                label: "Auto and Composer",
                short: "Auto",
                window: window(plan.auto_percent_used),
            },
            Limit {
                id: "api",
                label: "Other models",
                short: "other models",
                window: window(plan.api_percent_used),
            },
        ],
    };
    ProviderUsage::from_reading(
        INFO.id,
        INFO.name,
        limits,
        body.membership_type.as_deref().and_then(plan_label),
        chrono::DateTime::<chrono::Utc>::from(now).to_rfc3339(),
        "Cursor reported no monthly limits for your plan.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::UsageStatus;
    use std::time::UNIX_EPOCH;

    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_793_000_000)
    }

    #[test]
    fn reads_the_month_and_both_model_pools() {
        let body: Summary = serde_json::from_str(
            r#"{"billingCycleStart":"2026-10-05T00:00:00.000Z","billingCycleEnd":"2026-11-05T00:00:00.000Z",
            "membershipType":"pro","isUnlimited":false,
            "individualUsage":{"plan":{"enabled":true,"used":951,"limit":2000,"remaining":1049,
              "totalPercentUsed":47.5,"autoPercentUsed":12.0,"apiPercentUsed":83.0},
              "onDemand":{"enabled":false,"used":0,"limit":null}}}"#,
        )
        .unwrap();
        let usage = usage_from(body, now());
        assert_eq!(usage.plan.as_deref(), Some("Pro"));
        let used: Vec<(&str, f64)> = usage
            .limits
            .iter()
            .map(|l| match l.window {
                LimitWindow::Reported {
                    used_percent,
                    resets_at,
                } => {
                    assert_eq!(resets_at, Some(1_793_836_800));
                    (l.id, used_percent)
                }
                _ => (l.id, -1.0),
            })
            .collect();
        assert_eq!(used, vec![("monthly", 47.5), ("auto", 12.0), ("api", 83.0)]);
    }

    #[test]
    fn a_team_plan_without_individual_usage_is_unknown() {
        let body: Summary =
            serde_json::from_str(r#"{"membershipType":"team","individualUsage":{}}"#).unwrap();
        let usage = usage_from(body, now());
        assert_eq!(usage.status, UsageStatus::Unknown);
    }

    #[test]
    fn builds_the_dashboard_cookie_from_the_login() {
        let payload = URL_SAFE_NO_PAD.encode(br#"{"sub":"auth0|user_01ABC","exp":1900000000}"#);
        let token = format!("eyJhbGciOiJIUzI1NiJ9.{payload}.sig");
        let claims = claims(&token).unwrap();
        assert_eq!(claims.exp, Some(1_900_000_000));
        assert_eq!(
            session_cookie(&token, claims.sub.as_deref().unwrap()).unwrap(),
            format!("WorkosCursorSessionToken=user_01ABC%3A%3A{token}")
        );
    }

    #[test]
    fn reads_the_login_from_the_editor_database() {
        let dir = std::env::temp_dir().join(format!("acm-cursor-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("state.vscdb");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);
             INSERT INTO ItemTable VALUES ('cursorAuth/accessToken', 'a.b.c');",
        )
        .unwrap();
        drop(conn);
        assert_eq!(editor_token(&db).as_deref(), Some("a.b.c"));
        assert_eq!(editor_token(&dir.join("missing.vscdb")), None);
        let _ = std::fs::remove_dir_all(dir);
    }
}
