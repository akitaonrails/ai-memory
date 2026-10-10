# Zed (research notes)

> Research for RFC #878. Zed is open source (cloned to
> `~/Projects/_desktop-research/zed-research`); docs at zed.dev. Local
> install verified (`~/.config/zed/settings.json`).

## Product shape

Open-source editor (Rust) with an Agent Panel supporting three agent paths
(<https://zed.dev/docs/ai/mcp>):

1. **Zed Agent** (native, built-in)
2. **External Agents** via **ACP** (Agent Client Protocol,
   <https://agentclientprotocol.com/>), e.g. Gemini CLI, Claude Code,
   codex-acp, opencode. Verified locally: the user's
   `~/.config/zed/settings.json` configures `agent_servers`:
   `opencode` (`type: "registry"`), `codex-acp`, `claude-acp`.
3. **Terminal Threads**: native CLIs run in Zed's terminal with their own
   config (so a `claude`/`codex` TUI inside Zed behaves exactly like the
   CLI: ai-memory hooks fire as usual, keyed by cwd).

## MCP client support

- Config: **`context_servers`** in the Zed `settings.json` (Linux
  `~/.config/zed/settings.json`, verified locally; macOS
  `~/Library/Application Support/Zed/settings.json` per Zed docs/ai-memory
  `docs/mcp-install.md`). Entries:
  - local stdio: `{"command", "args", "env"}`
  - remote: `{"url", "headers"}`; OAuth flow prompts when no
    `Authorization` header is configured.
- GUI: Settings → AI → MCP Servers → Add Server (local/remote).
- Extensions: MCP servers can ship as Zed extensions
  (<https://zed.dev/docs/extensions/mcp-extensions>).
- Feature coverage: **Tools + Prompts only** ("Discovery, Sampling,
  Elicitation, etc." not yet supported); handles
  `notifications/tools/list_changed` (same zed.dev/docs/ai/mcp page).
- MCP servers configured in Zed are **forwarded to External Agents over
  ACP**; external agents may also read their own native MCP config
  (configuration boundaries documented at
  <https://zed.dev/docs/ai/external-agents>).
- Tool permissions: `agent.tool_permissions.default`
  (`confirm`/`allow`/`deny`), per-tool rules keyed `mcp:<server>:<tool>`.

ai-memory status today: `install-mcp --client zed` exists (repo
`docs/mcp-install.md`); remote HTTP ai-memory servers fit the `url` entry.

## Lifecycle hooks / capture surface

- Zed itself has no push hooks (no equivalent of Claude Code/Codex/Antigravity
  hook files). Auto-capture must come from the agent running inside Zed:
  - Terminal Threads and ACP external agents (claude-acp / codex-acp /
    opencode) fire their own hooks, and ai-memory captures those sessions
    through the existing per-agent hook integrations (agent kind reported by
    the CLI; session lands in the project keyed by cwd, which Zed sets to
    the workspace dir).
  - The native Zed Agent has no hook surface; its threads are stored in
    Zed's own database (source: `crates/agent`, `crates/agent_ui` in the
    clone; a poll/watch importer would break across Zed versions, so it falls in
    the same bucket as RFC #878 option (c), only if demand appears).

## Session identity / dedup

Zed does not sit in the session path: session ids are whatever the
underlying agent (Zed Agent / ACP server / terminal CLI) mints. For ACP
agents, ai-memory sees the agent's own session id via its hooks; a Zed
ACP-run claude-acp session and a terminal `claude` session in the same repo
are two sessions in the same project, and cwd-keyed project scope already
merges them correctly. No Zed-specific dedup work identified.

## Open-source status

Open source: github.com/zed-industries/zed (cloned, shallow, for crate
layout: `crates/agent`, `crates/agent_servers`, `crates/agent_settings`,
`crates/language_extension` etc.).

## Sources

- <https://zed.dev/docs/ai/mcp>, <https://zed.dev/docs/ai/external-agents>,
  <https://zed.dev/docs/extensions/mcp-extensions>,
  <https://agentclientprotocol.com/>
- Local: `~/.config/zed/settings.json` (`agent_servers` with opencode /
  codex-acp / claude-acp), `/usr/share/applications/dev.zed.Zed.desktop`
- Clone: `~/Projects/_desktop-research/zed-research`
