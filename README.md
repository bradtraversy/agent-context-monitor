# Agent Context Monitor

A small tray app that shows how much Claude Code and Codex usage you have left this session and this week, and how much context each active chat has left. Everything runs locally and read-only.

## What it reads

- Codex: `~/.codex/sessions` (or `$CODEX_HOME/sessions`). Usage limits come from the newest `token_count` record. Windows are identified by length, so a plan with only a weekly limit shows "Not reported" for the session limit. Chat titles come from `session_index.jsonl`.
- Claude: the login Claude Code saves in `~/.claude/.credentials.json` (or `$CLAUDE_CONFIG_DIR`), used for one read-only usage request to Anthropic every five minutes. This is the app's only network call. The token is never logged or stored. If the login is missing or expired, Claude usage shows as unknown.

- Claude chats: Claude Code's live session registry in `~/.claude/sessions` lists open chats with their name and busy or idle state. Context tokens come from the last reply in each transcript. Transcripts do not record the context window, so it is estimated from the model name (1M for Opus and Sonnet 5.x or any `[1m]` model, 200k for Haiku) and marked estimated. Unknown models show a token count only.

Only usage and session metadata are kept. Message content is never stored or displayed.

## Tray

The tray dot is grey with no data, green when fine, amber at 70% used, and red at 90% used, across all limits and chats. Closing the window keeps the app in the tray. On Linux, open the window or quit from the tray menu. GNOME needs the AppIndicator extension, which Ubuntu ships by default.

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
