# Desktop landscape — apps × capabilities (research matrix)

> Research for RFC #878 (2026-10-07). Every row is sourced in the per-app
> note (`<app>.md`). "Local verified" = confirmed from files on the research
> machine; otherwise the official-doc URL is the source.

| App | MCP client | Remote HTTP MCP | Lifecycle hooks | Local transcript store | Plugin/extension API | Open source | Session bridge to CLI |
| --- | --- | --- | --- | --- | --- | --- | --- |
| **Claude Desktop** (Chat+Cowork+Code tabs; Linux beta) | Chat tab: stdio via `claude_desktop_config.json` + GUI connectors. Code tab: full Claude Code MCP (stdio/SSE/HTTP/WS, plugins) | Yes (connectors + Code-tab http) | **Yes for the Code tab** — same `~/.claude/settings.json` hooks the CLI uses (verified from transcripts); none for Chat tab; Cowork unverified (VM on Linux) | Code tab: `~/.claude/projects/<cwd>/<uuid>.jsonl` (local verified). Cowork: own store under `local-agent-mode-sessions/`. Chat: cloud only | Plugins (marketplace, can bundle MCP+hooks; desktop has plugin browser); `.mcpb` desktop extensions — Anthropic's current route for local MCP servers (re-checked 2026-10-08) | App closed; claude-code CLI distributed not-open | **Yes** — `/desktop`, `claude --desktop --resume <id>` (macOS/x64 Windows only), `/resume` inside Desktop; live registry `~/.claude/sessions/<pid>.json` (`entrypoint`, `hostSessionId`, cc-socks messaging socket); desktop record `local_* ↔ cliSessionId` |
| **Codex desktop** (= ChatGPT desktop app, macOS/Windows/Linux) | Yes — `~/.codex/config.toml` `[mcp_servers.*]` (stdio + streamable HTTP + OAuth/CIMD/DCR), Settings GUI, shared with CLI/IDE | Yes (first-class) | **Yes (core)** — `~/.codex/hooks.json`, 12 events, trust review; `codex app-server` (the desktop's engine) fires trusted hooks, verified; one in-app prompt pending | `~/.codex/sessions/**/rollout-*.jsonl` + `history.jsonl` (local verified) | Plugins (bundle MCP servers + hooks, `PLUGIN_ROOT`); App Server protocol open | Codex core open (`openai/codex`); desktop app closed (Linux via community DMG repack) | None documented |
| **Antigravity IDE** (Google; 2.0 app + CLI `agy` share `~/.gemini`) | Yes — `~/.gemini/config/mcp_config.json` + `.agents/mcp_config.json`; `serverUrl` schema; ADC/OAuth; MCP Store GUI | Yes (`serverUrl` + headers) | **Yes** — `.agents/hooks.json` / `~/.gemini/config/hooks.json` (PreToolUse/PostToolUse/PreInvocation/PostInvocation/Stop; payload includes `conversationId`, `transcriptPath`, `workspacePaths`) | `~/.gemini/antigravity-ide/brain/<conversationId>/**` incl. `transcript.jsonl` (per docs) | Skills/Rules/subagents/sidecars/marketplace + SDK | IDE closed (VS Code/Windsurf fork); CLI closed; Gemini CLI is open ancestor | None documented (IDE vs CLI share config root, distinct `brain/` roots) |
| **ChatGPT desktop (hosted chat side)** | Hosted chat: remote MCP **plugins only** (no local config read) | Yes (remote plugins) | No (Chat-mode) | No local store (cloud) | Plugins (remote MCP + UI + events; OAuth/SIWC) | Closed | n/a |
| **Grok webapp launcher** (local wrapper) | n/a (browser) | unverified | n/a | n/a | n/a | n/a | n/a |
| **Grok Bot** (Cursor; native Electron agent) | Yes — MCP machinery in app.asar (`mcpBoxServers` in `~/.grokbot/settings.json`); format/docs **unverified** | unknown | No | No local transcript documented (cloud + leveldb UI state) | plugins evident (`plugin-logo-cache`) — undocumented | Closed (downloads.cursor.com) | n/a |
| **Zed** | Yes — `context_servers` in settings.json (stdio + url/OAuth); MCP extensions; forwards servers to ACP agents | Yes | No native hooks; ACP/terminal agents bring their own | Native-agent threads in Zed's DB (fragile); terminal/ACP agents use their own stores | Extension API (WASM extensions; MCP extension manifest) | **Open** (`zed-industries/zed`) | n/a (agents run in-place; no bridge needed) |

## Highlights vs RFC #878's assumptions (what changed)

1. **The capture picture improved for two ecosystems:**
   - **Claude Desktop's Code tab is literally Claude Code** — hooks in
     `~/.claude/settings.json` already capture it (`agent=claude-code`;
     a folderless session gets a scratch-workspace cwd). RFC #878 classed
     Claude Desktop wholesale as "no hook/event surface"; that is true of
     the Chat tab, and Cowork is unverified.
   - **Antigravity (IDE included) has first-class lifecycle hooks** with
     `transcriptPath` in every payload — a full auto-capture target the RFC
     didn't evaluate (it listed Cursor as the only hook-bearing desktop
     target).
2. **Codex is now the ChatGPT desktop app** with shared `~/.codex` config:
   one `install-mcp`/`install-hooks` write serves CLI + desktop + IDE
   extension (desktop-app hook firing needs one live verification).
3. **Official Linux desktop builds now exist** for Claude Desktop (beta) and
   the ChatGPT/Codex app — ai-memory's `docs/mcp-install.md` "Linux: not
   officially distributed" note for claude-desktop is stale.
4. **Grok Bot is a Cursor product** (not xAI) with embedded MCP support but
   no public docs — opportunistic, not plannable.
5. Remote/streamable-HTTP MCP is now the norm across Codex/Antigravity/Zed/
   Claude(Code) — a loopback ai-memory HTTP server needs **no npx shims** on
   any researched app except legacy Claude-Desktop-Chat stdio config.

## Ordered capture opportunity (auto-capture without new product code)

1. Claude Desktop Code tab — works today via existing hooks (verified).
2. Codex desktop — its engine fires the existing `~/.codex` hooks once
   trusted (verified); one in-app prompt pending.
3. Antigravity IDE — via existing `~/.gemini/config/hooks.json` (agent
   mapping/`workspacePaths` handling + verification needed).
4. Zed — via ACP/terminal agents' own hooks (nothing to build).
5. Chat tab / hosted ChatGPT / Grok Bot — model-discretion only (MCP tools);
   hosted ChatGPT additionally needs a published plugin to reach a
   user-run server at all.
