//! Agents whose usage is asked for over the network. Each one is a `UsageSource`, checked
//! on its own thread so a slow reply never holds up the others.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use crate::model::{LIVE_REFRESH_AFTER_SECS, ProviderInfo, ProviderUsage, unix_secs};

const TICK: Duration = Duration::from_secs(1);
/// Opening the window re-checks usage unless it was checked this recently.
const OPEN_GAP_SECS: i64 = 5;
/// After a provider asks for fewer checks, only the regular check runs for this long.
const BACKOFF_SECS: i64 = 2 * 60;
/// Looking for the agent is cheap, but needn't happen every second.
const INSTALL_CHECK_SECS: i64 = 30;

/// Why a check didn't get a reading.
#[derive(Debug, PartialEq)]
pub enum Failure {
    /// No saved login, or the provider turned it down. Says what to do about it.
    NoLogin(String),
    Request(String),
    /// The provider asked for fewer checks.
    TooManyChecks,
}

pub trait UsageSource: Send + 'static {
    fn info(&self) -> ProviderInfo;
    /// Whether the agent looks installed, so its card is worth showing.
    fn installed(&self) -> bool;
    fn fetch(&mut self, now: SystemTime) -> Result<ProviderUsage, Failure>;
}

/// The latest usage from a source, or `None` while the agent isn't installed.
pub type Latest = Arc<Mutex<Option<ProviderUsage>>>;

/// Keeps the last good reading up after a failed check, with the problem beside it.
#[derive(Default)]
struct Readings {
    last_good: Option<ProviderUsage>,
    problem: Option<String>,
}

impl Readings {
    fn apply(&mut self, result: Result<ProviderUsage, Failure>) {
        match result {
            Ok(usage) => {
                self.last_good = Some(usage);
                self.problem = None;
            }
            Err(Failure::NoLogin(hint)) => {
                // A login that stopped working means the old numbers can't be trusted.
                self.last_good = None;
                self.problem = Some(hint);
            }
            Err(Failure::Request(problem)) => self.problem = Some(problem),
            Err(Failure::TooManyChecks) => {}
        }
    }

    fn usage(&self, info: &ProviderInfo) -> ProviderUsage {
        match &self.last_good {
            Some(usage) => {
                let mut usage = usage.clone();
                usage.note = self.problem.clone().or(usage.note);
                usage
            }
            None => ProviderUsage::unknown(
                info.id,
                info.name,
                self.problem
                    .clone()
                    .unwrap_or_else(|| format!("Checking {} usage.", info.name)),
            ),
        }
    }
}

/// Whether to check now. The regular check keeps readings under the freshness limit, and
/// opening the window checks sooner unless the provider asked for fewer checks.
fn check_due(now: i64, last_check: i64, opened: bool, backing_off: bool) -> bool {
    let age = now.saturating_sub(last_check);
    age >= LIVE_REFRESH_AFTER_SECS || (opened && !backing_off && age >= OPEN_GAP_SECS)
}

/// Checks `source` on its own thread. `opened` counts window openings, and `first_done`
/// is bumped once the first check is done, or right away if the agent isn't installed.
pub fn spawn(
    mut source: impl UsageSource,
    opened: Arc<AtomicU64>,
    first_done: Arc<AtomicUsize>,
) -> Latest {
    let latest: Latest = Arc::default();
    let shared = Arc::clone(&latest);
    thread::spawn(move || {
        let info = source.info();
        let mut readings = Readings::default();
        let mut installed = false;
        let mut last_install_check = i64::MIN;
        let mut last_check = i64::MIN;
        let mut backoff_until = i64::MIN;
        let mut seen_open = opened.load(Ordering::Relaxed);
        let mut first = true;
        loop {
            let now = SystemTime::now();
            let secs = unix_secs(now);
            if secs.saturating_sub(last_install_check) >= INSTALL_CHECK_SECS {
                last_install_check = secs;
                installed = source.installed();
                if !installed {
                    last_check = i64::MIN;
                    *shared.lock().unwrap() = None;
                }
            }
            let open_count = opened.load(Ordering::Relaxed);
            let was_opened = open_count != seen_open;
            seen_open = open_count;
            if installed && check_due(secs, last_check, was_opened, secs < backoff_until) {
                last_check = secs;
                let result = source.fetch(now);
                if result == Err(Failure::TooManyChecks) {
                    backoff_until = secs + BACKOFF_SECS;
                }
                readings.apply(result);
                *shared.lock().unwrap() = Some(readings.usage(&info));
            }
            if std::mem::take(&mut first) {
                first_done.fetch_add(1, Ordering::Relaxed);
            }
            thread::sleep(TICK);
        }
    });
    latest
}

/// The plan name as a provider sends it, made readable: `pro_plus` becomes `Pro plus`.
pub fn plan_label(raw: &str) -> Option<String> {
    let words = raw.trim().replace(['_', '-'], " ");
    let mut chars = words.chars();
    let first = chars.next()?;
    Some(first.to_uppercase().chain(chars).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::UsageStatus;

    const INFO: ProviderInfo = ProviderInfo {
        id: "x",
        name: "X",
        chats: false,
    };

    #[test]
    fn checks_regularly_and_when_opened_unless_backing_off() {
        assert!(check_due(1000, i64::MIN, false, false));
        assert!(!check_due(
            1000 + LIVE_REFRESH_AFTER_SECS - 1,
            1000,
            false,
            false
        ));
        assert!(check_due(
            1000 + LIVE_REFRESH_AFTER_SECS,
            1000,
            false,
            false
        ));
        assert!(!check_due(1003, 1000, true, false));
        assert!(check_due(1000 + OPEN_GAP_SECS, 1000, true, false));
        assert!(!check_due(1100, 1000, true, true));
    }

    #[test]
    fn a_failed_check_keeps_the_last_reading_but_a_lost_login_does_not() {
        let mut readings = Readings::default();
        assert_eq!(
            readings.usage(&INFO).note.as_deref(),
            Some("Checking X usage.")
        );
        let mut good = ProviderUsage::unknown("x", "X", "");
        good.status = UsageStatus::Available;
        good.note = None;
        readings.apply(Ok(good));
        readings.apply(Err(Failure::Request("Offline.".into())));
        let usage = readings.usage(&INFO);
        assert_eq!(usage.status, UsageStatus::Available);
        assert_eq!(usage.note.as_deref(), Some("Offline."));
        readings.apply(Err(Failure::NoLogin("Sign in.".into())));
        let usage = readings.usage(&INFO);
        assert_eq!(usage.status, UsageStatus::Unknown);
        assert_eq!(usage.note.as_deref(), Some("Sign in."));
    }

    #[test]
    fn plan_names_read_like_words() {
        assert_eq!(plan_label("pro_plus").as_deref(), Some("Pro plus"));
        assert_eq!(plan_label("individual").as_deref(), Some("Individual"));
        assert_eq!(plan_label(" "), None);
    }
}
