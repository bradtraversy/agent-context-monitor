//! Claude plan usage from the login Claude Code saved locally.
//!
//! These are ACM's only network requests. The tokens are sent only to Anthropic and
//! are never logged or shown.
//!
//! Claude Code saves the login in `.credentials.json` on Linux and Windows, and in
//! the Keychain on macOS. Its access token lasts a few hours and Claude Code renews
//! it only while it runs, so when the token is about to expire ACM renews it the way
//! Claude Code does: under Claude Code's own refresh lock, writing the new tokens
//! back to the same file or Keychain item so both keep sharing one login.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use crate::files;
use crate::model::{Limit, LimitWindow, ProviderUsage, UsageAction, unix_secs};
use crate::programs;

const PROVIDER: &str = "claude";
const NAME: &str = "Claude";
const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const SIGN_IN_HINT: &str = "Sign in to Claude Code to see Claude usage.";
const EXPIRED_HINT: &str = "Claude's login expired. Sign in again to see usage.";
const INSTALL_HINT: &str = "Install Claude Code to see Claude usage.";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
/// Renew this long before expiry, as Claude Code does.
const RENEW_AHEAD_MS: i64 = 5 * 60 * 1000;
/// Claude Code treats a refresh lock older than this as left behind by a crash.
const STALE_LOCK: Duration = Duration::from_secs(60);
/// How long to hold off extra usage checks after Claude says there are too many.
const BUSY_BACKOFF_SECS: i64 = 2 * 60;
const OAUTH_KEY: &str = "claudeAiOauth";
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

/// The Keychain item Claude Code uses. With its own config folder it adds the start
/// of a hash of that folder's path, so each folder keeps a separate login.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn keychain_service() -> String {
    let var = |name| std::env::var(name).ok();
    let (default, dir) = match var("CLAUDE_SECURESTORAGE_CONFIG_DIR") {
        Some(dir) if dir.is_empty() => (true, String::new()),
        Some(dir) => (false, dir),
        None => match var("CLAUDE_CONFIG_DIR") {
            Some(dir) if !dir.is_empty() => (false, dir),
            _ => (true, String::new()),
        },
    };
    keychain_service_for((!default).then_some(dir.as_str()))
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn keychain_service_for(config_dir: Option<&str>) -> String {
    use sha2::{Digest, Sha256};
    match config_dir {
        None => KEYCHAIN_SERVICE.to_string(),
        Some(dir) => {
            let hash: String = Sha256::digest(dir.as_bytes())
                .iter()
                .take(4)
                .map(|b| format!("{b:02x}"))
                .collect();
            format!("{KEYCHAIN_SERVICE}-{hash}")
        }
    }
}

/// Where Claude Code keeps the login, which is also where renewed tokens go.
#[derive(Clone, Copy)]
enum Store {
    File,
    Keychain,
}

#[derive(Deserialize)]
struct Credentials {
    #[serde(rename = "claudeAiOauth")]
    oauth: Option<OAuth>,
}

#[derive(Deserialize)]
struct OAuth {
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
    #[serde(rename = "refreshToken")]
    refresh_token: Option<String>,
    #[serde(rename = "expiresAt")]
    expires_at_ms: Option<i64>,
    #[serde(rename = "subscriptionType")]
    subscription: Option<String>,
    #[serde(default, deserialize_with = "crate::jsonl::null_as_default")]
    scopes: Vec<String>,
    #[serde(rename = "clientId")]
    client_id: Option<String>,
}

impl OAuth {
    fn expires_within(&self, now_ms: i64, ahead_ms: i64) -> bool {
        self.expires_at_ms
            .is_some_and(|exp| exp <= now_ms + ahead_ms)
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
    refresh_token_expires_in: Option<i64>,
    scope: Option<String>,
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
    claude_home: PathBuf,
    credentials_path: PathBuf,
    last_good: Option<Reading>,
    problem: Option<String>,
    /// The saved login, by fingerprint, when it last failed to give a reading, so a new
    /// sign-in can be picked up right away.
    failed_login: Option<u64>,
    can_sign_in: bool,
    /// Whether to look in the macOS Keychain, where Claude Code keeps its live login.
    keychain: bool,
    /// Unix seconds until which Claude has asked for fewer usage checks.
    busy_until: i64,
}

impl ClaudeSource {
    pub fn new(claude_home: PathBuf) -> Self {
        ClaudeSource {
            credentials_path: claude_home.join(".credentials.json"),
            claude_home,
            last_good: None,
            problem: None,
            failed_login: None,
            can_sign_in: false,
            keychain: cfg!(target_os = "macos"),
            busy_until: 0,
        }
    }

