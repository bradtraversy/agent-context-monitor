//! Saving files so that a crash or a full disk never leaves one half written.

use std::path::{Path, PathBuf};

/// Replaces the file in one step, readable only by its owner: the new contents go
/// to a temporary file next to it, which then takes its place.
pub fn replace(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = tmp_path(path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let result = options
        .open(&tmp)
        .and_then(|mut f| f.write_all(contents).and_then(|_| f.sync_all()))
        .and_then(|_| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// The temporary file `replace` writes first.
pub fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".acm-{}", std::process::id()));
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_the_whole_file_and_leaves_nothing_behind() {
        let dir = std::env::temp_dir().join(format!("acm-files-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, "an older, longer file").unwrap();
        replace(&path, b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert!(!tmp_path(&path).exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
