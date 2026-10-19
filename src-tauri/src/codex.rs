//! Reads Codex usage and per-chat context from `~/.codex/sessions`.
//!
//! Only allowlisted metadata and usage fields are deserialized; message content is
//! skipped by serde and never stored.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::Deserialize;

use crate::jsonl::{collect_jsonl, consume_lines, contains, project_label, read_from};

use crate::model::{
    ChatContext, Confidence, ContextMeasure, LimitWindow, ProviderUsage, UsageStatus, unix_secs,
};

const PROVIDER: &str = "codex";
const NAME: &str = "Codex";
/// Files untouched for longer than a weekly window cannot hold the newest usage reading.
const LOOKBACK: Duration = Duration::from_secs(8 * 24 * 60 * 60);
pub const ACTIVE_WINDOW: Duration = Duration::from_secs(30 * 60);
/// Windows up to a day long are the rolling session limit; longer ones are weekly.
const SESSION_WINDOW_MAX_MINUTES: u64 = 24 * 60;

#[derive(Debug, Clone, PartialEq)]
struct RateReading {
    observed_at: String,
    plan: Option<String>,
    windows: Vec<RateWindow>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
struct RateWindow {
    used_percent: f64,
    window_minutes: Option<u64>,
    resets_at: Option<i64>,
}

#[derive(Debug, Default)]
struct SessionFile {
    offset: u64,
    modified: Option<SystemTime>,
    id: Option<String>,
    cwd: Option<String>,
    parent_id: Option<String>,
    model: Option<String>,
    context: Option<ContextMeasure>,
    rate: Option<RateReading>,
}

#[derive(Deserialize)]
struct Envelope<P> {
    timestamp: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    payload: P,
}

#[derive(Deserialize)]
struct MetaPayload {
    id: Option<String>,
    cwd: Option<String>,
    thread_source: Option<String>,
    source: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct TurnPayload {
    model: Option<String>,
    cwd: Option<String>,
}

#[derive(Deserialize)]
struct TokenPayload {
    #[serde(rename = "type")]
    kind: String,
    info: Option<TokenInfo>,
    rate_limits: Option<RateLimits>,
}

#[derive(Deserialize)]
struct TokenInfo {
    last_token_usage: Option<TokenUsage>,
    model_context_window: Option<u64>,
}

#[derive(Deserialize)]
struct TokenUsage {
    input_tokens: u64,
    output_tokens: u64,
    total_tokens: u64,
}

#[derive(Deserialize)]
struct RateLimits {
    primary: Option<RateWindow>,
    secondary: Option<RateWindow>,
    plan_type: Option<String>,
}

#[derive(Deserialize)]
struct IndexEntry {
    id: String,
    thread_name: Option<String>,
}

pub struct CodexSource {
    sessions_dir: PathBuf,
    index_path: PathBuf,
    files: HashMap<PathBuf, SessionFile>,
    titles: HashMap<String, String>,
    index_offset: u64,
}

impl CodexSource {
    pub fn new(codex_home: PathBuf) -> Self {
        CodexSource {
            sessions_dir: codex_home.join("sessions"),
            index_path: codex_home.join("session_index.jsonl"),
            files: HashMap::new(),
            titles: HashMap::new(),
            index_offset: 0,
        }
    }

    /// Reads only bytes appended since the last refresh.
    pub fn refresh(&mut self, now: SystemTime) {
        self.refresh_titles();
        let mut seen = Vec::new();
        collect_jsonl(&self.sessions_dir, &mut seen);
        let mut live = HashMap::with_capacity(seen.len());
        for (path, modified, len) in seen {
            if now.duration_since(modified).unwrap_or_default() > LOOKBACK {
                continue;
            }
            let mut state = self.files.remove(&path).unwrap_or_default();
            if len < state.offset {
                state = SessionFile::default();
            }
            if len > state.offset
                && let Ok(chunk) = read_from(&path, state.offset)
            {
                state.offset += consume_lines(&chunk, |line| state.apply(line));
            }
            state.modified = Some(modified);
            live.insert(path, state);
        }
        self.files = live;
    }

    fn refresh_titles(&mut self) {
        let Ok(meta) = fs::metadata(&self.index_path) else {
            return;
        };
        if meta.len() < self.index_offset {
            self.index_offset = 0;
            self.titles.clear();
        }
        if meta.len() == self.index_offset {
            return;
        }
        if let Ok(chunk) = read_from(&self.index_path, self.index_offset) {
            let titles = &mut self.titles;
            self.index_offset += consume_lines(&chunk, |line| {
                if let Ok(entry) = serde_json::from_slice::<IndexEntry>(line)
                    && let Some(name) = entry.thread_name.filter(|n| !n.trim().is_empty())
                {
                    titles.insert(entry.id, name);
                }
            });
        }
    }

    pub fn usage(&self, now: SystemTime) -> ProviderUsage {
        let newest = self
            .files
            .values()
            .filter_map(|f| f.rate.as_ref())
            .max_by(|a, b| a.observed_at.cmp(&b.observed_at));
        match newest {
            None => ProviderUsage::unknown(
                PROVIDER,
                NAME,
                "No Codex usage in the last week. It appears after you use Codex.",
            ),
            Some(reading) => usage_from_reading(reading, unix_secs(now)),
        }
    }

