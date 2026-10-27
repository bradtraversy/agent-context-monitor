//! Lists recent Claude Code chats and their context from local files.
//!
//! Claude Code registers each running session in `~/.claude/sessions/<pid>.json`.
//! Closed chats are found from transcripts written within the recent window.
//! Context comes from the last assistant usage in the session transcript. Transcripts
//! do not record the context window, so it is estimated from the model name, and
//! only the token count is shown for models that aren't Claude.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;

#[cfg(test)]
use crate::jsonl::consume_lines;
use crate::jsonl::{contains, project_label, read_lines_from, rescan_due};
use crate::model::{
    Activity, ChatContext, Confidence, ContextMeasure, RECENT_CHAT_SECS, unix_secs,
};

const PROVIDER: &str = "claude";
const ONE_MILLION: u64 = 1_000_000;
const TWO_HUNDRED_K: u64 = 200_000;
/// Used when liveness cannot be checked against the process table.
#[cfg(not(target_os = "linux"))]
const REGISTRY_FRESHNESS_SECS: i64 = 30 * 60;

#[derive(Deserialize)]
struct RegistryEntry {
    pid: u32,
    #[serde(rename = "sessionId")]
    session_id: String,
    cwd: Option<String>,
    #[serde(rename = "procStart")]
    proc_start: Option<String>,
    name: Option<String>,
    /// "derived" names are auto-generated from the folder, like `brad-b4`.
    #[serde(rename = "nameSource")]
    name_source: Option<String>,
    entrypoint: Option<String>,
    status: Option<String>,
    #[serde(rename = "updatedAt")]
    updated_at_ms: Option<i64>,
}

#[derive(Deserialize)]
struct Record {
    #[serde(rename = "type")]
    kind: String,
    subtype: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "isSidechain", default)]
    is_sidechain: bool,
    #[serde(rename = "requestedModel")]
    requested_model: Option<String>,
    message: Option<Message>,
    cwd: Option<String>,
    entrypoint: Option<String>,
    #[serde(rename = "customTitle")]
    custom_title: Option<String>,
    #[serde(rename = "aiTitle")]
    ai_title: Option<String>,
}

#[derive(Deserialize)]
struct Message {
    model: Option<String>,
    usage: Option<Usage>,
}

#[derive(Deserialize, Clone, Copy)]
struct Counts {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

impl Counts {
    fn context_tokens(self) -> u64 {
        self.input_tokens + self.cache_creation_input_tokens + self.cache_read_input_tokens
    }
}

#[derive(Deserialize)]
struct Usage {
    #[serde(flatten)]
    totals: Counts,
    #[serde(default)]
    iterations: Vec<Iteration>,
}

#[derive(Deserialize)]
struct Iteration {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(flatten)]
    counts: Counts,
}

#[derive(Default)]
struct Transcript {
    path: Option<PathBuf>,
    offset: u64,
    modified: Option<SystemTime>,
    model: Option<String>,
    context: Option<ContextMeasure>,
    cwd: Option<String>,
    entrypoint: Option<String>,
    custom_title: Option<String>,
    ai_title: Option<String>,
}

pub struct ClaudeSessions {
    registry_dir: PathBuf,
    projects_dir: PathBuf,
    transcripts: HashMap<String, Transcript>,
    /// Recent closed chats from the last folder search.
    recent: Vec<(String, PathBuf)>,
    last_scan: Option<SystemTime>,
}

impl ClaudeSessions {
    pub fn new(claude_home: PathBuf) -> Self {
        ClaudeSessions {
            registry_dir: claude_home.join("sessions"),
            projects_dir: claude_home.join("projects"),
            transcripts: HashMap::new(),
            recent: Vec::new(),
            last_scan: None,
        }
    }

