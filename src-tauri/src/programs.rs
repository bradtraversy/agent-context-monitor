//! Finding installed command line tools and the PATH to run them with.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// The first of `names` in the usual install folders. An app opened from the Dock,
/// Start menu, or a launcher gets a much shorter PATH than a terminal, so those
/// folders are searched too.
pub fn find(names: &[&str]) -> Option<PathBuf> {
    search_dirs()
        .into_iter()
        .flat_map(|dir| names.iter().map(move |n| dir.join(n)))
        .find(|p| p.is_file())
}

/// A PATH for running a found program: its own folder first, then every folder searched
/// for it, so a Node script finds the `node` it was installed with.
pub fn path_for(program: &Path) -> Option<OsString> {
    let dirs = program
        .parent()
        .map(Path::to_path_buf)
        .into_iter()
        .chain(search_dirs());
    std::env::join_paths(dirs).ok()
}

fn search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    // Folders a shell profile adds, such as a Node install in the home folder.
    if let Some(path) = login_shell_path() {
        dirs.extend(std::env::split_paths(path));
    }
    let home = std::env::home_dir().unwrap_or_default();
    if cfg!(windows) {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            dirs.push(PathBuf::from(appdata).join("npm"));
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(PathBuf::from(local).join("Programs").join("codex"));
        }
        dirs.push(home.join(".local").join("bin"));
    } else {
        for dir in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
            dirs.push(PathBuf::from(dir));
        }
        for dir in [
            ".local/bin",
            ".npm-global/bin",
            ".bun/bin",
            ".volta/bin",
            ".cargo/bin",
        ] {
            dirs.push(home.join(dir));
        }
        // nvm keeps one folder per Node version; prefer the newest.
        let mut nvm: Vec<PathBuf> = std::fs::read_dir(home.join(".nvm/versions/node"))
            .map(|entries| entries.flatten().map(|e| e.path().join("bin")).collect())
            .unwrap_or_default();
        nvm.sort_by_key(|bin| node_version(bin));
        dirs.extend(nvm.into_iter().rev());
    }
    dirs
}

/// Printed around the PATH so it can be told apart from what a shell profile prints.
#[cfg(unix)]
const MARK: &str = "__ACM_PATH__";

/// The PATH the user's own terminal gets, asked of their login shell once per run. An
/// app opened from the Dock or a launcher gets a much shorter one.
fn login_shell_path() -> Option<&'static OsString> {
    static PATH: OnceLock<Option<OsString>> = OnceLock::new();
    PATH.get_or_init(read_login_shell_path).as_ref()
}

#[cfg(unix)]
fn read_login_shell_path() -> Option<OsString> {
    // A slow or stuck shell profile must not hold up the app for long.
    const WAIT: Duration = Duration::from_secs(5);
    let shell = std::env::var_os("SHELL").filter(|s| !s.is_empty())?;
    shell_path(&shell, WAIT)
}

/// Asks `shell` for its login PATH, giving up after `wait`.
#[cfg(unix)]
fn shell_path(shell: &std::ffi::OsStr, wait: Duration) -> Option<OsString> {
    use std::io::Read;
    use std::sync::mpsc;
    let mut child = Command::new(shell)
        .args([
            "-l",
            "-i",
            "-c",
            &format!("printf '{MARK}%s{MARK}' \"$PATH\""),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Read as the output arrives instead of after the shell exits: something the profile
    // starts in the background can keep the output open long after, and waiting for it
    // to close would never finish.
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = stdout.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + wait;
    let mut out = Vec::new();
    let path = loop {
        if let Some(path) = marked_path(&String::from_utf8_lossy(&out), MARK) {
            break Some(OsString::from(path));
        }
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(chunk) => out.extend(chunk),
            // Timed out, or the output closed without the PATH in it.
            Err(_) => break None,
        }
    };
    let _ = child.kill();
    let _ = child.wait();
    path
}

#[cfg(not(unix))]
fn read_login_shell_path() -> Option<OsString> {
    None
}

/// The text between two marks, since a shell profile can print its own lines around it.
#[cfg_attr(not(unix), allow(dead_code))]
fn marked_path<'a>(out: &'a str, mark: &str) -> Option<&'a str> {
    let start = out.find(mark)? + mark.len();
    let end = start + out[start..].find(mark)?;
    Some(&out[start..end]).filter(|p| !p.is_empty())
}

/// `v22.10.0` from nvm's `.../v22.10.0/bin`, as numbers so it sorts after `v22.9.0`.
fn node_version(bin: &Path) -> Vec<u64> {
    bin.parent()
        .and_then(|d| d.file_name())
        .and_then(|n| n.to_str())
        .map(|n| {
            n.trim_start_matches('v')
                .split('.')
                .map(|part| part.parse().unwrap_or(0))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_versions_sort_as_numbers() {
        let mut bins: Vec<PathBuf> = ["v22.10.0", "v22.9.0", "v8.1.0"]
            .iter()
            .map(|v| PathBuf::from("/n").join(v).join("bin"))
            .collect();
        bins.sort_by_key(|bin| node_version(bin));
        let names: Vec<_> = bins
            .iter()
            .map(|b| b.parent().unwrap().file_name().unwrap())
            .collect();
        assert_eq!(names, ["v8.1.0", "v22.9.0", "v22.10.0"]);
    }

    #[test]
    fn reads_the_path_between_the_marks() {
        let out = "Welcome back!\n__M__/a/bin:/usr/bin__M__\n";
        assert_eq!(marked_path(out, "__M__"), Some("/a/bin:/usr/bin"));
        assert_eq!(marked_path("no marks", "__M__"), None);
        assert_eq!(marked_path("__M____M__", "__M__"), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_profile_left_running_in_the_background_does_not_hold_up_the_path() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("acm-shell-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Stands in for a shell whose profile starts something that keeps the output open.
        let shell = dir.join("shell");
        std::fs::write(
            &shell,
            format!("#!/bin/sh\nsleep 30 &\nprintf '{MARK}/x/bin{MARK}'\n"),
        )
        .unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = Instant::now();
        let path = shell_path(shell.as_os_str(), Duration::from_secs(10));
        assert_eq!(path, Some(OsString::from("/x/bin")));
        assert!(started.elapsed() < Duration::from_secs(5));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