    pub fn chats(&self, now: SystemTime) -> Vec<ChatContext> {
        let is_active = |f: &SessionFile| {
            f.modified
                .is_some_and(|m| now.duration_since(m).unwrap_or_default() <= ACTIVE_WINDOW)
        };
        let mut subagents: HashMap<&str, usize> = HashMap::new();
        for f in self.files.values().filter(|f| is_active(f)) {
            if let Some(parent) = f.parent_id.as_deref() {
                *subagents.entry(parent).or_default() += 1;
            }
        }
        self.files
            .values()
            .filter(|f| f.parent_id.is_none() && is_active(f))
            .filter_map(|f| {
                let id = f.id.clone()?;
                Some(ChatContext {
                    provider: PROVIDER,
                    title: self.titles.get(&id).cloned(),
                    subagents: subagents.get(id.as_str()).copied().unwrap_or(0),
                    activity: None,
                    project: f.cwd.as_deref().and_then(project_label),
                    cwd: f.cwd.clone(),
                    model: f.model.clone(),
                    context: f.context.clone(),
                    last_activity: f.modified.map(unix_secs).unwrap_or(0),
                    id,
                })
            })
            .collect()
    }
}

impl SessionFile {
    fn apply(&mut self, line: &[u8]) {
        // Cheap prefilter: most lines are content records this reader never needs.
        if contains(line, b"\"session_meta\"") {
            if let Ok(rec) = serde_json::from_slice::<Envelope<MetaPayload>>(line)
                && rec.kind == "session_meta"
            {
                let p = rec.payload;
                self.id = p.id.or(self.id.take());
                self.cwd = p.cwd.or(self.cwd.take());
                if p.thread_source.as_deref() == Some("subagent") {
                    self.parent_id = p.source.as_ref().and_then(parent_thread_id);
                    // Keep subagents out of the root list even without a parent link.
                    self.parent_id.get_or_insert_with(String::new);
                }
            }
        } else if contains(line, b"\"turn_context\"") {
            if let Ok(rec) = serde_json::from_slice::<Envelope<TurnPayload>>(line)
                && rec.kind == "turn_context"
            {
                self.model = rec.payload.model.or(self.model.take());
                self.cwd = rec.payload.cwd.or(self.cwd.take());
            }
        } else if contains(line, b"\"token_count\"")
            && let Ok(rec) = serde_json::from_slice::<Envelope<TokenPayload>>(line)
            && rec.kind == "event_msg"
            && rec.payload.kind == "token_count"
        {
            self.apply_token_count(rec.timestamp.unwrap_or_default(), rec.payload);
        }
    }

    fn apply_token_count(&mut self, observed_at: String, payload: TokenPayload) {
        if let Some(info) = payload.info
            && let (Some(usage), Some(window)) = (info.last_token_usage, info.model_context_window)
            && window > 0
        {
            // Totals that do not add up are provider estimates, e.g. after compaction.
            let confidence = if usage.total_tokens == usage.input_tokens + usage.output_tokens {
                Confidence::Confirmed
            } else {
                Confidence::Estimated
            };
            self.context = Some(ContextMeasure {
                used_tokens: usage.total_tokens,
                context_window: Some(window),
                used_percent: Some((usage.total_tokens as f64 / window as f64 * 100.0).min(100.0)),
                confidence,
                observed_at: observed_at.clone(),
            });
        }
        if let Some(limits) = payload.rate_limits {
            self.rate = Some(RateReading {
                observed_at,
                plan: limits.plan_type,
                windows: [limits.primary, limits.secondary]
                    .into_iter()
                    .flatten()
                    .collect(),
            });
        }
    }
}

/// Classifies windows by length, never by primary/secondary position.
fn usage_from_reading(reading: &RateReading, now: i64) -> ProviderUsage {
    let pick = |is_session: bool| {
        reading
            .windows
            .iter()
            .find(|w| {
                w.window_minutes
                    .is_some_and(|m| (m <= SESSION_WINDOW_MAX_MINUTES) == is_session)
            })
            .map(|w| match w.resets_at {
                Some(reset) if reset <= now => LimitWindow::ResetSinceReading { resets_at: reset },
                resets_at => LimitWindow::Reported {
                    used_percent: w.used_percent.clamp(0.0, 100.0),
                    resets_at,
                },
            })
            .unwrap_or(LimitWindow::NotReported)
    };
    let session = pick(true);
    let weekly = pick(false);
    let empty = session == LimitWindow::NotReported && weekly == LimitWindow::NotReported;
    ProviderUsage {
        provider: PROVIDER,
        name: NAME,
        status: if empty {
            UsageStatus::Unknown
        } else {
            UsageStatus::Available
        },
        note: empty.then(|| "Codex's latest reading had no limits.".to_string()),
        plan: reading.plan.clone(),
        observed_at: Some(reading.observed_at.clone()),
        session,
        weekly,
    }
}

fn parent_thread_id(source: &serde_json::Value) -> Option<String> {
    source
        .pointer("/subagent/thread_spawn/parent_thread_id")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SENTINEL: &str = "SENTINEL-PRIVATE-PROMPT-7731";

    fn meta(id: &str, extra: &str) -> String {
        format!(
            r#"{{"timestamp":"2026-10-19T10:00:00Z","type":"session_meta","payload":{{"id":"{id}","cwd":"/home/u/Code/demo","base_instructions":{{"text":"{SENTINEL}"}}{extra}}}}}"#
        )
    }

    fn token(ts: &str, total: u64, input: u64, output: u64, limits: &str) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":{input},"output_tokens":{output},"total_tokens":{total}}},"model_context_window":100000}},"rate_limits":{limits}}}}}"#
        )
    }

