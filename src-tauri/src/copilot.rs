//! GitHub Copilot usage: the monthly premium request allowance, from the same GitHub
//! request Copilot's own apps use, signed in with a login already saved on this computer.
//!
//! Copilot CLI keeps context sizes out of its session files (they are sent live only), so
//! Copilot adds a usage card but no chats.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use crate::model::{Limit, LimitWindow, ProviderInfo, ProviderUsage, unix_secs};
use crate::programs;
use crate::sources::{Failure, UsageSource, plan_label};

const INFO: ProviderInfo = ProviderInfo {
    id: "copilot",
    name: "Copilot",
    chats: false,
};
const USER_URL: &str = "https://api.github.com/copilot_internal/user";
const SIGN_IN_HINT: &str = "Sign in with Copilot CLI (copilot login) or the GitHub CLI (gh auth login) to see Copilot usage.";

pub struct CopilotSource {
    home: PathBuf,
    /// The login that worked last, tried first next time.
    token: Option<String>,
    /// macOS asks before letting another app read Copilot CLI's Keychain item, so it is
    /// only asked once per run.
    asked_keychain: bool,
}

impl CopilotSource {
    pub fn new(home: PathBuf) -> Self {
        CopilotSource {
            home,
            token: None,
            asked_keychain: false,
        }
    }

    fn config_dir(&self) -> PathBuf {
        match std::env::var_os("LOCALAPPDATA") {
            Some(local) if cfg!(windows) => PathBuf::from(local).join("github-copilot"),
            _ => self.home.join(".config").join("github-copilot"),
        }
    }

    /// Saved GitHub logins, the ones that need no prompt first.
    fn tokens(&mut self) -> Vec<String> {
        let mut found: Vec<String> = ["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"]
            .iter()
            .filter_map(|name| std::env::var(name).ok())
            .filter(|t| is_github_token(t))
            .collect();
        // Copilot in JetBrains, Neovim, and Xcode, and Copilot CLI without a keychain.
        for file in ["apps.json", "hosts.json"] {
            found.extend(file_tokens(&self.config_dir().join(file)));
        }
        found.extend(file_tokens(&self.home.join(".copilot").join("config.json")));
        found.extend(gh_token());
        if found.is_empty() && !std::mem::replace(&mut self.asked_keychain, true) {
            found.extend(keychain_token());
        }
        let mut unique = Vec::new();
        for token in found {
            let token = token.trim().to_string();
            if !token.is_empty() && !unique.contains(&token) {
                unique.push(token);
            }
        }
        unique
    }
}

impl UsageSource for CopilotSource {
    fn info(&self) -> ProviderInfo {
        INFO
    }

    fn installed(&self) -> bool {
        let vscode_extension = fs::read_dir(self.home.join(".vscode").join("extensions"))
            .map(|entries| {
                entries.flatten().any(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with("github.copilot")
                })
            })
            .unwrap_or(false);
        self.home.join(".copilot").is_dir()
            || self.config_dir().is_dir()
            || vscode_extension
            || programs::find(&["copilot", "copilot.cmd", "copilot.exe"]).is_some()
    }

    fn fetch(&mut self, now: SystemTime) -> Result<ProviderUsage, Failure> {
        // The login that worked last time saves looking through them all again.
        if let Some(token) = self.token.clone() {
            match self.fetch_with(&[token], now) {
                Err(Failure::NoLogin(_)) => self.token = None,
                result => return result,
            }
        }
        let tokens = self.tokens();
        self.fetch_with(&tokens, now)
    }
}

impl CopilotSource {
    fn fetch_with(&mut self, tokens: &[String], now: SystemTime) -> Result<ProviderUsage, Failure> {
        let mut turned_down = false;
        for token in tokens {
            let response = ureq::get(USER_URL)
                .header("Authorization", &format!("token {token}"))
                .header("Accept", "application/json")
                .header("X-GitHub-Api-Version", "2025-04-01")
                .config()
                .timeout_global(Some(Duration::from_secs(15)))
                .build()
                .call();
            let body: UserResponse = match response {
                Ok(mut r) => r.body_mut().read_json().map_err(|_| {
                    Failure::Request("Copilot usage response was not readable.".into())
                })?,
                // A login without Copilot access, or an old one; another may work.
                Err(ureq::Error::StatusCode(401 | 403 | 404)) => {
                    turned_down = true;
                    continue;
                }
                Err(ureq::Error::StatusCode(429)) => return Err(Failure::TooManyChecks),
                Err(ureq::Error::StatusCode(code)) => {
                    return Err(Failure::Request(format!(
                        "Copilot usage request failed ({code})."
                    )));
                }
                Err(_) => {
                    return Err(Failure::Request(
                        "Couldn't reach GitHub to refresh Copilot usage.".into(),
                    ));
                }
            };
            self.token = Some(token.clone());
            return Ok(usage_from(body, now));
        }
        Err(Failure::NoLogin(if turned_down {
            "GitHub turned down the saved login. Sign in again with copilot login or gh auth login."
                .into()
        } else {
            SIGN_IN_HINT.into()
        }))
    }
}

