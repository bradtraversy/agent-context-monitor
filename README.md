# Agent Context Monitor

A small tray app that shows how much Claude Code and Codex usage you have left this session and this week, and how much context each recent chat has left. Everything runs locally and read-only.

## What it reads

- Codex: when no Codex reading is newer than four minutes, the app asks the installed Codex for live usage (`codex app-server`, read-only `account/rateLimits/read`), the same source as its /status screen. Codex uses its own login. The newest `token_count` record in `~/.codex/sessions` (or `$CODEX_HOME/sessions`) is used too, and whichever reading is newer wins. Windows are identified by length, so a plan with only a weekly limit shows "Not reported" for the session limit. Chat titles come from `session_index.jsonl`.
- Claude: the login Claude Code saves in `~/.claude/.credentials.json` (or `$CLAUDE_CONFIG_DIR`), used for one read-only usage request to Anthropic every four minutes. Apart from Codex's own usage check, this is the app's only network call. The token is never logged or stored. If the login is missing or expired, Claude usage shows as unknown.

- Claude chats: Claude Code's live session registry in `~/.claude/sessions` lists open chats with their name and busy or idle state. Closed chats come from transcripts in `~/.claude/projects` written in the last 24 hours, titled from the name Claude saved. Context tokens come from the last reply in each transcript. Transcripts do not record the context window, so it is estimated from the model name (1M for Opus and Sonnet 5.x or any `[1m]` model, 200k for Haiku) and marked estimated. Unknown models show a token count only.

The Context tab shows Claude and Codex chats used in the last 24 hours, most recent first. A chat drops off after a quiet day and comes back as soon as it is used again. Only chats used in the last 30 minutes color the tray. The Context limit picker measures every chat against your own limit, such as 300K, instead of the model's window when it is smaller. It is saved in the app's config folder.

Only usage and session metadata are kept. For the History tab, each fresh usage reading that differs from the last one (time, session and weekly percent, weekly reset time) is appended to `usage-history.jsonl` in the app's data folder, and readings older than 120 days are dropped at startup. Daily bars show how much of the weekly limit was used that day; weekly bars show the highest weekly usage reached before each reset.

No usage reading should be more than five minutes old. Older ones are greyed out and labeled as possibly out of date, and they don't color the tray. A new provider must refresh usage from a live source within that limit, using as little as it can, with local files only as a fallback, so resets show up even when the agent hasn't been used. Message content is never stored or displayed.

## Tray

The tray dot is grey with no data, green when fine, amber at 70% used, and red at 90% used, across all limits and chats. A desktop alert pops up once when a chat used in the last 30 minutes reaches 90% of its context (or your limit), and once per window when session or weekly usage passes 90%. Closing the window keeps the app in the tray. On Linux, open the window or quit from the tray menu. GNOME needs the AppIndicator extension, which Ubuntu ships by default.

## Roadmap

1. Desktop alerts when a chat nears its context limit or usage is nearly gone
2. A "runs out at" estimate for session and weekly usage
3. A usage history chart
4. Usage broken down by project
5. Click a chat to open its folder or copy its resume command
6. More providers, such as Gemini CLI, Cursor, and Copilot
7. A keyboard shortcut to open the window

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