    fn file_from(lines: &[String]) -> SessionFile {
        let mut f = SessionFile::default();
        let text = lines.join("\n") + "\n";
        f.offset = consume_lines(text.as_bytes(), |l| f.apply(l));
        f
    }

    #[test]
    fn weekly_only_pro_plan_is_classified_by_length() {
        let limits = r#"{"primary":{"used_percent":13.0,"window_minutes":10080,"resets_at":1792880374},"secondary":null,"plan_type":"pro"}"#;
        let f = file_from(&[token("2026-10-19T10:51:09Z", 10, 8, 2, limits)]);
        let usage = usage_from_reading(f.rate.as_ref().unwrap(), 1_792_000_000);
        assert_eq!(usage.session, LimitWindow::NotReported);
        assert_eq!(
            usage.weekly,
            LimitWindow::Reported {
                used_percent: 13.0,
                resets_at: Some(1792880374)
            }
        );
        assert_eq!(usage.plan.as_deref(), Some("pro"));
    }

    #[test]
    fn five_hour_and_weekly_windows_map_to_session_and_week() {
        let limits = r#"{"primary":{"used_percent":40.0,"window_minutes":300,"resets_at":2000000000},"secondary":{"used_percent":75.0,"window_minutes":10080,"resets_at":2000000000}}"#;
        let f = file_from(&[token("2026-07-01T00:00:00Z", 10, 8, 2, limits)]);
        let usage = usage_from_reading(f.rate.as_ref().unwrap(), 1_900_000_000);
        assert!(
            matches!(usage.session, LimitWindow::Reported { used_percent, .. } if used_percent == 40.0)
        );
        assert!(
            matches!(usage.weekly, LimitWindow::Reported { used_percent, .. } if used_percent == 75.0)
        );
    }

    #[test]
    fn passed_reset_hides_old_percentage() {
        let limits = r#"{"primary":{"used_percent":80.0,"window_minutes":10080,"resets_at":1000}}"#;
        let f = file_from(&[token("2026-07-01T00:00:00Z", 10, 8, 2, limits)]);
        let usage = usage_from_reading(f.rate.as_ref().unwrap(), 2000);
        assert_eq!(
            usage.weekly,
            LimitWindow::ResetSinceReading { resets_at: 1000 }
        );
    }

    #[test]
    fn null_windows_are_unknown_not_zero() {
        let limits = r#"{"primary":null,"secondary":null}"#;
        let f = file_from(&[token("2026-10-14T00:00:00Z", 10, 8, 2, limits)]);
        let usage = usage_from_reading(f.rate.as_ref().unwrap(), 0);
        assert_eq!(usage.status, UsageStatus::Unknown);
    }

    #[test]
    fn context_uses_last_usage_and_flags_estimates() {
        let f = file_from(&[meta("abc", ""), token("t1", 25_000, 24_000, 1_000, "null")]);
        let ctx = f.context.unwrap();
        assert_eq!(ctx.used_tokens, 25_000);
        assert_eq!(ctx.used_percent, Some(25.0));
        assert_eq!(ctx.confidence, Confidence::Confirmed);

        let f = file_from(&[meta("abc", ""), token("t2", 9_000, 30_000, 1_000, "null")]);
        assert_eq!(f.context.unwrap().confidence, Confidence::Estimated);
    }

    #[test]
    fn partial_trailing_line_waits_for_the_rest() {
        let full = token("t1", 10, 8, 2, "null");
        let (head, tail) = full.split_at(full.len() / 2);
        let mut f = SessionFile::default();
        assert_eq!(consume_lines(head.as_bytes(), |l| f.apply(l)), 0);
        let rest = format!("{head}{tail}\n{{broken json\n");
        consume_lines(rest.as_bytes(), |l| f.apply(l));
        assert!(f.context.is_some());
    }

    #[test]
    fn subagents_are_not_root_rows_and_content_is_not_kept() {
        let child = meta(
            "child",
            r#","thread_source":"subagent","source":{"subagent":{"thread_spawn":{"parent_thread_id":"root"}}}"#,
        );
        let f = file_from(&[child]);
        assert_eq!(f.parent_id.as_deref(), Some("root"));
        let root = file_from(&[meta("root", "")]);
        assert!(!format!("{root:?}{f:?}").contains(SENTINEL));
    }
}
