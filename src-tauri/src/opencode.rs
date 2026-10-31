//! OpenCode chats and how full their context is, from the database OpenCode keeps every
//! chat in. OpenCode runs on the user's other logins and has no plan limits of its own, so
//! it adds chats but no usage card.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;

use crate::jsonl::project_label;
use crate::model::{
    Activity, ChatContext, Confidence, ContextMeasure, ProviderInfo, RECENT_CHAT_SECS, unix_secs,
};

const PROVIDER: &str = "opencode";
/// The database is read again at least this often, so idle chats age out of the list.
const REREAD_SECS: i64 = 60;
/// A reply still being written this long after the chat was last saved was cut off.
const WORKING_SECS: i64 = 10 * 60;

pub struct OpenCodeChats {
    data_dir: PathBuf,
    cache_dir: PathBuf,
    /// The database files' change times when `chats` was last read, and when that was.
    read_at: Option<(Vec<Option<SystemTime>>, i64)>,
    chats: Vec<ChatContext>,
    windows: ModelWindows,
}

impl OpenCodeChats {
    pub const INFO: ProviderInfo = ProviderInfo {
        id: PROVIDER,
        name: "OpenCode",
        chats: true,
    };

    pub fn new(home: PathBuf) -> Self {
        let xdg = |name: &str, fallback: &str| {
            std::env::var_os(name)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(fallback))
        };
        OpenCodeChats {
            data_dir: xdg("XDG_DATA_HOME", ".local/share").join("opencode"),
            cache_dir: xdg("XDG_CACHE_HOME", ".cache").join("opencode"),
            read_at: None,
            chats: Vec::new(),
            windows: ModelWindows::default(),
        }
    }

    /// OpenCode's database: `opencode.db`, or one per release channel such as
    /// `opencode-dev.db`, so the most recently used one.
    fn db_path(&self) -> Option<PathBuf> {
        if let Some(db) = std::env::var_os("OPENCODE_DB").filter(|v| !v.is_empty()) {
            let db = self.data_dir.join(db);
            return db.is_file().then_some(db);
        }
        fs::read_dir(&self.data_dir)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("opencode") && n.ends_with(".db"))
            })
            .max_by_key(|p| fs::metadata(p).and_then(|m| m.modified()).ok())
    }

    pub fn installed(&self) -> bool {
        self.db_path().is_some()
    }

    pub fn chats(&mut self, now: SystemTime) -> Vec<ChatContext> {
        let Some(db) = self.db_path() else {
            self.chats.clear();
            return Vec::new();
        };
        // OpenCode writes to the -wal file first, so it changes when the database doesn't.
        let stamps: Vec<Option<SystemTime>> = ["", "-wal"]
            .iter()
            .map(|suffix| {
                let mut path = db.clone().into_os_string();
                path.push(suffix);
                fs::metadata(path).and_then(|m| m.modified()).ok()
            })
            .collect();
        let secs = unix_secs(now);
        let fresh = self
            .read_at
            .as_ref()
            .is_some_and(|(s, at)| *s == stamps && secs - at < REREAD_SECS);
        if !fresh {
            self.windows.refresh(&self.cache_dir.join("models.json"));
            if let Some(chats) = read_chats(&db, secs, &self.windows) {
                self.chats = chats;
            }
            self.read_at = Some((stamps, secs));
        }
        self.chats.clone()
    }
}

/// Each model's context window, from the model list OpenCode keeps a copy of.
#[derive(Default)]
struct ModelWindows {
    modified: Option<SystemTime>,
    /// By provider id, then model id.
    windows: HashMap<String, HashMap<String, u64>>,
}

#[derive(Deserialize)]
struct ModelProvider {
    #[serde(default)]
    models: HashMap<String, ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    limit: Option<ModelLimit>,
}

#[derive(Deserialize)]
struct ModelLimit {
    context: Option<u64>,
}

impl ModelWindows {
    fn refresh(&mut self, path: &Path) {
        let modified = fs::metadata(path).and_then(|m| m.modified()).ok();
        if modified.is_none() || modified == self.modified {
            return;
        }
        self.modified = modified;
        let Ok(providers) = fs::read(path).map(|b| {
            serde_json::from_slice::<HashMap<String, ModelProvider>>(&b).unwrap_or_default()
        }) else {
            return;
        };
        self.windows = providers
            .into_iter()
            .map(|(id, p)| {
                let models = p
                    .models
                    .into_iter()
                    .filter_map(|(m, e)| Some((m, e.limit?.context.filter(|&c| c > 0)?)))
                    .collect();
                (id, models)
            })
            .collect();
    }

