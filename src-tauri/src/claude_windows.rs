//! Remembers the context window each Claude model has been seen to run with.
//!
//! The desktop app's chats never run the status line, and their files name the model
//! without saying whether it has the 200k or the 1M window. What this computer has
//! already seen settles it: a terminal chat's status line reports the window outright,
//! and any chat that went past 200k must have had 1M. Either one counts for every chat
//! on that model. It is saved, so it still holds after a restart.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

pub const TWO_HUNDRED_K: u64 = 200_000;
pub const ONE_MILLION: u64 = 1_000_000;

pub struct KnownWindows {
    path: Option<PathBuf>,
    by_model: BTreeMap<String, u64>,
}

impl KnownWindows {
    pub fn load(path: Option<PathBuf>) -> Self {
        let by_model = path
            .as_ref()
            .and_then(|p| fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        KnownWindows { path, by_model }
    }

    /// The window seen for `model`, if any.
    pub fn get(&self, model: &str) -> Option<u64> {
        self.by_model.get(model).copied()
    }

    /// Notes that a chat on `model` ran with `window`. A model seen with the 1M window
    /// keeps it, since a smaller reading from another chat doesn't undo what was seen.
    pub fn saw_window(&mut self, model: &str, window: u64) {
        // A `[1m]` model says its window in its name, so there is nothing to learn.
        if window == 0 || model.ends_with("[1m]") || self.get(model) >= Some(window) {
            return;
        }
        self.by_model.insert(model.to_string(), window);
        self.save();
    }

    /// Notes the most context a chat on `model` has used. Past 200k proves 1M.
    pub fn saw_usage(&mut self, model: &str, peak: u64) {
        if peak > TWO_HUNDRED_K {
            self.saw_window(model, ONE_MILLION);
        }
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_vec_pretty(&self.by_model) {
            let _ = crate::files::replace(path, &json);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chat_past_200k_proves_1m_for_its_model() {
        let mut known = KnownWindows::load(None);
        known.saw_usage("claude-opus-5-5", 150_000);
        assert_eq!(known.get("claude-opus-5-5"), None);
        known.saw_usage("claude-opus-5-5", 777_070);
        assert_eq!(known.get("claude-opus-5-5"), Some(ONE_MILLION));
        assert_eq!(known.get("claude-sonnet-5-5"), None);
    }

    #[test]
    fn a_reported_window_counts_and_1m_is_kept() {
        let mut known = KnownWindows::load(None);
        known.saw_window("claude-haiku-5-5", TWO_HUNDRED_K);
        assert_eq!(known.get("claude-haiku-5-5"), Some(TWO_HUNDRED_K));
        known.saw_window("claude-haiku-5-5", ONE_MILLION);
        known.saw_window("claude-haiku-5-5", TWO_HUNDRED_K);
        assert_eq!(known.get("claude-haiku-5-5"), Some(ONE_MILLION));
        known.saw_window("claude-opus-5-5[1m]", ONE_MILLION);
        assert_eq!(known.get("claude-opus-5-5[1m]"), None);
    }

    #[test]
    fn what_was_seen_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("acm-windows-{}", std::process::id()));
        let path = dir.join("claude-windows.json");
        KnownWindows::load(Some(path.clone())).saw_usage("claude-opus-5-5", 300_000);
        assert_eq!(
            KnownWindows::load(Some(path)).get("claude-opus-5-5"),
            Some(ONE_MILLION)
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
