//! Finding installed command line tools and opening one in a terminal window.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The first of `names` in the usual install folders. An app opened from the Dock,
/// Start menu, or a launcher gets a much shorter PATH than a terminal, so those
/// folders are searched too.
pub fn find(names: &[&str]) -> Option<PathBuf> {
    search_dirs()
        .into_iter()
        .flat_map(|dir| names.iter().map(move |n| dir.join(n)))
        .find(|p| p.is_file())
}

fn search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
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
        nvm.sort();
        dirs.extend(nvm.into_iter().rev());
    }
    dirs
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
    fn script_quotes_the_program_path() {
        let script = shell_script(Path::new("/Users/o'neil/my bin/claude"), &["auth", "login"]);
        assert!(script.contains(r"'/Users/o'\''neil/my bin/claude' 'auth' 'login'"));
        assert!(script.starts_with("#!/bin/sh\n"));
    }
}
