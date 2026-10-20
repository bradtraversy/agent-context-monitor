use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

pub const WARNING_PERCENT: f64 = 70.0;
pub const CRITICAL_PERCENT: f64 = 90.0;

/// Ordered so the most urgent level is the maximum.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    #[default]
    Unknown,
    Ok,
    Warning,
    Critical,
}

impl Level {
    pub fn from_used_percent(used: f64) -> Level {
        if used >= CRITICAL_PERCENT {
            Level::Critical
        } else if used >= WARNING_PERCENT {
            Level::Warning
        } else {
            Level::Ok
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LimitWindow {
    Reported {
        used_percent: f64,
        resets_at: Option<i64>,
    },
    /// The reset time passed after the last reading, so the current value is unknown.
    ResetSinceReading {
        resets_at: i64,
    },
    NotReported,
}

impl LimitWindow {
    pub fn level(&self) -> Level {
        match self {
            LimitWindow::Reported { used_percent, .. } => Level::from_used_percent(*used_percent),
            _ => Level::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageStatus {
    Available,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ProviderUsage {
    pub provider: &'static str,
    pub name: &'static str,
    pub status: UsageStatus,
    pub note: Option<String>,
    pub plan: Option<String>,
    /// RFC 3339 time of the reading, as reported by the provider.
    pub observed_at: Option<String>,
    pub session: LimitWindow,
    pub weekly: LimitWindow,
    /// The reading is too old to trust as the current value.
    pub stale: bool,
}

/// Every provider refreshes usage every few minutes, so anything older missed updates.
pub const USAGE_STALE_AFTER_SECS: i64 = 15 * 60;

impl ProviderUsage {
    pub fn unknown(provider: &'static str, name: &'static str, note: impl Into<String>) -> Self {
        ProviderUsage {
            provider,
            name,
            status: UsageStatus::Unknown,
            note: Some(note.into()),
            plan: None,
            observed_at: None,
            session: LimitWindow::NotReported,
            weekly: LimitWindow::NotReported,
            stale: false,
        }
    }

    pub fn with_staleness(mut self, now: SystemTime) -> Self {
        let observed = self
            .observed_at
            .as_deref()
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.timestamp());
        self.stale = observed.is_some_and(|t| unix_secs(now) - t > USAGE_STALE_AFTER_SECS);
        self
    }

    pub fn level(&self) -> Level {
        if self.stale {
            return Level::Unknown;
        }
        self.session.level().max(self.weekly.level())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Confirmed,
    /// Provider-side estimate, for example right after compaction.
    Estimated,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ContextMeasure {
    pub used_tokens: u64,
    pub context_window: Option<u64>,
    pub used_percent: Option<f64>,
    pub confidence: Confidence,
    pub observed_at: String,
}

impl ContextMeasure {
    pub fn level(&self) -> Level {
        self.used_percent
            .map(Level::from_used_percent)
            .unwrap_or_default()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ChatContext {
    pub provider: &'static str,
    pub id: String,
    pub title: Option<String>,
    pub project: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub context: Option<ContextMeasure>,
    /// Unix seconds of the last write to the session file.
    pub last_activity: i64,
    pub subagents: usize,
    pub activity: Option<Activity>,
    /// Where the chat runs, such as the terminal or the desktop app.
    pub surface: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Activity {
    Working,
    Idle,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Snapshot {
    pub usage: Vec<ProviderUsage>,
    pub chats: Vec<ChatContext>,
    pub level: Level,
}

impl Snapshot {
    pub fn new(usage: Vec<ProviderUsage>, chats: Vec<ChatContext>) -> Self {
        let level = usage
            .iter()
            .map(ProviderUsage::level)
            .chain(
                chats
                    .iter()
                    .filter_map(|c| c.context.as_ref().map(ContextMeasure::level)),
            )
            .max()
            .unwrap_or_default();
        Snapshot {
            usage,
            chats,
            level,
        }
    }

    pub fn tooltip(&self) -> String {
        let mut lines = vec!["Agent Context Monitor".to_string()];
        for u in &self.usage {
            let part = |label: &str, w: &LimitWindow| match w {
                LimitWindow::Reported { used_percent, .. } => Some(format!(
                    "{label} {:.0}% left",
                    (100.0 - used_percent).max(0.0)
                )),
                _ => None,
            };
            let parts: Vec<String> = [part("session", &u.session), part("week", &u.weekly)]
                .into_iter()
                .flatten()
                .collect();
            let parts = if u.stale { Vec::new() } else { parts };
            let summary = if parts.is_empty() {
                "unknown".to_string()
            } else {
                parts.join(", ")
            };
            lines.push(format!("{}: {summary}", u.name));
        }
        lines.join("\n")
    }
}

pub fn unix_secs(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn old_readings_are_stale_and_do_not_color_the_tray() {
        let mut usage = ProviderUsage::unknown("x", "X", "");
        usage.observed_at = Some("2026-10-19T13:03:28.563Z".into());
        usage.weekly = LimitWindow::Reported {
            used_percent: 95.0,
            resets_at: None,
        };
        let observed = 1_792_415_008; // 2026-10-19T13:03:28Z
        let at = |secs| UNIX_EPOCH + Duration::from_secs(secs);
        let fresh = usage.clone().with_staleness(at(observed + 60));
        assert!(!fresh.stale);
        assert_eq!(fresh.level(), Level::Critical);
        let old = usage.with_staleness(at(observed + 21 * 60 * 60));
        assert!(old.stale);
        assert_eq!(old.level(), Level::Unknown);
    }
}