    fn get(&self, provider: Option<&str>, model: &str) -> Option<u64> {
        if let Some(window) = provider.and_then(|p| self.windows.get(p)?.get(model)) {
            return Some(*window);
        }
        // A provider OpenCode's list doesn't know may still serve a model it does.
        self.windows.values().find_map(|m| m.get(model)).copied()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Message {
    role: Option<String>,
    #[serde(rename = "modelID")]
    model_id: Option<String>,
    #[serde(rename = "providerID")]
    provider_id: Option<String>,
    tokens: Option<Tokens>,
    time: Option<Times>,
}

#[derive(Deserialize)]
struct Tokens {
    #[serde(default)]
    input: u64,
    #[serde(default)]
    output: u64,
    #[serde(default)]
    reasoning: u64,
    cache: Option<CacheTokens>,
}

#[derive(Deserialize)]
struct CacheTokens {
    #[serde(default)]
    read: u64,
    #[serde(default)]
    write: u64,
}

#[derive(Deserialize)]
struct Times {
    created: Option<i64>,
    completed: Option<i64>,
}

impl Tokens {
    /// Everything the model saw and wrote in a reply, which OpenCode shows as the chat's
    /// context.
    fn total(&self) -> u64 {
        let cache = self.cache.as_ref().map_or(0, |c| c.read + c.write);
        self.input + self.output + self.reasoning + cache
    }
}

struct SessionRow {
    id: String,
    title: Option<String>,
    directory: Option<String>,
    updated_ms: i64,
    subagents: usize,
}

fn read_chats(db: &Path, now: i64, windows: &ModelWindows) -> Option<Vec<ChatContext>> {
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    let since_ms = (now - RECENT_CHAT_SECS) * 1000;
    let query = |archived: &str| {
        format!(
            "SELECT s.id, s.title, s.directory, s.time_updated,
               (SELECT COUNT(*) FROM session c WHERE c.parent_id = s.id AND c.time_updated >= ?1)
             FROM session s
             WHERE s.parent_id IS NULL AND s.time_updated >= ?1 {archived}
             ORDER BY s.time_updated DESC LIMIT 50"
        )
    };
    // Older versions have no archive column.
    let mut sessions = conn
        .prepare(&query("AND s.time_archived IS NULL"))
        .or_else(|_| conn.prepare(&query("")))
        .ok()?;
    let rows: Vec<SessionRow> = sessions
        .query_map([since_ms], |row| {
            Ok(SessionRow {
                id: row.get(0)?,
                title: row.get(1)?,
                directory: row.get(2)?,
                updated_ms: row.get(3)?,
                subagents: row.get::<_, i64>(4)? as usize,
            })
        })
        .ok()?
        .flatten()
        .collect();
    let mut messages = conn
        .prepare(
            "SELECT data FROM message WHERE session_id = ?1 ORDER BY time_created DESC LIMIT 30",
        )
        .ok()?;
    let chats = rows
        .into_iter()
        .map(|row| {
            let replies: Vec<Message> = messages
                .query_map([&row.id], |r| r.get::<_, String>(0))
                .map(|rows| {
                    rows.flatten()
                        .filter_map(|data| serde_json::from_str::<Message>(&data).ok())
                        .filter(|m| m.role.as_deref() == Some("assistant"))
                        .collect()
                })
                .unwrap_or_default();
            chat_from(row, &replies, now, windows)
        })
        .collect();
    Some(chats)
}

/// A chat from its session and its replies, newest first.
fn chat_from(
    row: SessionRow,
    replies: &[Message],
    now: i64,
    windows: &ModelWindows,
) -> ChatContext {
    let last_activity = row.updated_ms / 1000;
    let measured = replies
        .iter()
        .find(|m| m.tokens.as_ref().is_some_and(|t| t.total() > 0));
    let context = measured.map(|m| {
        let used_tokens = m.tokens.as_ref().map_or(0, Tokens::total);
        let context_window = m
            .model_id
            .as_deref()
            .and_then(|model| windows.get(m.provider_id.as_deref(), model));
        let at_ms = m.time.as_ref().and_then(|t| t.completed.or(t.created));
        ContextMeasure {
            used_tokens,
            context_window,
            used_percent: context_window
                .map(|w| (used_tokens as f64 / w as f64 * 100.0).min(100.0)),
            confidence: Confidence::Confirmed,
            observed_at: at_ms
                .and_then(chrono::DateTime::from_timestamp_millis)
                .map(|t| t.to_rfc3339())
                .unwrap_or_default(),
            limited: false,
        }
    });
    let writing = replies
        .first()
        .is_some_and(|m| m.time.as_ref().is_some_and(|t| t.completed.is_none()));
    ChatContext {
        provider: PROVIDER,
        project: row.directory.as_deref().and_then(project_label),
        cwd: row.directory,
        title: row.title.filter(|t| !t.is_empty()),
        model: replies.first().and_then(|m| m.model_id.clone()),
        context,
        last_activity,
        subagents: row.subagents,
        activity: Some(if writing && now - last_activity < WORKING_SECS {
            Activity::Working
        } else {
            Activity::Idle
        }),
        surface: None,
        id: row.id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_793_000_000;

    fn database(dir: &Path) -> PathBuf {
        fs::create_dir_all(dir).unwrap();
        let db = dir.join("opencode.db");
        let _ = fs::remove_file(&db);
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT NOT NULL,
               title TEXT NOT NULL, time_updated INTEGER NOT NULL, time_archived INTEGER);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
               time_created INTEGER NOT NULL, data TEXT NOT NULL);",
        )
        .unwrap();
        let ms = |secs_ago: i64| (NOW - secs_ago) * 1000;
        let sessions = [
            ("ses_main", None, "Fix login", 60, None),
            ("ses_sub", Some("ses_main"), "Look around", 120, None),
            ("ses_old", None, "Yesterday", 2 * 86_400, None),
            ("ses_archived", None, "Archived", 60, Some(ms(30))),
        ];
        for (id, parent, title, ago, archived) in sessions {
            conn.execute(
                "INSERT INTO session VALUES (?1, ?2, '/home/brad/Code/shop', ?3, ?4, ?5)",
                rusqlite::params![id, parent, title, ms(ago), archived],
            )
            .unwrap();
        }
        let reply = |input: u64, cached: u64, completed: bool| {
            let done = if completed { r#","completed":2"# } else { "" };
            format!(
                r#"{{"role":"assistant","modelID":"claude-sonnet-4-5","providerID":"anthropic",
                "tokens":{{"input":{input},"output":{cached},"reasoning":0,"cache":{{"read":{cached},"write":{cached}}}}},
                "time":{{"created":1{done}}}}}"#
            )
        };
        let messages = [
            (
                "m1",
                300,
                r#"{"role":"user","time":{"created":1}}"#.to_string(),
            ),
            ("m2", 200, reply(48_500, 500, true)),
            ("m3", 100, reply(0, 0, false)),
        ];
        for (id, ago, data) in messages {
            conn.execute(
                "INSERT INTO message VALUES (?1, 'ses_main', ?2, ?3)",
                rusqlite::params![id, ms(ago), data],
            )
            .unwrap();
        }
        db
    }

    #[test]
    fn lists_recent_top_level_chats_with_their_context() {
        let dir = std::env::temp_dir().join(format!("acm-opencode-{}", std::process::id()));
        let db = database(&dir);
        let mut windows = ModelWindows::default();
        let models = dir.join("models.json");
        fs::write(
            &models,
            r#"{"anthropic":{"models":{"claude-sonnet-4-5":{"limit":{"context":200000,"output":64000}}}}}"#,
        )
        .unwrap();
        windows.refresh(&models);
        let chats = read_chats(&db, NOW, &windows).unwrap();
        assert_eq!(chats.len(), 1);
        let chat = &chats[0];
        assert_eq!(chat.id, "ses_main");
        assert_eq!(chat.title.as_deref(), Some("Fix login"));
        assert_eq!(chat.project.as_deref(), Some("shop"));
        assert_eq!(chat.subagents, 1);
        assert_eq!(chat.model.as_deref(), Some("claude-sonnet-4-5"));
        // The newest reply is still being written and has no tokens yet.
        assert_eq!(chat.activity, Some(Activity::Working));
        let context = chat.context.as_ref().unwrap();
        assert_eq!(context.used_tokens, 50_000);
        assert_eq!(context.context_window, Some(200_000));
        assert_eq!(context.used_percent, Some(25.0));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_model_the_list_lacks_has_no_percentage() {
        let windows = ModelWindows::default();
        let row = SessionRow {
            id: "s".into(),
            title: Some(String::new()),
            directory: None,
            updated_ms: NOW * 1000,
            subagents: 0,
        };
        let reply: Message = serde_json::from_str(
            r#"{"role":"assistant","modelID":"mystery","tokens":{"input":10,"output":5},"time":{"created":1,"completed":2}}"#,
        )
        .unwrap();
        let chat = chat_from(row, &[reply], NOW, &windows);
        assert_eq!(chat.title, None);
        assert_eq!(chat.activity, Some(Activity::Idle));
        let context = chat.context.unwrap();
        assert_eq!(context.used_tokens, 15);
        assert_eq!(context.used_percent, None);
    }
}