    /// True while Claude has asked for fewer usage checks, so only the regular check runs.
    pub fn backing_off(&self, now: SystemTime) -> bool {
        unix_secs(now) < self.busy_until
    }

    /// Whether the saved login changed since it last failed, e.g. after signing in.
    pub fn login_changed(&self) -> bool {
        self.failed_login
            .is_some_and(|failed| failed != self.login_fingerprint())
    }

    fn login_fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.read_login().map(|(raw, _)| raw).hash(&mut hasher);
        hasher.finish()
    }

    pub fn refresh(&mut self, now: SystemTime) {
        match self.fetch(now) {
            Ok(reading) => {
                self.last_good = Some(reading);
                self.problem = None;
                self.failed_login = None;
                self.can_sign_in = false;
            }
            Err(Failure::NoLogin(hint)) => {
                // A missing or expired login must not leave an old number on screen.
                self.last_good = None;
                self.can_sign_in = find_claude().is_some();
                self.problem = Some(no_login_hint(hint, self.can_sign_in).to_string());
                self.failed_login = Some(self.login_fingerprint());
            }
            Err(Failure::Request(msg)) => {
                self.problem = Some(msg);
                self.failed_login = None;
            }
            Err(Failure::TooManyChecks) => {
                // The last reading stays on screen; it is retried after the back-off.
                self.busy_until = unix_secs(now) + BUSY_BACKOFF_SECS;
                self.failed_login = None;
            }
        }
    }

    /// On macOS the Keychain comes first: Claude Code uses the file there only when the
    /// Keychain fails, so a file left over from that can hold an old login.
    fn read_login(&self) -> Option<(Vec<u8>, Store)> {
        let from_keychain = || {
            self.keychain
                .then(keychain_credentials)
                .flatten()
                .map(|raw| (raw, Store::Keychain))
        };
        from_keychain().or_else(|| {
            std::fs::read(&self.credentials_path)
                .ok()
                .map(|raw| (raw, Store::File))
        })
    }

    fn read_store(&self, store: Store) -> Option<Vec<u8>> {
        match store {
            Store::File => std::fs::read(&self.credentials_path).ok(),
            Store::Keychain => keychain_credentials(),
        }
    }

    /// Whether renewed tokens could be saved. Checked before renewing, because once
    /// Claude hands out new tokens the old refresh token stops working, and losing the
    /// new ones would sign Claude Code out too.
    fn can_write_store(&self, store: Store) -> bool {
        match store {
            Store::File => {
                let probe = files::tmp_path(&self.credentials_path);
                let ok = std::fs::File::create(&probe).is_ok();
                let _ = std::fs::remove_file(&probe);
                ok
            }
            Store::Keychain => keychain_item_account().is_some(),
        }
    }

    fn write_store(&self, store: Store, contents: &[u8]) -> bool {
        match store {
            Store::File => files::replace(&self.credentials_path, contents).is_ok(),
            Store::Keychain => save_keychain_credentials(contents),
        }
    }

