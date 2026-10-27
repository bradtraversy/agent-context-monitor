//! Reads the Claude desktop app's record of its Code chats.
//!
//! The desktop app starts a new Claude Code session each time a chat is reopened and
//! keeps the earlier session ids in `priorCliSessionIds`. It also marks archived chats.
//! Those sessions are hidden so a chat shows once, with its current context.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Deserialize;

#[derive(Deserialize)]
struct Record {
    #[serde(rename = "cliSessionId")]
    cli_session_id: Option<String>,
    #[serde(rename = "priorCliSessionIds", default)]
    prior_cli_session_ids: Vec<String>,
    #[serde(rename = "isArchived", default)]
    is_archived: bool,
}

struct Entry {
    modified: Option<SystemTime>,
    /// Session ids this chat hides: earlier ones, plus the current one when archived.
    hidden: Vec<String>,
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

    /// Claude Code session ids that are replaced or archived. Records are only parsed
    /// again when they change, since the folder holds every chat ever.
    pub fn hidden_sessions(&mut self) -> HashSet<String> {
        let mut entries = HashMap::with_capacity(self.entries.len());
        for path in records(&self.dir) {
            let modified = fs::metadata(&path).and_then(|m| m.modified()).ok();
            let entry = match self.entries.remove(&path) {
                Some(entry) if entry.modified.is_some() && entry.modified == modified => entry,
                _ => Entry {
                    modified,
                    hidden: fs::read(&path)
                        .ok()
                        .and_then(|bytes| serde_json::from_slice::<Record>(&bytes).ok())
                        .map(hidden_by)
                        .unwrap_or_default(),
                },
            };
            entries.insert(path, entry);
        }
        self.entries = entries;
        self.entries
            .values()
            .flat_map(|e| e.hidden.iter().cloned())
            .collect()
    }
}

fn hidden_by(record: Record) -> Vec<String> {
    let mut hidden = record.prior_cli_session_ids;
    if record.is_archived {
        hidden.extend(record.cli_session_id);
    }
    hidden
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
        fs::write(org.join("deleted_c"), "x").unwrap();
        let mut desktop = DesktopChats::new(dir.clone());
        let hidden = desktop.hidden_sessions();
        let mut hidden: Vec<&str> = hidden.iter().map(String::as_str).collect();
        hidden.sort();
        assert_eq!(hidden, ["gone", "gone-old", "old", "older"]);
        assert!(desktop.hidden_sessions().contains("old"));
        fs::remove_dir_all(&dir).unwrap();
        assert!(desktop.hidden_sessions().is_empty());
    }
}