#[derive(Deserialize)]
struct UserResponse {
    copilot_plan: Option<String>,
    quota_reset_date_utc: Option<String>,
    quota_reset_date: Option<String>,
    #[serde(default)]
    quota_snapshots: HashMap<String, Quota>,
    /// The free plan's older shape: what is left, and the monthly allowance.
    #[serde(default)]
    limited_user_quotas: HashMap<String, f64>,
    #[serde(default)]
    monthly_quotas: HashMap<String, f64>,
    limited_user_reset_date: Option<String>,
}

#[derive(Deserialize)]
struct Quota {
    entitlement: Option<f64>,
    remaining: Option<f64>,
    percent_remaining: Option<f64>,
    #[serde(default)]
    unlimited: bool,
}

impl Quota {
    fn used_percent(&self) -> Option<f64> {
        if self.unlimited {
            return None;
        }
        if let Some(left) = self.percent_remaining {
            return Some(100.0 - left);
        }
        let entitlement = self.entitlement.filter(|&e| e > 0.0)?;
        Some((entitlement - self.remaining?) / entitlement * 100.0)
    }
}

/// Each allowance the plan limits, premium requests first. Unlimited ones are left out.
const QUOTAS: [(&str, &str, &str, &str); 3] = [
    (
        "premium_interactions",
        "monthly",
        "Premium requests this month",
        "premium",
    ),
    ("chat", "chat", "Chat messages this month", "chat"),
    (
        "completions",
        "completions",
        "Code completions this month",
        "completions",
    ),
];

fn usage_from(body: UserResponse, now: SystemTime) -> ProviderUsage {
    let reset = body
        .quota_reset_date_utc
        .as_deref()
        .or(body.quota_reset_date.as_deref())
        .or(body.limited_user_reset_date.as_deref())
        .and_then(parse_date);
    let limits = QUOTAS
        .iter()
        .filter_map(|&(key, id, label, short)| {
            let used = match body.quota_snapshots.get(key) {
                Some(quota) => quota.used_percent()?,
                None => {
                    let allowed = body.monthly_quotas.get(key).copied().filter(|&a| a > 0.0)?;
                    let left = body.limited_user_quotas.get(key).copied()?;
                    (allowed - left) / allowed * 100.0
                }
            };
            Some(Limit {
                id,
                label,
                short,
                window: LimitWindow::from_reading(used, reset, unix_secs(now)),
            })
        })
        .collect();
    ProviderUsage::from_reading(
        INFO.id,
        INFO.name,
        limits,
        body.copilot_plan.as_deref().and_then(plan_label),
        chrono::DateTime::<chrono::Utc>::from(now).to_rfc3339(),
        "Your Copilot plan has no monthly limits.",
    )
}

/// GitHub sends the reset as a full time or as a date, which starts at midnight UTC.
fn parse_date(text: &str) -> Option<i64> {
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(text) {
        return Some(t.timestamp());
    }
    chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .ok()?
        .and_hms_opt(0, 0, 0)
        .map(|t| t.and_utc().timestamp())
}

/// GitHub logins saved in a JSON file, wherever they sit in it.
fn file_tokens(path: &Path) -> Vec<String> {
    let Ok(value) = fs::read(path).map(|b| serde_json::from_slice::<serde_json::Value>(&b)) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    collect_tokens(&value.unwrap_or_default(), &mut found);
    found
}

fn collect_tokens(value: &serde_json::Value, found: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) if is_github_token(s) => found.push(s.clone()),
        serde_json::Value::Array(items) => items.iter().for_each(|v| collect_tokens(v, found)),
        serde_json::Value::Object(map) => map.values().for_each(|v| collect_tokens(v, found)),
        _ => {}
    }
}

/// Classic personal access tokens (ghp_) can't use Copilot, so they aren't tried.
fn is_github_token(s: &str) -> bool {
    ["gho_", "ghu_", "github_pat_"]
        .iter()
        .any(|prefix| s.starts_with(prefix))
}

fn run(program: &Path, args: &[&str]) -> Option<String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Keeps a console window from flashing up.
        command.creation_flags(0x0800_0000);
    }
    let out = command.output().ok()?;
    let text = String::from_utf8(out.stdout).ok()?;
    (out.status.success() && !text.trim().is_empty()).then(|| text.trim().to_string())
}

/// The GitHub CLI's login, which Copilot CLI also falls back to.
fn gh_token() -> Option<String> {
    run(&programs::find(&["gh", "gh.exe"])?, &["auth", "token"])
}

