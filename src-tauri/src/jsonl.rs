//! Incremental reading of append-only JSONL session files.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

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

pub fn read_from(path: &Path, offset: u64) -> std::io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

/// Feeds each complete line to `f` and returns the bytes consumed. A partial
/// trailing line is left for the next read.
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
    haystack.windows(needle.len()).any(|w| w == needle)
}
