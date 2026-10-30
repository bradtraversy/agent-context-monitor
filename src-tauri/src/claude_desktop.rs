//! Reads the Claude desktop app's record of its Code chats.
//!
//! The desktop app starts a new Claude Code session each time a chat is reopened and
//! keeps the earlier session ids in `priorCliSessionIds`. It also marks archived chats.
//! Those sessions are hidden so a chat shows once, with its current context. The
//! title shown in the app's sidebar names chats Claude Code itself left untitled.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Deserialize;

#[derive(Deserialize)]
struct Record {
    #[serde(rename = "cliSessionId")]
    cli_session_id: Option<String>,
    #[serde(rename = "priorCliSessionIds", default)]
    prior_cli_session_ids: Vec<String>,
    #[serde(rename = "isArchived", default)]
    is_archived: bool,
    title: Option<String>,
}

struct Entry {
    modified: Option<SystemTime>,
    /// Session ids this chat hides: earlier ones, plus the current one when archived.
    hidden: Vec<String>,
    /// The current session id and the chat's title.
    title: Option<(String, String)>,
}

#[derive(Default)]
pub struct Desktop {
    /// Claude Code session ids that are replaced or archived.
    pub hidden: HashSet<String>,
    /// Chat titles by current Claude Code session id.
    pub titles: HashMap<String, String>,
}

pub struct DesktopChats {
    /// `claude-code-sessions` in the desktop app's config folder.
    dir: PathBuf,
    entries: HashMap<PathBuf, Entry>,
}

impl DesktopChats {
    pub fn new(dir: PathBuf) -> Self {
        DesktopChats {
            dir,
            entries: HashMap::new(),
        }
    }

    /// The folder holds every chat ever, so it is only searched when `search` is set.
    /// In between, only records changed lately are looked at again, since those are
    /// the chats in use. Records are only parsed again when they change.
    pub fn read(&mut self, search: bool, now: SystemTime) -> Desktop {
        let paths = if search {
            records(&self.dir)
        } else {
            self.entries
                .iter()
                .filter(|(_, e)| changed_lately(e, now))
                .map(|(path, _)| path.clone())
                .collect()
        };
        let mut entries = if search {
            HashMap::with_capacity(self.entries.len())
        } else {
            std::mem::take(&mut self.entries)
        };
        for path in paths {
            let Ok(meta) = fs::metadata(&path) else {
                entries.remove(&path);
                continue;
            };
            let modified = meta.modified().ok();
            let entry = match self.entries.remove(&path).or_else(|| entries.remove(&path)) {
                Some(entry) if entry.modified.is_some() && entry.modified == modified => entry,
                _ => fs::read(&path)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Record>(&bytes).ok())
                    .map(|record| entry_from(record, modified))
                    .unwrap_or(Entry {
                        modified,
                        hidden: Vec::new(),
                        title: None,
                    }),
            };
            entries.insert(path, entry);
        }
        self.entries = entries;
        Desktop {
            hidden: self
                .entries
                .values()
                .flat_map(|e| e.hidden.iter().cloned())
                .collect(),
            titles: self
                .entries
                .values()
                .filter_map(|e| e.title.clone())
                .collect(),
        }
    }
}

/// Records changed within this long are checked on every read, not just on a search.
const LATELY: Duration = Duration::from_secs(24 * 60 * 60);

fn changed_lately(entry: &Entry, now: SystemTime) -> bool {
    entry
        .modified
        .and_then(|m| now.duration_since(m).ok())
        .is_none_or(|age| age <= LATELY)
}

/// Chats started from a project all get this placeholder, so it names nothing.
pub const PLACEHOLDER_TITLE: &str = "Remote Control session (from a project)";

fn entry_from(record: Record, modified: Option<SystemTime>) -> Entry {
    let mut hidden = record.prior_cli_session_ids;
    if record.is_archived {
        hidden.extend(record.cli_session_id.clone());
    }
    let title = record
        .title
        .filter(|t| !t.trim().is_empty() && t != PLACEHOLDER_TITLE);
    Entry {
        modified,
        hidden,
        title: record.cli_session_id.zip(title),
    }
}

/// `<account>/<organization>/local_<id>.json` under the sessions folder.
fn records(dir: &Path) -> Vec<PathBuf> {
    let subdirs = |dir: &Path| -> Vec<PathBuf> {
        fs::read_dir(dir)
            .map(|d| {
                d.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
                    .collect()
            })
            .unwrap_or_default()
    };
    subdirs(dir)
        .iter()
        .flat_map(|account| subdirs(account))
        .flat_map(|org| fs::read_dir(org).into_iter().flatten().flatten())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|e| e == "json")
                && p.file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with("local_"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn earlier_and_archived_sessions_are_hidden() {
        let dir = std::env::temp_dir().join(format!("acm-desktop-{}", std::process::id()));
        let org = dir.join("account").join("org");
        fs::create_dir_all(&org).unwrap();
        fs::write(
            org.join("local_a.json"),
            r#"{"cliSessionId":"now","priorCliSessionIds":["old","older"],"isArchived":false,"title":"Sheetbug"}"#,
        )
        .unwrap();
        fs::write(
            org.join("local_b.json"),
            r#"{"cliSessionId":"gone","priorCliSessionIds":["gone-old"],"isArchived":true}"#,
        )
        .unwrap();
        fs::write(
            org.join("local_c.json"),
            r#"{"cliSessionId":"thread","title":"Remote Control session (from a project)"}"#,
        )
        .unwrap();
        fs::write(org.join("deleted_d"), "x").unwrap();
        let mut desktop = DesktopChats::new(dir.clone());
        let now = SystemTime::now();
        let read = desktop.read(true, now);
        let mut hidden: Vec<&str> = read.hidden.iter().map(String::as_str).collect();
        hidden.sort();
        assert_eq!(hidden, ["gone", "gone-old", "old", "older"]);
        assert_eq!(read.titles.get("now").map(String::as_str), Some("Sheetbug"));
        assert!(!read.titles.contains_key("thread"));
        assert!(desktop.read(false, now).hidden.contains("old"));
        // Without a search, a recently changed record is still looked at again.
        fs::write(
            org.join("local_b.json"),
            r#"{"cliSessionId":"gone","isArchived":false}"#,
        )
        .unwrap();
        assert!(!desktop.read(false, now).hidden.contains("gone"));
        fs::remove_dir_all(&dir).unwrap();
        assert!(desktop.read(false, now).hidden.is_empty());
    }
}