    pub fn chats(&mut self, now: SystemTime) -> Vec<ChatContext> {
        // Every project folder holds every chat ever, so it is searched only now and then.
        let rescan = rescan_due(self.last_scan, now);
        if rescan {
            self.last_scan = Some(now);
            self.recent = recent_transcripts(&self.projects_dir, now);
        }
        let live = live_sessions(&self.registry_dir, now);
        let mut transcripts = HashMap::with_capacity(live.len());
        let mut chats = Vec::with_capacity(live.len());
        let live_ids: Vec<String> = live.iter().map(|e| e.session_id.clone()).collect();
        for entry in live {
            let mut transcript = self
                .transcripts
                .remove(&entry.session_id)
                .unwrap_or_default();
            if transcript.path.is_none() {
                transcript.path = transcript_path(&self.projects_dir, &entry, rescan);
            }
            if let Some(path) = transcript.path.clone()
                && !transcript.read_new(&path)
            {
                transcript.path = None;
            }
            chats.push(ChatContext {
                provider: PROVIDER,
                title: entry
                    .name
                    .clone()
                    .filter(|n| {
                        !n.trim().is_empty() && entry.name_source.as_deref() != Some("derived")
                    })
                    .or_else(|| transcript.title()),
                surface: entry.entrypoint.as_deref().map(surface_label),
                project: entry.cwd.as_deref().and_then(project_label),
                cwd: entry.cwd.clone(),
                model: transcript.model.clone(),
                context: transcript.context.clone(),
                last_activity: transcript
                    .modified
                    .map(unix_secs)
                    .or(entry.updated_at_ms.map(|ms| ms / 1000))
                    .unwrap_or(0),
                subagents: 0,
                activity: match entry.status.as_deref() {
                    Some("busy") => Some(Activity::Working),
                    Some("idle") => Some(Activity::Idle),
                    _ => None,
                },
                id: entry.session_id.clone(),
            });
            transcripts.insert(entry.session_id, transcript);
        }
        for (id, path) in &self.recent {
            if live_ids.contains(id) {
                continue;
            }
            let id = id.clone();
            let mut transcript = self.transcripts.remove(&id).unwrap_or_default();
            transcript.read_new(path);
            chats.push(ChatContext {
                provider: PROVIDER,
                title: transcript.title(),
                surface: transcript.entrypoint.as_deref().map(surface_label),
                project: transcript.cwd.as_deref().and_then(project_label),
                cwd: transcript.cwd.clone(),
                model: transcript.model.clone(),
                context: transcript.context.clone(),
                last_activity: transcript.modified.map(unix_secs).unwrap_or(0),
                subagents: 0,
                activity: None,
                id: id.clone(),
            });
            transcripts.insert(id, transcript);
        }
        self.transcripts = transcripts;
        drop_superseded(&mut chats);
        chats
    }
}

/// The desktop app starts a new session, with its own transcript, when a chat is
/// reopened, so one chat can leave several transcripts with the same title and folder.
/// Only the open session, or else the newest copy, is kept. Open sessions always stay.
fn drop_superseded(chats: &mut Vec<ChatContext>) {
    let key = |c: &ChatContext| c.title.clone().zip(c.cwd.clone());
    let keep: Vec<bool> = chats
        .iter()
        .enumerate()
        .map(|(i, chat)| {
            let Some(k) = key(chat) else { return true };
            chat.activity.is_some()
                || !chats.iter().enumerate().any(|(j, other)| {
                    j != i
                        && key(other).as_ref() == Some(&k)
                        && (other.activity.is_some()
                            || other.last_activity > chat.last_activity
                            || (other.last_activity == chat.last_activity && j < i))
                })
        })
        .collect();
    let mut keep = keep.into_iter();
    chats.retain(|_| keep.next().unwrap_or(true));
}

/// Top-level transcripts written within the recent window, keyed by session id.
/// Subagent transcripts live one folder deeper and are skipped.
fn recent_transcripts(projects_dir: &Path, now: SystemTime) -> Vec<(String, PathBuf)> {
    let Ok(projects) = fs::read_dir(projects_dir) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for project in projects.flatten() {
        let Ok(files) = fs::read_dir(project.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().is_none_or(|e| e != "jsonl") {
                continue;
            }
            let recent = file
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|m| unix_secs(now) - unix_secs(m) <= RECENT_CHAT_SECS);
            if let (true, Some(id)) = (recent, path.file_stem()) {
                found.push((id.to_string_lossy().into_owned(), path));
            }
        }
    }
    found
}

impl Transcript {
    /// Reads lines added since last time. False if the file is gone.
    fn read_new(&mut self, path: &Path) -> bool {
        let Ok(meta) = fs::metadata(path) else {
            return false;
        };
        if meta.len() < self.offset {
            *self = Transcript {
                path: self.path.take(),
                ..Transcript::default()
            };
        }
        self.modified = meta.modified().ok();
        if meta.len() > self.offset {
            let offset = self.offset;
            self.offset += read_lines_from(path, offset, |line| self.apply(line));
        }
        true
    }

    fn title(&self) -> Option<String> {
        self.custom_title
            .clone()
            .or_else(|| self.ai_title.clone())
            .filter(|t| !t.trim().is_empty())
    }

