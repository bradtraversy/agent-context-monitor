//! The user's settings, saved as a small JSON file in the app's config folder.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    /// Tokens to measure each chat's context against; none means the model's window.
    context_limit: Option<u64>,
}

pub struct Settings {
    path: Option<PathBuf>,
    /// Zero means no limit, so the model's window is used.
    context_limit: AtomicU64,
}

impl Settings {
    pub fn load(path: Option<PathBuf>) -> Self {
        let saved: Saved = path
            .as_ref()
            .and_then(|p| fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Settings {
            path,
            context_limit: AtomicU64::new(saved.context_limit.unwrap_or(0)),
        }
    }

    pub fn context_limit(&self) -> Option<u64> {
        Some(self.context_limit.load(Ordering::Relaxed)).filter(|&l| l > 0)
    }

    pub fn set_context_limit(&self, limit: Option<u64>) {
        self.context_limit
            .store(limit.unwrap_or(0), Ordering::Relaxed);
        let Some(path) = &self.path else { return };
        let saved = Saved {
            context_limit: self.context_limit(),
        };
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_vec_pretty(&saved) {
            let _ = fs::write(path, json);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_limit_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("acm-settings-{}", std::process::id()));
        let path = dir.join("settings.json");
        Settings::load(Some(path.clone())).set_context_limit(Some(300_000));
        assert_eq!(
            Settings::load(Some(path.clone())).context_limit(),
            Some(300_000)
        );
        Settings::load(Some(path.clone())).set_context_limit(None);
        assert_eq!(Settings::load(Some(path)).context_limit(), None);
        fs::remove_dir_all(&dir).unwrap();
    }
}
