//! Desktop alerts for chats near their context limit and usage nearly gone.
//!
//! Each alert fires once. A chat alerts again only after its context drops back
//! below the threshold or it is picked up again after a quiet spell, and a usage
//! window only after it resets. What was sent is saved, so a restart doesn't repeat it.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::model::{
    CRITICAL_PERCENT, ChatContext, LimitWindow, RESET_JITTER_SECS, Snapshot, TRAY_CHAT_SECS,
    same_reset, unix_secs,
};

#[derive(Debug, PartialEq)]
pub struct Alert {
    pub title: String,
    pub body: String,
}

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
struct Sent {
    chats: HashSet<String>,
    /// Provider, window label, and reset time, so a new window can alert again.
    windows: Vec<(String, String, Option<i64>)>,
}

#[derive(Default)]
pub struct Alerts {
    path: Option<PathBuf>,
    sent: Sent,
}

impl Alerts {
    pub fn load(path: PathBuf) -> Self {
        let sent = fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Alerts {
            path: Some(path),
            sent,
        }
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_vec(&self.sent) {
            let _ = fs::write(path, json);
        }
    }

    pub fn check(&mut self, snapshot: &Snapshot, now: SystemTime) -> Vec<Alert> {
        let before = self.sent.clone();
        let alerts = self.find(snapshot, now);
        // Windows that reset long ago can't come back, so they needn't be kept.
        let cutoff = unix_secs(now) - RESET_JITTER_SECS;
        self.sent
            .windows
            .retain(|(_, _, r)| r.is_none_or(|r| r > cutoff));
        // Compared as sets, since the saved order of chats changes from run to run.
        if self.sent != before {
            self.save();
        }
        alerts
    }

    fn find(&mut self, snapshot: &Snapshot, now: SystemTime) -> Vec<Alert> {
        let mut alerts = Vec::new();

        let mut full = HashSet::new();
        for chat in &snapshot.chats {
            let Some(used) = chat.context.as_ref().and_then(|c| c.used_percent) else {
                continue;
            };
            // Old chats stay quiet until they are used again, like the tray.
            let recent = unix_secs(now) - chat.last_activity <= TRAY_CHAT_SECS;
            if used < CRITICAL_PERCENT || !recent {
                continue;
            }
            let key = format!("{}:{}", chat.provider, chat.id);
            if !self.sent.chats.contains(&key) {
                let limited = chat.context.as_ref().is_some_and(|c| c.limited);
                let of = if limited {
                    "your context limit"
                } else {
                    "its context"
                };
                alerts.push(Alert {
                    title: format!("{} is almost out of context", chat_name(chat)),
                    body: format!(
                        "It has used {used:.0}% of {of}. Consider clearing or compacting it."
                    ),
                });
            }
            full.insert(key);
        }
        self.sent.chats = full;

        for usage in snapshot.usage.iter().filter(|u| !u.stale) {
            for (label, window) in [("session", &usage.session), ("weekly", &usage.weekly)] {
                let LimitWindow::Reported {
                    used_percent,
                    resets_at,
                } = window
                else {
                    continue;
                };
                if *used_percent < CRITICAL_PERCENT {
                    // With no reset time to tell windows apart, dropping back below
                    // the threshold is the sign of a new one.
                    if resets_at.is_none() {
                        self.sent.windows.retain(|(p, l, r)| {
                            !(p == usage.provider && l == label && r.is_none())
                        });
                    }
                    continue;
                }
                // Reset times jitter between readings, so a near match is the same window.
                let seen = self.sent.windows.iter().any(|(p, l, r)| {
                    p == usage.provider && l == label && same_reset(*r, *resets_at)
                });
                if !seen {
                    self.sent.windows.push((
                        usage.provider.to_string(),
                        label.to_string(),
                        *resets_at,
                    ));
                    alerts.push(Alert {
                        title: format!("{} {label} usage is almost gone", usage.name),
                        body: format!("{:.0}% left.", (100.0 - used_percent).max(0.0)),
                    });
                }
            }
        }
        alerts
    }
}

