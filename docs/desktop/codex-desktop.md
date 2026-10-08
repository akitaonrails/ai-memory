# Codex desktop (the ChatGPT desktop app) (research notes)

> Research for RFC #878. Verified against official docs, the open-source CLI
> repo (cloned to `~/Projects/_desktop-research/codex-research`), the Linux
> repackaging project (cloned to
> `~/Projects/_desktop-research/codex-desktop-linux-research`), and the local
> install.

## Product shape (as of 2026-10)

OpenAI has merged Codex into the **ChatGPT desktop app**: "Download ChatGPT
for macOS, Windows, or Linux… Choose ChatGPT or Codex"
(<https://developers.openai.com/codex/app>). Officially supported on
**macOS, Windows, and Linux** (docs at
<https://developers.openai.com/codex/linux/linux-app>). The app is Electron;
the local machine runs the community Linux repackaging
`codex-desktop-linux` (AUR `codex-desktop-linux` / originally
`ilysenko/codex-desktop-linux`, maintained here under the `distsystem` fork)
which **converts the upstream macOS `Codex.dmg` into Linux .deb/.rpm/pacman
packages** — i.e. the app itself is closed; the wrapper is open.

Local artifacts: `/usr/share/applications/codex-desktop.desktop`
(`Exec=... /usr/bin/codex-desktop`), Electron profiles at `~/.config/Codex`
(release) and `~/.config/Codex (Dev)` (dev build), `~/.config/codex-desktop`
only holds `electron-flags.conf`.

The desktop app drives the **same Codex core** as the CLI — the open-source
repo contains `codex-rs/app-server`, `app-server-protocol`,
`app-server-daemon`, `app-server-transport` crates
(`~/Projects/_desktop-research/codex-research/codex-rs/`), and docs at
<https://developers.openai.com/codex/app-server> describe the App Server
protocol; <https://developers.openai.com/siwc/token-sharing-open-source/codex-app-server>
documents running your own app-server against the ChatGPT plan.

## MCP client support — shared config with the CLI

"The ChatGPT desktop app, Codex CLI, and IDE extension support MCP servers
and **share MCP configuration for the same Codex host**"
(<https://developers.openai.com/codex/extend/mcp>):

- Config file: `~/.codex/config.toml`, `[mcp_servers.<name>]` tables;
  project scope `.codex/config.toml` (trusted projects only). **Verified
  locally**: `~/.codex/config.toml` already contains
  `[mcp_servers.ai-memory] url = "http://192.168.0.90:49374/mcp"` (remote
  streamable HTTP — works with the desktop app, no Node/npx needed).
- Desktop app GUI: Settings → MCP servers → Add server (STDIO or Streamable
  HTTP); `/mcp` in the composer lists servers.
- CLI: `codex mcp add <name> -- <command>` / `codex mcp list`.
- Transports: stdio (command/args/env/cwd/env_vars) and streamable HTTP
  (`url`, `bearer_token_env_var`, `http_headers`, `env_http_headers`,
  `http_headers_helper`), OAuth incl. CIMD/DCR and ChatGPT session auth.
  ai-memory's loopback HTTP + optional bearer token maps directly onto this.
- Server `instructions` field is read as server-wide guidance (keep first
  512 chars self-contained) — relevant for how ai-memory's tool-routing
  guidance is surfaced in Codex.

## Lifecycle hooks — full Claude-Code-style model, in core

Codex has a native lifecycle-hooks framework
(<https://developers.openai.com/codex/hooks>), confirmed in source
(`codex-rs/hooks/`, events `user_prompt_submit.rs`, `declarations.rs`):

- Events: `SessionStart`, `SessionEnd`, `UserPromptSubmit`, `PreToolUse`,
  `PostToolUse`, `PermissionRequest`, `PreCompact`, `PostCompact`,
  `SubagentStart`, `SubagentStop`, `Stop`, `Interrupt`.
- Config: `~/.codex/hooks.json` (or inline `[hooks]` in `config.toml`),
  repo `<repo>/.codex/hooks.json`, plugin-bundled hooks. **Verified
  locally**: ai-memory already writes `~/.codex/hooks.json` with
  SessionStart/UserPromptSubmit/PreToolUse/PostToolUse command hooks calling
  `ai-memory hook --event ... --agent codex --server-url ...`.
- Hook stdin includes `session_id`, `transcript_path`, `cwd`,
  `hook_event_name`, `model` — everything ai-memory's capture needs.
- Trust review: non-managed hooks must be trusted once (`/hooks` in the
  TUI); trust is recorded against the hook's hash.
- Handlers can be `command` or `mcp_tool` (a lifecycle event can call a tool
  on a connected MCP server — an interesting future capture path).

**Verified 2026-10-08 (see `verification-2026-10.md`):** the desktop app
spawns `codex app-server` from the `codex` on `PATH` with
`CODEX_HOME=~/.codex`, and app-server runs the same hooks as the TUI once
they are trusted. Untrusted or modified hooks are skipped without any
message, and trust is recorded per hook content (event, matcher, handler),
so changing an installed hook command needs a fresh trust. `SessionEnd`
fires when a thread is archived or app-server shuts down cleanly, not when
a client unsubscribes. The hook payload does not say which client started
the session; the rollout's `session_meta.originator` does. A desktop-app
prompt with the ai-memory hooks in place has not been run yet.

## Transcript store

`~/.codex/sessions/YYYY/MM/DD/rollout-*-<session-uuid>.jsonl` (verified
locally), plus `~/.codex/history.jsonl` (prompt history keyed by
`session_id`). Same store for CLI and (expected) desktop sessions — the
desktop app is the same Codex host.

## Session bridging

No CLI↔desktop bridge equivalent to Claude's `/desktop` is documented.
CLI and desktop are separate conversation lists over the same host config;
ChatGPT web/mobile/cloud surfaces are cloud-side and do not read local
config ("ChatGPT web doesn't read local Codex configuration files",
<https://developers.openai.com/codex/extend/mcp> — web section). `dots`
(persistent agents) are cloud-orchestrated; enterprise hooks for dots are
remote MCP hooks, not local command hooks.

## Open-source status

- `openai/codex` (Apache-2.0): CLI + `codex-rs` core, **hooks crate,
  codex-mcp, app-server crates** — open.
- The ChatGPT/Codex **desktop app**: closed (Electron); Linux build only via
  the community DMG-conversion project (`ilysenko/codex-desktop-linux`,
  packaged here by `distsystem`).

## Sources

- <https://developers.openai.com/codex/app>,
  <https://developers.openai.com/codex/extend/mcp>,
  <https://developers.openai.com/codex/hooks>,
  <https://developers.openai.com/codex/linux/linux-app>,
  <https://developers.openai.com/codex/app-server>
- Clones: `~/Projects/_desktop-research/codex-research` (openai/codex @
  5a31401, 2026-10-07), `~/Projects/_desktop-research/codex-desktop-linux-research`
- Local: `~/.codex/{config.toml,hooks.json,sessions/,history.jsonl}`,
  `~/.config/Codex/`, `~/.config/'Codex (Dev)'/`,
  `/usr/share/applications/codex-desktop.desktop` (pacman
  `codex-desktop-linux 26.707.31428`)
