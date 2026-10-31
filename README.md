# Agent Context Monitor

A small desktop app that lives in your tray and shows how much Claude Code, Codex, GitHub Copilot, and Cursor usage you have left, plus how full the context is in each of your recent Claude Code, Codex, and OpenCode chats. It reads what's already on your computer and never stores your messages.

![Usage, Context, and History tabs side by side](docs/screenshots/tabs.png)

## Install

On macOS or Linux, run this in a terminal:

```bash
curl -fsSL https://raw.githubusercontent.com/bradtraversy/agent-context-monitor/main/install.sh | sh
```

On Windows, run this in PowerShell:

```powershell
irm https://raw.githubusercontent.com/bradtraversy/agent-context-monitor/main/install.ps1 | iex
```

Both download the latest release for your system and install it. Run the same command again to update. On Linux it uses the `.deb` or `.rpm` when your system has apt or dnf, otherwise the AppImage in `~/.local/bin`.

Or download the installer yourself from the [latest release](https://github.com/bradtraversy/agent-context-monitor/releases/latest).

- macOS: the `.dmg`. Works on Apple Silicon and Intel, macOS 11 or later.
- Windows: the `-setup.exe` or the `.msi`. If Windows shows "Windows protected your PC", click More info, then Run anyway.
- Linux: the `.deb` (Ubuntu, Debian), the `.rpm` (Fedora), or the `.AppImage` (any distro, make it executable and run it).

You need at least one of them installed and logged in. The app picks them up on its own, there's nothing to set up.

## What it shows

- Usage: how much of your current session and weekly limit is left for Claude and Codex, how much of the month's allowance is left for Copilot and Cursor, and when each one resets. Agents you don't have installed don't show up.
- Context: every Claude, Codex, and OpenCode chat you used in the last 24 hours, with how much of its context window is left. Filter by provider, or set your own context limit (say 300K) to measure chats against.
- History: daily and weekly bars of how much of your weekly (or, for Copilot and Cursor, monthly) limit you used.

Widen the window, or click the arrow button in the top corner, to see everything side by side.

![Wide view with usage, context, and history side by side](docs/screenshots/wide.png)

The tray dot turns amber at 70% used and red at 90%, and you get a desktop alert when a limit or an active chat is close to running out. Closing the window keeps the app in the tray. Turn on Start at login from the tray menu.

On Linux, use the tray menu to open the window or quit. GNOME needs the AppIndicator extension, which Ubuntu ships with.

## Privacy

Everything runs on your computer, read-only. The app reads chat metadata (title, model, token counts) from Claude Code, Codex, and OpenCode's local files, never message content. The only network calls are the usage checks, each a read-only request every few minutes using a login already saved on your computer: Anthropic for Claude, Codex's own check through the installed Codex, GitHub for Copilot, and Cursor for Cursor. Your login token is never logged or stored by the app. For the History tab it keeps a small log of usage percentages in its data folder, trimmed to 120 days.

<details>
<summary>Where the data comes from</summary>

- Claude usage: the login Claude Code saves in `~/.claude/.credentials.json` (or `$CLAUDE_CONFIG_DIR`), or the Keychain on macOS, used for one read-only usage request every four minutes. If the login is missing or expired, Claude usage shows as unknown.
- Codex usage: the installed Codex's read-only rate limit check (`codex app-server`), the same source as its /status screen, plus the newest `token_count` record in `~/.codex/sessions` (or `$CODEX_HOME/sessions`). Whichever is newer wins. The app finds Codex on the PATH, in the usual install folders, and in the Codex desktop app.
- Claude chats: open chats from `~/.claude/sessions`, recent ones from transcripts in `~/.claude/projects`, and the Claude desktop app's own chat list so reopened and archived chats are handled the way the app shows them. Claude transcripts don't record the context window, so it's estimated from the model (1M for `[1m]` models or chats already past 200K, otherwise 200K) and marked estimated.
- Codex chats: `~/.codex/sessions`, titled from `session_index.jsonl`.
- Copilot usage: GitHub's Copilot usage request (`copilot_internal/user`, the one Copilot's own apps use), signed in with a saved GitHub login: Copilot in JetBrains, Neovim, or Xcode (`~/.config/github-copilot`), the GitHub CLI (`gh auth token`), or Copilot CLI's own login. Shows premium requests left this month and when they reset. Copilot CLI doesn't save context sizes in its session files, so Copilot chats aren't listed.
- Cursor usage: the request Cursor's dashboard makes (`cursor.com/api/usage-summary`), signed in with the login Cursor saves in its settings database (read-only) or the Cursor CLI's `auth.json`. Shows the month's included usage, split into Auto and Composer and other models, and when the billing cycle resets.
- OpenCode chats: OpenCode's database (`~/.local/share/opencode/opencode.db`), read-only, with each model's context window from OpenCode's own model list (`~/.cache/opencode/models.json`). OpenCode runs on your other logins and has no limits of its own, so it has no usage card.

Readings older than five minutes are greyed out and marked as possibly out of date, and they don't color the tray.

</details>

## Roadmap

1. A "runs out at" estimate for session and weekly usage
2. Usage broken down by project
3. Click a chat to open its folder or copy its resume command
4. More providers, such as Google Antigravity and Gemini CLI
5. A keyboard shortcut to open the window

## Develop

Requires Rust, Node, and the [Tauri prerequisites](https://tauri.app/start/prerequisites/).

```bash
npm install
npm run tauri dev
```

Run the tests with `cargo test --manifest-path src-tauri/Cargo.toml` and build installers with `npm run tauri build`. Release steps are in [docs/RELEASING.md](docs/RELEASING.md).

## License

MIT