    fn fetch(&self, now: SystemTime) -> Result<Reading, Failure> {
        let (raw, store) = self.read_login().ok_or(Failure::NoLogin(SIGN_IN_HINT))?;
        let mut oauth = parse_oauth(&raw).ok_or(Failure::NoLogin(SIGN_IN_HINT))?;
        let now_ms = unix_secs(now) * 1000;
        if oauth.expires_within(now_ms, RENEW_AHEAD_MS) {
            match self.renew_login(store, now_ms) {
                Ok(renewed) => oauth = renewed,
                // A token with a few minutes left still works if renewing failed.
                Err(e) if oauth.expires_within(now_ms, 0) => return Err(e),
                Err(_) => {}
            }
        }
        if oauth.expires_within(now_ms, 0) {
            return Err(Failure::NoLogin(EXPIRED_HINT));
        }
        let token = oauth
            .access_token
            .clone()
            .ok_or(Failure::NoLogin(SIGN_IN_HINT))?;

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
            Err(ureq::Error::StatusCode(429)) => return Err(Failure::TooManyChecks),
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

    /// Renews the saved login under Claude Code's refresh lock and writes the new
    /// tokens back where Claude Code keeps them.
    fn renew_login(&self, store: Store, now_ms: i64) -> Result<OAuth, Failure> {
        let busy =
            || Failure::Request("Claude is renewing its login. Checking again shortly.".into());
        let _lock = RefreshLock::acquire(&self.claude_home).ok_or_else(busy)?;

        // Claude Code may have renewed it while ACM waited for the lock.
        let raw = self
            .read_store(store)
            .ok_or(Failure::NoLogin(SIGN_IN_HINT))?;
        let oauth = parse_oauth(&raw).ok_or(Failure::NoLogin(SIGN_IN_HINT))?;
        if !oauth.expires_within(now_ms, RENEW_AHEAD_MS) {
            return Ok(oauth);
        }
        if !self.can_write_store(store) {
            return Err(Failure::Request(
                "Couldn't save a renewed Claude login, so it wasn't renewed.".into(),
            ));
        }
        let refresh_token = oauth
            .refresh_token
            .as_deref()
            .ok_or(Failure::NoLogin(EXPIRED_HINT))?;

        let request = serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": oauth.client_id.as_deref().unwrap_or(CLIENT_ID),
            "scope": oauth.scopes.join(" "),
        });
        let response = ureq::post(TOKEN_URL)
            .config()
            .timeout_global(Some(Duration::from_secs(15)))
            .build()
            .send_json(&request);
        let tokens: TokenResponse = match response {
            Ok(mut r) => r
                .body_mut()
                .read_json()
                .map_err(|_| Failure::Request("Claude's login renewal was not readable.".into()))?,
            // The refresh token itself was rejected, so only a new sign-in helps.
            Err(ureq::Error::StatusCode(400 | 401 | 403)) => {
                return Err(Failure::NoLogin(SIGN_IN_HINT));
            }
            Err(ureq::Error::StatusCode(code)) => {
                return Err(Failure::Request(format!(
                    "Renewing Claude's login failed ({code})."
                )));
            }
            Err(_) => {
                return Err(Failure::Request(
                    "Couldn't reach Claude to renew its login.".into(),
                ));
            }
        };

        let updated = apply_tokens(&raw, &tokens, now_ms)
            .ok_or_else(|| Failure::Request("Claude's saved login was not readable.".into()))?;
        // The old refresh token is spent now, so try hard to keep the new one.
        let saved = (0..3).any(|attempt| {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(500));
            }
            self.write_store(store, &updated)
        });
        if !saved {
            return Err(Failure::Request(
                "Couldn't save Claude's renewed login.".into(),
            ));
        }
        parse_oauth(&updated).ok_or(Failure::NoLogin(SIGN_IN_HINT))
    }

    pub fn usage(&self, now: SystemTime) -> ProviderUsage {
        let Some(reading) = &self.last_good else {
            let mut usage = ProviderUsage::unknown(
                PROVIDER,
                NAME,
                self.problem
                    .clone()
                    .unwrap_or_else(|| "Checking Claude usage.".into()),
            );
            usage.action = self.can_sign_in.then_some(UsageAction::SignIn);
            return usage;
        };
        let now = unix_secs(now);
        let to_window = |w: Option<(f64, Option<i64>)>| {
            w.map_or(LimitWindow::NotReported, |(used, reset)| {
                LimitWindow::from_reading(used, reset, now)
            })
        };
        let mut usage = ProviderUsage::from_reading(
            PROVIDER,
            NAME,
            vec![
                Limit::session(to_window(reading.session)),
                Limit::weekly(to_window(reading.weekly)),
            ],
            reading.plan.clone(),
            reading.fetched_at.clone(),
            "Claude reported no plan limits.",
        );
        // A failed check leaves the last reading up, with the problem beside it.
        usage.note = self.problem.clone().or(usage.note);
        usage
    }
}

