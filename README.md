# Agent Context Monitor

A small tray app that shows how much Claude Code and Codex usage you have left this session and this week, and how much context each recent chat has left. Everything runs locally and read-only.

## What it reads

- Codex: when no Codex reading is newer than four minutes, the app asks the installed Codex for live usage (`codex app-server`, read-only `account/rateLimits/read`), the same source as its /status screen. Codex uses its own login. The app looks for Codex on the PATH, in the usual install folders (Homebrew, npm, nvm, `~/.local/bin`), and in the Codex desktop app. The newest `token_count` record in `~/.codex/sessions` (or `$CODEX_HOME/sessions`) is used too, and whichever reading is newer wins. Windows are identified by length, so a plan with only a weekly limit shows "Not reported" for the session limit. Chat titles come from `session_index.jsonl`.
- Claude: the login Claude Code saves in `~/.claude/.credentials.json` (or `$CLAUDE_CONFIG_DIR`), or in the Keychain on macOS, used for one read-only usage request to Anthropic every four minutes. Apart from Codex's own usage check, this is the app's only network call. The token is never logged or stored. If the login is missing or expired, Claude usage shows as unknown.

- Claude chats: Claude Code's live session registry in `~/.claude/sessions` lists open chats with their name and busy or idle state. Closed chats come from transcripts in `~/.claude/projects` written in the last 24 hours, titled from the name Claude saved. The desktop app starts a new session each time a chat is reopened, so older sessions with the same title and folder are hidden behind the current one. Context tokens come from the last reply in each transcript. Transcripts do not record the context window, so it is estimated from the model name and marked estimated: 1M when Claude Code marks the model with `[1m]` or the chat has already gone past 200k, otherwise 200k. Models that aren't Claude show a token count only.

The Context tab shows Claude and Codex chats used in the last 24 hours, most recent first. A chat drops off after a quiet day and comes back as soon as it is used again. Only chats used in the last 30 minutes color the tray. The Context limit picker measures every chat against your own limit, such as 300K, instead of the model's window when it is smaller. It is saved in the app's config folder.

Only usage and session metadata are kept. For the History tab, each fresh usage reading that differs from the last one (time, session and weekly percent, weekly reset time) is appended to `usage-history.jsonl` in the app's data folder, and readings older than 120 days are dropped at startup. Daily bars show how much of the weekly limit was used that day; weekly bars show the highest weekly usage reached before each reset.

No usage reading should be more than five minutes old. Older ones are greyed out and labeled as possibly out of date, and they don't color the tray. A new provider must refresh usage from a live source within that limit, using as little as it can, with local files only as a fallback, so resets show up even when the agent hasn't been used. Message content is never stored or displayed.

## Tray

The tray dot is grey with no data, green when fine, amber at 70% used, and red at 90% used, across all limits and chats. A desktop alert pops up once when a chat used in the last 30 minutes reaches 90% of its context (or your limit), and once per window when session or weekly usage passes 90%. Sent alerts are remembered, so restarting the app doesn't repeat them. Closing the window keeps the app in the tray, and opening the app again just brings the window back. Turn on Start at login from the tray menu. On Linux, open the window or quit from the tray menu. GNOME needs the AppIndicator extension, which Ubuntu ships by default.

## Roadmap

1. A "runs out at" estimate for session and weekly usage
2. Usage broken down by project
3. Click a chat to open its folder or copy its resume command
4. More providers, such as Gemini CLI, Cursor, and Copilot
5. A keyboard shortcut to open the window

## Install

Download the installer for your system from the latest release:

- macOS: the `.dmg` (works on Apple Silicon and Intel). It's signed and notarized by Apple.
- Windows: the `-setup.exe` or the `.msi`. Both are signed. While the certificate is new, Windows may still show "Windows protected your PC" for a while. Click More info, then Run anyway.
- Linux: the `.deb` (Ubuntu, Debian), the `.rpm` (Fedora), or the `.AppImage` (any distro, make it executable and run it).

## Release

Bump `version` in `src-tauri/tauri.conf.json`, then push a matching tag:

```bash
git tag v0.1.0
git push origin v0.1.0
```

The Installers workflow builds on all three OSes and creates a draft release with the files attached. Check it over and publish it on GitHub.

Signing turns on once these repository secrets are set (Settings, Secrets and variables, Actions). Without them the builds still work, unsigned.

- macOS: `APPLE_CERTIFICATE` (base64 Developer ID Application `.p12`), `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`, and for notarizing `APPLE_ID`, `APPLE_PASSWORD` (an app-specific password), `APPLE_TEAM_ID`
- Windows (Azure Artifact Signing, account `bradtraversy-signing`, profile `bradtraversy-public`): `AZURE_CLIENT_ID`, `AZURE_TENANT_ID`, `AZURE_SUBSCRIPTION_ID`. There's no password: the Azure app trusts this repo's `release` environment through a federated credential.

## Develop

Requires Rust, Node, and the [Tauri prerequisites](https://tauri.app/start/prerequisites/).

```bash
npm install
npm run tauri dev
```

Tests: `cargo test --manifest-path src-tauri/Cargo.toml`

Build: `npm run tauri build`

## License

MIT