fn chat_name(chat: &ChatContext) -> String {
    chat.title
        .clone()
        .or_else(|| chat.project.clone())
        .unwrap_or_else(|| format!("A {} chat", chat.provider))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Confidence, ContextMeasure, ProviderUsage};
    use std::time::{Duration, UNIX_EPOCH};

    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_900_000_000)
    }

    fn chat(used: f64, age: i64) -> ChatContext {
        ChatContext {
            provider: "claude",
            id: "a".into(),
            title: Some("Fix login".into()),
            project: None,
            cwd: None,
            model: None,
            context: Some(ContextMeasure {
                used_tokens: 0,
                context_window: Some(300_000),
                used_percent: Some(used),
                confidence: Confidence::Estimated,
                observed_at: String::new(),
                limited: true,
            }),
            last_activity: unix_secs(now()) - age,
            subagents: 0,
            activity: None,
            surface: None,
        }
    }

    fn snapshot(usage: Vec<ProviderUsage>, chats: Vec<ChatContext>) -> Snapshot {
        Snapshot::new(usage, chats, now())
    }

    #[test]
    fn a_full_chat_alerts_once_until_it_is_cleared() {
        let mut alerts = Alerts::default();
        assert!(
            alerts
                .check(&snapshot(vec![], vec![chat(80.0, 10)]), now())
                .is_empty()
        );
        let fired = alerts.check(&snapshot(vec![], vec![chat(92.0, 10)]), now());
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].title, "Fix login is almost out of context");
        assert!(fired[0].body.contains("your context limit"));
        assert!(
            alerts
                .check(&snapshot(vec![], vec![chat(95.0, 10)]), now())
                .is_empty()
        );
        alerts.check(&snapshot(vec![], vec![chat(10.0, 10)]), now());
        assert_eq!(
            alerts
                .check(&snapshot(vec![], vec![chat(91.0, 10)]), now())
                .len(),
            1
        );
    }

    #[test]
    fn a_restart_does_not_repeat_alerts() {
        let dir = std::env::temp_dir().join(format!("acm-alerts-{}", std::process::id()));
        let path = dir.join("alerts-sent.json");
        let full = || snapshot(vec![], vec![chat(95.0, 10)]);
        assert_eq!(Alerts::load(path.clone()).check(&full(), now()).len(), 1);
        assert!(Alerts::load(path.clone()).check(&full(), now()).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn old_chats_do_not_alert() {
        let mut alerts = Alerts::default();
        assert!(
            alerts
                .check(&snapshot(vec![], vec![chat(99.0, 2 * 60 * 60)]), now())
                .is_empty()
        );
    }

    #[test]
    fn usage_alerts_once_per_window() {
        let usage = |used, resets_at, stale| {
            let mut u = ProviderUsage::unknown("codex", "Codex", "");
            u.weekly = LimitWindow::Reported {
                used_percent: used,
                // Offsets from a reset a day from now.
                resets_at: Some(unix_secs(now()) + 86_400 + resets_at),
            };
            u.stale = stale;
            u
        };
        let mut alerts = Alerts::default();
        assert!(
            alerts
                .check(&snapshot(vec![usage(95.0, 1, true)], vec![]), now())
                .is_empty()
        );
        let fired = alerts.check(&snapshot(vec![usage(95.0, 1, false)], vec![]), now());
        assert_eq!(
            fired,
            vec![Alert {
                title: "Codex weekly usage is almost gone".into(),
                body: "5% left.".into(),
            }]
        );
        assert!(
            alerts
                .check(&snapshot(vec![usage(97.0, 1, false)], vec![]), now())
                .is_empty()
        );
        // A reset time that moved by a few seconds is still the same window.
        assert!(
            alerts
                .check(&snapshot(vec![usage(96.0, 3, false)], vec![]), now())
                .is_empty()
        );
        assert_eq!(
            alerts
                .check(
                    &snapshot(vec![usage(91.0, 1 + 7 * 86_400, false)], vec![]),
                    now()
                )
                .len(),
            1
        );
    }

    #[test]
    fn a_window_without_a_reset_time_alerts_again_after_dropping() {
        let usage = |used| {
            let mut u = ProviderUsage::unknown("codex", "Codex", "");
            u.weekly = LimitWindow::Reported {
                used_percent: used,
                resets_at: None,
            };
            u
        };
        let mut alerts = Alerts::default();
        let mut check = |used| {
            alerts
                .check(&snapshot(vec![usage(used)], vec![]), now())
                .len()
        };
        assert_eq!(check(95.0), 1);
        assert_eq!(check(96.0), 0);
        assert_eq!(check(10.0), 0);
        assert_eq!(check(92.0), 1);
    }
}