fn find_claude() -> Option<PathBuf> {
    let names: &[&str] = if cfg!(windows) {
        &["claude.exe", "claude.cmd"]
    } else {
        &["claude"]
    };
    let home = std::env::home_dir().unwrap_or_default();
    // Older installs keep Claude Code in its own folder instead of on PATH.
    programs::find(names)
        .or_else(|| Some(home.join(".claude").join("local").join(names[0])).filter(|p| p.is_file()))
}

/// Opens a terminal running Claude Code's own sign-in.
pub fn sign_in() -> Result<(), String> {
    let claude = find_claude().ok_or_else(|| INSTALL_HINT.to_string())?;
    programs::open_in_terminal(&claude, &["auth", "login"])
        .map_err(|_| "Couldn't open a terminal. Run claude auth login in one to sign in.".into())
}

/// Without Claude Code there's nothing to sign in with, so say to install it instead.
fn no_login_hint(hint: &'static str, installed: bool) -> &'static str {
    if installed { hint } else { INSTALL_HINT }
}

enum Failure {
    NoLogin(&'static str),
    Request(String),
    /// Claude answered 429: usage is being checked too often.
    TooManyChecks,
}

fn parse_oauth(raw: &[u8]) -> Option<OAuth> {
    serde_json::from_slice::<Credentials>(raw).ok()?.oauth
}

/// The credentials file with renewed tokens, leaving every other field as it was.
fn apply_tokens(raw: &[u8], tokens: &TokenResponse, now_ms: i64) -> Option<Vec<u8>> {
    let mut creds: serde_json::Value = serde_json::from_slice(raw).ok()?;
    let oauth = creds.get_mut(OAUTH_KEY)?.as_object_mut()?;
    oauth.insert("accessToken".into(), tokens.access_token.clone().into());
    if let Some(refresh) = &tokens.refresh_token {
        oauth.insert("refreshToken".into(), refresh.clone().into());
    }
    oauth.insert(
        "expiresAt".into(),
        (now_ms + tokens.expires_in * 1000).into(),
    );
    if let Some(secs) = tokens.refresh_token_expires_in {
        oauth.insert(
            "refreshTokenExpiresAt".into(),
            (now_ms + secs * 1000).into(),
        );
    }
    if let Some(scope) = &tokens.scope {
        let scopes: Vec<&str> = scope.split_whitespace().collect();
        oauth.insert("scopes".into(), scopes.into());
    }
    serde_json::to_vec(&creds).ok()
}

/// Claude Code's OAuth refresh lock: the `.oauth_refresh.lock` directory in its
/// config folder plus the older `<config folder>.lock`, taken in that order the way
/// its lock library does, by creating the directories.
struct RefreshLock {
    held: Vec<PathBuf>,
}

impl RefreshLock {
    fn acquire(claude_home: &Path) -> Option<RefreshLock> {
        let real = std::fs::canonicalize(claude_home).unwrap_or_else(|_| claude_home.to_path_buf());
        let mut legacy = real.into_os_string();
        legacy.push(".lock");
        let mut lock = RefreshLock { held: Vec::new() };
        for path in [
            claude_home.join(".oauth_refresh.lock"),
            PathBuf::from(legacy),
        ] {
            if !take_lock_dir(&path) {
                return None; // Drop releases whatever was taken.
            }
            lock.held.push(path);
        }
        Some(lock)
    }
}

impl Drop for RefreshLock {
    fn drop(&mut self) {
        for path in self.held.iter().rev() {
            let _ = std::fs::remove_dir(path);
        }
    }
}

fn take_lock_dir(path: &Path) -> bool {
    if std::fs::create_dir(path).is_ok() {
        return true;
    }
    let stale = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| m.elapsed().ok())
        .is_some_and(|age| age > STALE_LOCK);
    stale && std::fs::remove_dir(path).is_ok() && std::fs::create_dir(path).is_ok()
}

/// Runs the system's own Keychain tool, which Claude Code's item already trusts, so
/// reading and writing it never asks for permission.
#[cfg(target_os = "macos")]
fn security(args: &[&str], input: Option<&str>) -> Option<Vec<u8>> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("/usr/bin/security")
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        let _ = stdin.write_all(input.as_bytes());
    }
    let out = child.wait_with_output().ok()?;
    out.status.success().then_some(out.stdout)
}

