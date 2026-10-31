//! Finding installed command line tools and opening one in a terminal window.

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

/// The PATH the user's own terminal gets, asked of their login shell once per run. An
/// app opened from the Dock or a launcher gets a much shorter one.
fn login_shell_path() -> Option<&'static OsString> {
    static PATH: OnceLock<Option<OsString>> = OnceLock::new();
    PATH.get_or_init(read_login_shell_path).as_ref()
}

#[cfg(unix)]
fn read_login_shell_path() -> Option<OsString> {
    use std::io::Read;
    const MARK: &str = "__ACM_PATH__";
    // A slow or stuck shell profile must not hold up the app for long.
    const WAIT: Duration = Duration::from_secs(5);
    let shell = std::env::var_os("SHELL").filter(|s| !s.is_empty())?;
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
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < WAIT => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut out = String::new();
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    marked_path(&out, MARK).map(OsString::from)
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

/// Runs `program` with `args` in a new terminal window that stays open afterwards.
/// `args` are fixed words, never user input.
pub fn open_in_terminal(program: &Path, args: &[&str]) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        // Opening a .command file runs it in the user's terminal app with their own
        // shell setup, without asking for permission to control Terminal.
        use std::os::unix::fs::PermissionsExt;
        let script = std::env::temp_dir().join("agent-context-monitor-sign-in.command");
        std::fs::write(&script, shell_script(program, args))?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))?;
        spawn(Command::new("/usr/bin/open").arg(&script))
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
        // cmd drops the outer quotes, leaving the quoted program path and its words.
        let line = format!("/k \"\"{}\" {}\"", program.display(), args.join(" "));
        spawn(
            Command::new("cmd")
                .raw_arg(line)
                .creation_flags(CREATE_NEW_CONSOLE),
        )
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let script = shell_script(program, args);
        let preferred = std::env::var("TERMINAL").ok().filter(|t| !t.is_empty());
        let candidates = preferred
            .as_deref()
            .map(|t| (t, &["-e"][..]))
            .into_iter()
            .chain(LINUX_TERMINALS.iter().copied());
        for (terminal, run) in candidates {
            if let Some(bin) = find(&[terminal]) {
                return spawn(Command::new(bin).args(run).args(["sh", "-c", &script]));
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no terminal app found",
        ))
    }
}

/// Terminal apps and the option that makes each run a command, most likely first.
#[cfg(not(any(target_os = "macos", windows)))]
const LINUX_TERMINALS: &[(&str, &[&str])] = &[
    ("x-terminal-emulator", &["-e"]),
    ("gnome-terminal", &["--"]),
    ("ptyxis", &["--"]),
    ("kgx", &["--"]),
    ("konsole", &["-e"]),
    ("xfce4-terminal", &["-x"]),
    ("mate-terminal", &["-x"]),
    ("tilix", &["-e"]),
    ("kitty", &[]),
    ("alacritty", &["-e"]),
    ("wezterm", &["start", "--"]),
    ("ghostty", &["-e"]),
    ("foot", &[]),
    ("xterm", &["-e"]),
];

/// A shell script that runs the program, then waits so its last words stay readable.
#[cfg_attr(windows, allow(dead_code))]
fn shell_script(program: &Path, args: &[&str]) -> String {
    let quoted = |s: &str| format!("'{}'", s.replace('\'', r"'\''"));
    let command: Vec<String> = std::iter::once(program.to_string_lossy().as_ref())
        .chain(args.iter().copied())
        .map(quoted)
        .collect();
    format!(
        "#!/bin/sh\nclear\n{}\nprintf '\\nYou can close this window.\\n'\nread _\n",
        command.join(" ")
    )
}

/// Starts the program without waiting, reaping it in the background once it exits.
fn spawn(command: &mut Command) -> std::io::Result<()> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    std::thread::spawn(move || child.wait());
    Ok(())
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

    #[test]
    fn script_quotes_the_program_path() {
        let script = shell_script(Path::new("/Users/o'neil/my bin/claude"), &["auth", "login"]);
        assert!(script.contains(r"'/Users/o'\''neil/my bin/claude' 'auth' 'login'"));
        assert!(script.starts_with("#!/bin/sh\n"));
    }
}
