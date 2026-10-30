//! Incremental reading of append-only JSONL session files.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Folders are searched for new session files this often. Files already known are
/// checked on every refresh, which is only one look per file.
pub const RESCAN_INTERVAL: Duration = Duration::from_secs(30);

/// For `#[serde(default, deserialize_with = "jsonl::null_as_default")]`: a field
/// that is missing or `null` gets its default, where plain `default` rejects `null`
/// and with it the whole record.
pub fn null_as_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + serde::Deserialize<'de>,
{
    Ok(<Option<T> as serde::Deserialize>::deserialize(deserializer)?.unwrap_or_default())
}

/// Whether a folder search last done at `last` is due again.
pub fn rescan_due(last: Option<SystemTime>, now: SystemTime) -> bool {
    last.is_none_or(|t| {
        now.duration_since(t)
            .ok()
            .is_none_or(|d| d >= RESCAN_INTERVAL)
    })
}

pub fn project_label(cwd: &str) -> Option<String> {
    Path::new(cwd)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
}

pub fn collect_jsonl(dir: &Path, out: &mut Vec<(PathBuf, SystemTime, u64)>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        let path = entry.path();
        if meta.is_dir() {
            collect_jsonl(&path, out);
        } else if path.extension().is_some_and(|e| e == "jsonl")
            && let Ok(modified) = meta.modified()
        {
            out.push((path, modified, meta.len()));
        }
    }
}

/// Feeds each complete line after `offset` to `f`, one line at a time so a large
/// file is never held in memory, and returns the bytes consumed. A partial trailing
/// line is left for the next read.
pub fn read_lines_from(path: &Path, offset: u64, mut f: impl FnMut(&[u8])) -> u64 {
    let Ok(mut file) = File::open(path) else {
        return 0;
    };
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return 0;
    }
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut line = Vec::new();
    let mut consumed = 0;
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(n) if n > 0 && line.last() == Some(&b'\n') => {
                consumed += n as u64;
                let body = &line[..n - 1];
                if !body.is_empty() {
                    f(body);
                }
            }
            _ => return consumed,
        }
    }
}

/// Feeds each complete line to `f` and returns the bytes consumed. A partial
/// trailing line is left for the next read.
#[cfg(test)]
pub fn consume_lines(chunk: &[u8], mut f: impl FnMut(&[u8])) -> u64 {
    let Some(end) = chunk.iter().rposition(|&b| b == b'\n') else {
        return 0;
    };
    for line in chunk[..end].split(|&b| b == b'\n') {
        if !line.is_empty() {
            f(line);
        }
    }
    (end + 1) as u64
}

pub fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    memchr::memmem::find(haystack, needle).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_whole_lines_and_leaves_a_partial_one() {
        let dir = std::env::temp_dir().join(format!("acm-jsonl-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.jsonl");
        fs::write(&path, "one\n\ntwo\nthr").unwrap();
        let mut lines = Vec::new();
        let used = read_lines_from(&path, 0, |l| lines.push(l.to_vec()));
        assert_eq!(lines, vec![b"one".to_vec(), b"two".to_vec()]);
        assert_eq!(used, 9);
        fs::write(&path, "one\n\ntwo\nthree\n").unwrap();
        lines.clear();
        read_lines_from(&path, used, |l| lines.push(l.to_vec()));
        assert_eq!(lines, vec![b"three".to_vec()]);
        fs::remove_dir_all(&dir).unwrap();
    }
}