/// The login Claude Code keeps in the macOS Keychain.
#[cfg(target_os = "macos")]
fn keychain_credentials() -> Option<Vec<u8>> {
    let mut raw = security(
        &["find-generic-password", "-s", &keychain_service(), "-w"],
        None,
    )?;
    while raw.last().is_some_and(u8::is_ascii_whitespace) {
        raw.pop();
    }
    Some(raw)
}

/// Updates Claude Code's Keychain item in place, the way Claude Code writes it, and
/// reads it back to be sure it took.
#[cfg(target_os = "macos")]
fn save_keychain_credentials(contents: &[u8]) -> bool {
    let Some(account) = keychain_item_account() else {
        return false;
    };
    let hex: String = contents.iter().map(|b| format!("{b:02x}")).collect();
    let service = keychain_service();
    let line = keychain_update_line(&account, &service, &hex);
    // Fed on stdin so the login stays out of the process list, unless it's too long
    // for one line, where Claude Code passes it as arguments instead.
    let saved = if line.len() <= SECURITY_LINE_MAX {
        security(&["-i"], Some(&line))
    } else {
        security(
            &[
                "add-generic-password",
                "-U",
                "-a",
                &account,
                "-s",
                &service,
                "-X",
                &hex,
            ],
            None,
        )
    };
    saved.is_some() && keychain_credentials().is_some_and(|raw| raw == contents)
}

/// The account name on Claude Code's Keychain item, needed to update it.
#[cfg(target_os = "macos")]
fn keychain_item_account() -> Option<String> {
    security(&["find-generic-password", "-s", &keychain_service()], None)
        .and_then(|attrs| keychain_account(&String::from_utf8_lossy(&attrs)))
}

#[cfg(not(target_os = "macos"))]
fn keychain_item_account() -> Option<String> {
    None
}

#[cfg(not(target_os = "macos"))]
fn keychain_credentials() -> Option<Vec<u8>> {
    None
}

#[cfg(not(target_os = "macos"))]
fn save_keychain_credentials(_contents: &[u8]) -> bool {
    false
}

/// The longest command `security -i` reads, the limit Claude Code uses.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const SECURITY_LINE_MAX: usize = 4032;