    fn apply(&mut self, line: &[u8]) {
        // Cheap prefilter: skip user, tool, and attachment records without parsing,
        // apart from the first one that names the folder.
        let wanted = contains(line, b"\"type\":\"assistant\"")
            || contains(line, b"compact_boundary")
            || contains(line, b"-title\"")
            || (self.cwd.is_none() && contains(line, b"\"cwd\""));
        if !wanted {
            return;
        }
        let Ok(rec) = serde_json::from_slice::<Record>(line) else {
            return;
        };
        if self.cwd.is_none() && rec.cwd.is_some() {
            self.cwd = rec.cwd.clone();
            self.entrypoint = rec.entrypoint.clone();
        }
        match rec.kind.as_str() {
            "custom-title" => self.custom_title = rec.custom_title.clone(),
            "ai-title" => self.ai_title = rec.ai_title.clone(),
            _ => {}
        }
        if rec.kind == "system" && rec.subtype.as_deref() == Some("compact_boundary") {
            // The old measurement no longer describes the compacted context.
            self.context = None;
            return;
        }
        if rec.kind != "assistant" || rec.is_sidechain {
            return;
        }
        let Some(message) = rec.message else { return };
        let Some(usage) = message.usage else { return };
        let requested = rec.requested_model.or(message.model);
        let counts = usage
            .iterations
            .iter()
            .rev()
            .find(|i| matches!(i.kind.as_deref(), Some("message" | "fallback_message")))
            .map(|i| i.counts)
            .unwrap_or(usage.totals);
        let used = counts.context_tokens();
        if used == 0 {
            // Zero-filled usage appears around compaction and is not a measurement.
            self.context = None;
        } else {
            self.context = Some(measure(
                used,
                requested.as_deref(),
                rec.timestamp.unwrap_or_default(),
            ));
        }
        if requested.is_some() {
            self.model = requested;
        }
    }
}

fn measure(used: u64, model: Option<&str>, observed_at: String) -> ContextMeasure {
    let window = model.and_then(|m| window_from_model(m, used));
    ContextMeasure {
        used_tokens: used,
        context_window: window,
        used_percent: window.map(|w| (used as f64 / w as f64 * 100.0).min(100.0)),
        // Window derived from the model name, not reported by Claude itself.
        confidence: Confidence::Estimated,
        observed_at,
        limited: false,
    }
}

/// Claude Code marks a model running with the 1M window with a `[1m]` suffix, as in
/// `claude-opus-5-5[1m]`. Other Claude models use the standard 200k window, unless
/// the chat has already gone past it. Models that aren't Claude stay unknown.
fn window_from_model(model: &str, used: u64) -> Option<u64> {
    if model.ends_with("[1m]") || used > TWO_HUNDRED_K {
        return Some(ONE_MILLION);
    }
    let model = model.to_ascii_lowercase();
    ["claude", "opus", "sonnet", "haiku"]
        .iter()
        .any(|family| model.contains(family))
        .then_some(TWO_HUNDRED_K)
}

fn surface_label(entrypoint: &str) -> String {
    match entrypoint {
        "cli" => "Terminal".into(),
        "claude-desktop" => "Desktop app".into(),
        other => other.replace(['-', '_'], " "),
    }
}

fn live_sessions(dir: &Path, now: SystemTime) -> Vec<RegistryEntry> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut live: Vec<RegistryEntry> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .filter_map(|p| serde_json::from_slice::<RegistryEntry>(&fs::read(p).ok()?).ok())
        .filter(|e| is_alive(e, now))
        .collect();
    live.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    live.dedup_by(|a, b| a.session_id == b.session_id);
    live
}

#[cfg(target_os = "linux")]
fn is_alive(entry: &RegistryEntry, _now: SystemTime) -> bool {
    let Ok(stat) = fs::read_to_string(format!("/proc/{}/stat", entry.pid)) else {
        return false;
    };
    // Guard against pid reuse: field 22 (start time) must match what Claude recorded.
    let start = stat
        .rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().nth(19));
    match (&entry.proc_start, start) {
        (Some(recorded), Some(actual)) => recorded == actual,
        _ => true,
    }
}

#[cfg(not(target_os = "linux"))]
fn is_alive(entry: &RegistryEntry, now: SystemTime) -> bool {
    entry
        .updated_at_ms
        .is_some_and(|ms| unix_secs(now) - ms / 1000 <= REGISTRY_FRESHNESS_SECS)
}