/// Copilot CLI's own login, kept in the system keychain under `copilot-cli`.
#[cfg(target_os = "macos")]
fn keychain_token() -> Option<String> {
    run(
        Path::new("/usr/bin/security"),
        &["find-generic-password", "-s", "copilot-cli", "-w"],
    )
}

#[cfg(target_os = "linux")]
fn keychain_token() -> Option<String> {
    run(
        &programs::find(&["secret-tool"])?,
        &["lookup", "service", "copilot-cli"],
    )
}

#[cfg(windows)]
fn keychain_token() -> Option<String> {
    use windows_sys::Win32::Security::Credentials::{CREDENTIALW, CredEnumerateW, CredFree};
    let filter: Vec<u16> = "copilot-cli*".encode_utf16().chain([0]).collect();
    let mut count = 0u32;
    let mut list: *mut *mut CREDENTIALW = std::ptr::null_mut();
    // SAFETY: Windows fills `list` with `count` credentials, freed below with CredFree.
    unsafe {
        if CredEnumerateW(filter.as_ptr(), 0, &mut count, &mut list) == 0 {
            return None;
        }
        let creds = std::slice::from_raw_parts(list, count as usize);
        let token = creds.iter().find_map(|&cred| {
            let cred = &*cred;
            let blob =
                std::slice::from_raw_parts(cred.CredentialBlob, cred.CredentialBlobSize as usize);
            String::from_utf8(blob.to_vec())
                .ok()
                .filter(|t| !t.is_empty())
        });
        CredFree(list as *const std::ffi::c_void);
        token
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn keychain_token() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::UsageStatus;
    use std::time::UNIX_EPOCH;

    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_793_000_000) // 2026-10-26
    }

    fn parse(json: &str) -> ProviderUsage {
        usage_from(serde_json::from_str(json).unwrap(), now())
    }

    #[test]
    fn premium_requests_are_the_monthly_limit() {
        let usage = parse(
            r#"{"copilot_plan":"individual_pro","quota_reset_date":"2026-11-01",
            "quota_snapshots":{
              "premium_interactions":{"entitlement":300,"remaining":75,"percent_remaining":25.0,"unlimited":false},
              "chat":{"entitlement":0,"remaining":0,"percent_remaining":100.0,"unlimited":true},
              "completions":{"entitlement":0,"remaining":0,"percent_remaining":100.0,"unlimited":true}}}"#,
        );
        assert_eq!(usage.plan.as_deref(), Some("Individual pro"));
        assert_eq!(usage.limits.len(), 1);
        assert_eq!(usage.limits[0].id, "monthly");
        assert_eq!(
            usage.limits[0].window,
            LimitWindow::Reported {
                used_percent: 75.0,
                resets_at: Some(1_793_491_200) // 2026-11-01T00:00:00Z
            }
        );
    }

    #[test]
    fn the_free_plan_limits_chat_and_completions() {
        let usage = parse(
            r#"{"copilot_plan":"free","limited_user_reset_date":"2026-11-15",
            "limited_user_quotas":{"chat":40,"completions":1500},
            "monthly_quotas":{"chat":50,"completions":2000}}"#,
        );
        let used: Vec<(&str, f64)> = usage
            .limits
            .iter()
            .map(|l| match l.window {
                LimitWindow::Reported { used_percent, .. } => (l.id, used_percent),
                _ => (l.id, -1.0),
            })
            .collect();
        assert_eq!(used, vec![("chat", 20.0), ("completions", 25.0)]);
    }

    #[test]
    fn an_unlimited_plan_says_so() {
        let usage = parse(
            r#"{"quota_snapshots":{"premium_interactions":{"unlimited":true,"percent_remaining":100}}}"#,
        );
        assert_eq!(usage.status, UsageStatus::Unknown);
        assert_eq!(
            usage.note.as_deref(),
            Some("Your Copilot plan has no monthly limits.")
        );
    }

    #[test]
    fn finds_saved_logins_but_not_classic_tokens() {
        let value = serde_json::json!({
            "github.com:Iv1.b507a08c87ecfe98": {"user": "brad", "oauth_token": "ghu_abc"},
            "other": ["gho_def", "ghp_classic", 3],
        });
        let mut found = Vec::new();
        collect_tokens(&value, &mut found);
        found.sort();
        assert_eq!(found, vec!["gho_def", "ghu_abc"]);
    }

    #[test]
    fn reset_dates_read_with_or_without_a_time() {
        assert_eq!(parse_date("2026-11-01"), Some(1_793_491_200));
        assert_eq!(parse_date("2026-11-01T00:00:00.000Z"), Some(1_793_491_200));
        assert_eq!(parse_date("soon"), None);
    }
}