/// The account name on Claude Code's Keychain item, from `security`'s attribute listing.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn keychain_account(attributes: &str) -> Option<String> {
    let account = attributes
        .lines()
        .find_map(|l| l.trim().strip_prefix("\"acct\"<blob>=\""))?
        .strip_suffix('"')?;
    // Claude Code only uses names like this, which are also safe inside quotes.
    let plain = !account.is_empty()
        && account
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    plain.then(|| account.to_string())
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn keychain_update_line(account: &str, service: &str, hex: &str) -> String {
    format!("add-generic-password -U -a \"{account}\" -s \"{service}\" -X \"{hex}\"\n")
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
    use crate::model::UsageStatus;

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
    fn renewed_tokens_keep_the_rest_of_the_file() {
        let raw = br#"{"mcpOAuth":{"x":1},"organizationUuid":"org",
            "claudeAiOauth":{"accessToken":"old","refreshToken":"r1","expiresAt":1,
            "scopes":["user:inference"],"subscriptionType":"max","rateLimitTier":"t"}}"#;
        let tokens = TokenResponse {
            access_token: "new".into(),
            refresh_token: Some("r2".into()),
            expires_in: 3600,
            refresh_token_expires_in: None,
            scope: Some("user:inference user:profile".into()),
        };
        let updated = apply_tokens(raw, &tokens, 1_000).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&updated).unwrap();
        let o = &v["claudeAiOauth"];
        assert_eq!(o["accessToken"], "new");
        assert_eq!(o["refreshToken"], "r2");
        assert_eq!(o["expiresAt"], 3_601_000);
        assert_eq!(
            o["scopes"],
            serde_json::json!(["user:inference", "user:profile"])
        );
        assert_eq!(o["subscriptionType"], "max");
        assert_eq!(o["rateLimitTier"], "t");
        assert_eq!(v["mcpOAuth"]["x"], 1);
        assert_eq!(v["organizationUuid"], "org");
    }

    #[test]
    fn refresh_lock_is_shared_with_claude_code() {
        let dir = std::env::temp_dir().join(format!("acm-claude-lock-{}", std::process::id()));
        let home = dir.join(".claude");
        std::fs::create_dir_all(&home).unwrap();
        let first = RefreshLock::acquire(&home).unwrap();
        assert!(home.join(".oauth_refresh.lock").is_dir());
        assert!(dir.join(".claude.lock").is_dir());
        assert!(RefreshLock::acquire(&home).is_none());
        // The failed attempt must not release the lock someone else holds.
        assert!(home.join(".oauth_refresh.lock").is_dir());
        drop(first);
        assert!(!home.join(".oauth_refresh.lock").exists());
        assert!(!dir.join(".claude.lock").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finds_the_keychain_account_and_builds_the_update() {
        let attrs = "keychain: \"/Users/me/Library/Keychains/login.keychain-db\"\nclass: \"genp\"\nattributes:\n    \"acct\"<blob>=\"brad.t-1\"\n    \"svce\"<blob>=\"Claude Code-credentials\"\n";
        assert_eq!(keychain_account(attrs).as_deref(), Some("brad.t-1"));
        assert_eq!(
            keychain_account("    \"acct\"<blob>=0x00FF  \"\\000\"\n"),
            None
        );
        assert_eq!(keychain_account("    \"acct\"<blob>=\"a\" b\"\n"), None);
        assert_eq!(keychain_account("    \"acct\"<blob>=<NULL>\n"), None);
        assert_eq!(
            keychain_update_line("me", KEYCHAIN_SERVICE, "7b7d"),
            "add-generic-password -U -a \"me\" -s \"Claude Code-credentials\" -X \"7b7d\"\n"
        );
    }

    #[test]
    fn a_separate_config_folder_has_its_own_keychain_item() {
        assert_eq!(keychain_service_for(None), "Claude Code-credentials");
        assert_eq!(
            keychain_service_for(Some("/Users/me/.claude-work")),
            "Claude Code-credentials-1e91dd84"
        );
    }

    #[test]
    fn missing_login_is_unknown_with_a_hint() {
        let mut source = ClaudeSource::new(PathBuf::from("/nonexistent-acm-test"));
        source.keychain = false; // Keep a developer's own Mac login out of the test.
        source.refresh(SystemTime::now());
        let usage = source.usage(SystemTime::now());
        assert_eq!(usage.status, UsageStatus::Unknown);
        let installed = find_claude().is_some();
        assert_eq!(
            usage.note.as_deref(),
            Some(no_login_hint(SIGN_IN_HINT, installed))
        );
        assert_eq!(usage.action.is_some(), installed);
        assert_eq!(no_login_hint(EXPIRED_HINT, false), INSTALL_HINT);
        assert_eq!(no_login_hint(EXPIRED_HINT, true), EXPIRED_HINT);
    }

    #[test]
    fn a_new_login_is_noticed_after_a_failed_one() {
        let home = std::env::temp_dir().join(format!("acm-claude-login-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let mut source = ClaudeSource::new(home.clone());
        source.keychain = false;
        assert!(!source.login_changed());
        // No token is expired-only, so this fails without a network request.
        std::fs::write(home.join(".credentials.json"), r#"{"claudeAiOauth":{}}"#).unwrap();
        source.refresh(SystemTime::now());
        assert!(source.problem.is_some());
        assert!(!source.login_changed());
        std::fs::write(
            home.join(".credentials.json"),
            r#"{"claudeAiOauth":{"accessToken":"new"}}"#,
        )
        .unwrap();
        assert!(source.login_changed());
        let _ = std::fs::remove_dir_all(&home);
    }
}