/// Claude stores each transcript as `projects/<encoded cwd>/<session id>.jsonl`.
/// Searching every project folder is slow, so it only happens when `search` is set.
fn transcript_path(projects_dir: &Path, entry: &RegistryEntry, search: bool) -> Option<PathBuf> {
    let file = format!("{}.jsonl", entry.session_id);
    if let Some(cwd) = &entry.cwd {
        let encoded: String = cwd
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let direct = projects_dir.join(encoded).join(&file);
        if direct.is_file() {
            return Some(direct);
        }
    }
    if !search {
        return None;
    }
    fs::read_dir(projects_dir)
        .ok()?
        .flatten()
        .map(|d| d.path().join(&file))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SENTINEL: &str = "SENTINEL-PRIVATE-CLAUDE-4410";

    fn assistant(model: &str, input: u64, cache_read: u64, iterations: &str) -> String {
        format!(
            r#"{{"type":"assistant","isSidechain":false,"timestamp":"2026-10-19T12:00:00Z","requestedModel":"{model}","message":{{"model":"claude-opus-5-5","content":[{{"type":"text","text":"{SENTINEL}"}}],"usage":{{"input_tokens":{input},"cache_creation_input_tokens":0,"cache_read_input_tokens":{cache_read},"output_tokens":50{iterations}}}}}}}"#
        )
    }

    fn read(lines: &[String]) -> Transcript {
        let mut t = Transcript::default();
        let text = lines.join("\n") + "\n";
        consume_lines(text.as_bytes(), |l| t.apply(l));
        t
    }

    #[test]
    fn one_million_model_gives_a_percentage() {
        let t = read(&[assistant("claude-opus-5-5[1m]", 2, 249_998, "")]);
        let ctx = t.context.unwrap();
        assert_eq!(ctx.used_tokens, 250_000);
        assert_eq!(ctx.context_window, Some(ONE_MILLION));
        assert_eq!(ctx.used_percent, Some(25.0));
        assert_eq!(t.model.as_deref(), Some("claude-opus-5-5[1m]"));
    }

    #[test]
    fn window_is_estimated_from_the_model_name() {
        let window = |model| read(&[assistant(model, 10, 990, "")]).context.unwrap();
        assert_eq!(
            window("claude-sonnet-5-5[1m]").context_window,
            Some(ONE_MILLION)
        );
        assert_eq!(
            window("claude-opus-5-5").context_window,
            Some(TWO_HUNDRED_K)
        );
        assert_eq!(
            window("us.anthropic.claude-opus-4-1").context_window,
            Some(TWO_HUNDRED_K)
        );
        assert_eq!(window("claude-haiku-5-5").used_percent, Some(0.5));
        let unknown = window("some-future-model");
        assert_eq!(unknown.used_tokens, 1_000);
        assert_eq!(unknown.context_window, None);
        assert_eq!(unknown.used_percent, None);
    }

    #[test]
    fn a_chat_past_200k_must_have_the_1m_window() {
        let ctx = read(&[assistant("claude-opus-5-5", 1, 299_999, "")])
            .context
            .unwrap();
        assert_eq!(ctx.context_window, Some(ONE_MILLION));
        assert_eq!(ctx.used_percent, Some(30.0));
    }

    #[test]
    fn last_message_iteration_wins_over_totals() {
        let iterations = r#","iterations":[{"type":"message","input_tokens":5,"cache_read_input_tokens":100},{"type":"advisor","input_tokens":9000},{"type":"message","input_tokens":7,"cache_read_input_tokens":200}]"#;
        let ctx = read(&[assistant("claude-opus-5-5[1m]", 9999, 9999, iterations)])
            .context
            .unwrap();
        assert_eq!(ctx.used_tokens, 207);
    }

    #[test]
    fn compaction_clears_context_until_next_reply() {
        let boundary =
            r#"{"type":"system","subtype":"compact_boundary","content":"Conversation compacted"}"#;
        let t = read(&[
            assistant("claude-opus-5-5[1m]", 1, 5000, ""),
            boundary.to_string(),
        ]);
        assert!(t.context.is_none());
        let t = read(&[assistant("claude-opus-5-5[1m]", 0, 0, "")]);
        assert!(t.context.is_none());
    }

    #[test]
    fn subagent_records_and_content_are_ignored() {
        let side = assistant("claude-haiku-5-5", 1, 1, "")
            .replace(r#""isSidechain":false"#, r#""isSidechain":true"#);
        let t = read(&[assistant("claude-opus-5-5[1m]", 1, 99, ""), side]);
        assert_eq!(t.context.as_ref().unwrap().used_tokens, 100);
        assert!(!format!("{:?}{:?}", t.context, t.model).contains(SENTINEL));
    }

    #[test]
    fn closed_chats_keep_their_title_and_folder() {
        let first = r#"{"type":"user","cwd":"/home/u/Code/my-app","entrypoint":"cli","message":{"content":"SENTINEL-PRIVATE-CLAUDE-4410"}}"#;
        let later = r#"{"type":"user","cwd":"/elsewhere","entrypoint":"sdk-cli"}"#;
        let ai = r#"{"type":"ai-title","aiTitle":"Auto title","sessionId":"abc"}"#;
        let custom = r#"{"type":"custom-title","customTitle":"Fix login","sessionId":"abc"}"#;
        let t = read(&[first.into(), later.into(), ai.into()]);
        assert_eq!(t.cwd.as_deref(), Some("/home/u/Code/my-app"));
        assert_eq!(t.entrypoint.as_deref(), Some("cli"));
        assert_eq!(t.title().as_deref(), Some("Auto title"));
        let t = read(&[first.into(), ai.into(), custom.into()]);
        assert_eq!(t.title().as_deref(), Some("Fix login"));
    }

    #[test]
    fn only_recent_top_level_transcripts_are_listed() {
        let dir = std::env::temp_dir().join(format!("acm-recent-{}", std::process::id()));
        let project = dir.join("-home-u-app");
        fs::create_dir_all(project.join("abc").join("subagents")).unwrap();
        fs::write(project.join("abc.jsonl"), "").unwrap();
        fs::write(
            project.join("abc").join("subagents").join("agent-1.jsonl"),
            "",
        )
        .unwrap();
        let now = SystemTime::now();
        let found = recent_transcripts(&dir, now);
        assert_eq!(found, vec![("abc".to_string(), project.join("abc.jsonl"))]);
        let tomorrow = now + std::time::Duration::from_secs(RECENT_CHAT_SECS as u64 + 60);
        assert!(recent_transcripts(&dir, tomorrow).is_empty());
        fs::remove_dir_all(&dir).unwrap();
    }

    fn chat(id: &str, title: &str, cwd: &str, last: i64, live: bool) -> ChatContext {
        ChatContext {
            provider: PROVIDER,
            id: id.into(),
            title: Some(title.into()),
            project: None,
            cwd: Some(cwd.into()),
            model: None,
            context: None,
            last_activity: last,
            subagents: 0,
            activity: live.then_some(Activity::Idle),
            surface: None,
        }
    }

    #[test]
    fn reopened_chats_show_only_the_current_session() {
        let mut chats = vec![
            chat("live", "Sheetbug", "/app", 100, true),
            chat("old", "Sheetbug", "/app", 90, false),
            chat("older", "Sheetbug", "/app", 50, false),
            chat("other-folder", "Sheetbug", "/elsewhere", 80, false),
            chat("closed-new", "Docs", "/app", 70, false),
            chat("closed-old", "Docs", "/app", 60, false),
        ];
        drop_superseded(&mut chats);
        let ids: Vec<&str> = chats.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["live", "other-folder", "closed-new"]);

        let mut both_open = vec![
            chat("a", "Same", "/app", 10, true),
            chat("b", "Same", "/app", 20, true),
        ];
        drop_superseded(&mut both_open);
        assert_eq!(both_open.len(), 2);
    }

    #[test]
    fn transcript_path_encodes_cwd() {
        let dir = std::env::temp_dir().join(format!("acm-test-{}", std::process::id()));
        let project = dir.join("-home-u-Code-my-app");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("abc.jsonl"), "").unwrap();
        let entry = RegistryEntry {
            pid: 1,
            session_id: "abc".into(),
            cwd: Some("/home/u/Code/my.app".into()),
            proc_start: None,
            name: None,
            name_source: None,
            entrypoint: None,
            status: None,
            updated_at_ms: None,
        };
        assert_eq!(
            transcript_path(&dir, &entry, false),
            Some(project.join("abc.jsonl"))
        );
        fs::remove_file(project.join("abc.jsonl")).unwrap();
        fs::remove_dir(&project).unwrap();
        fs::remove_dir(&dir).unwrap();
    }
}
