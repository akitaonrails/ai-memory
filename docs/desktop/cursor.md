# Cursor desktop (research notes)

> Added to the RFC #878 research on 2026-10-08. Verified against Cursor's
> official docs and the local install (`cursor-bin` 3.24.9 from the AUR,
> Electron 42; `cursor-agent` CLI 2026.10.01). Closed source.

## Product shape

A VS Code-derived editor whose agent ("Agent Chat"/Composer) runs inside the
app through bundled extensions (`cursor-agent-exec`, `cursor-agent-host`).
The same agent is available in a terminal as `cursor agent` (from the
desktop's `cursor` launcher) and as the standalone `cursor-agent` CLI.
Config and agent data live in `~/.cursor/` (`projects/<workspace>/`,
`plugins/`, `skills-cursor/`, `cli-config.json`); the Electron profile is
`~/.config/Cursor/`.

ai-memory already supports Cursor: `install-mcp --client cursor`
(`~/.cursor/mcp.json` or `.cursor/mcp.json`) and `install-hooks --agent
cursor` (`~/.cursor/hooks.json`, flat `{"version": 1, "hooks": {...}}`
schema) — see `docs/mcp-install.md#cursor`.

## Lifecycle hooks — which files run where

Per <https://cursor.com/docs/hooks> (fetched 2026-10-08):

- Sources, highest priority first: enterprise
  (`/etc/cursor/hooks.json` on Linux), team (dashboard, Enterprise only),
  project `.cursor/hooks.json`, user `~/.cursor/hooks.json`.
- The desktop agent runs all four levels; the CLI runs project and user
  hooks; cloud/background agents run project, team and enterprise hooks
  (not user hooks) and defer `sessionStart`/`sessionEnd`.
- Events: `sessionStart`, `sessionEnd`, `beforeSubmitPrompt`, `preToolUse`,
  `postToolUse`, `postToolUseFailure`, shell, MCP and file before/after
  pairs, `subagentStart`/`subagentStop`, `preCompact`, `stop`,
  `afterAgentResponse`, `afterAgentThought`, tab hooks and `workspaceOpen`.
  Base payload: `conversation_id`, `generation_id`, `model`, `model_id`,
  `workspace_roots[]`, `user_email`, `transcript_path`, `hook_event_name`,
  `cursor_version`.
- Command hooks fail open (exit 2 blocks; `failClosed: true` opts out).
  `sessionStart` and `sessionEnd` are fire-and-forget; `sessionEnd` reports
  `completed`, `aborted`, `error`, `window_close` or `user_close`.

**Claude Code hooks run too, by default.** Per
<https://cursor.com/docs/reference/third-party-hooks>, Cursor loads
`.claude/settings.local.json`, `.claude/settings.json` and
`~/.claude/settings.json` (setting: Agents → Third-Party Imports, on by
default), maps `PreToolUse`, `PostToolUse`, `UserPromptSubmit`
(→ `beforeSubmitPrompt`), `Stop`, `SubagentStop`, `SessionStart`,
`SessionEnd` and `PreCompact`, and runs every matching hook from every
source. The page does not say which surfaces load them.

**Verified live:** the running desktop app executed the operator's
`~/.claude/settings.json` ai-memory `SessionStart` hook at launch, before
any conversation, with Cursor's payload rather than Claude's
(`conversation_id` and `session_id` both `"empty-state-draft"`,
`composer_mode: "agent"`, `is_background_agent: false`, `cursor_version:
"3.24.9"`, empty `workspace_roots`, null `transcript_path` and
`user_email`). The hook ran as `--agent claude-code` with no `cwd`; the
server relabels a payload carrying `cursor_version` as `cursor`
(`agent_from_payload` in `crates/ai-memory-hooks/src/payload.rs`), and the
#721 de-duplication drops this copy only when Cursor's own ai-memory hooks
are also installed.

## MCP

`~/.cursor/mcp.json` (user) and `.cursor/mcp.json` (project); `url` for
HTTP/SSE, `command`/`args` for stdio (<https://cursor.com/docs/mcp>). In
the empty window the app loaded only its built-in servers
(`cursor-app-control`, `cursor-ide-browser`, `cursor-origin`,
`cursor-subscriptions`), not the `ai-memory` and `blender` servers in the
operator's `~/.claude.json`: the third-party import brought the Claude
hooks in but, at least there, not the Claude MCP servers.

## Open questions (live tests)

1. The payload and event set of a real desktop conversation for both the
   native `.cursor/hooks.json` and the Claude-format hooks, including what
   `session_id`/`cwd` the Claude-format copy gets once a workspace is open.
2. ~~What the `empty-state-draft` session start does on the server.~~
   Measured 2026-10-08 by replaying the spooled event against a scratch
   server: it creates one `cursor` session with no `cwd` in the
   `default/scratch` project, and every later launch (a new ingest key, the
   same placeholder id) adds another `session-start` observation to that
   same session. Clutter, not loss; whether to drop it is a product call.
3. Where a desktop conversation's transcript lands (`transcript_path` once
   set) and whether the `cursor-agent` CLI loads the Claude-format hooks
   (it needs `cursor-agent login` here first).
4. Whether a workspace window imports Claude MCP servers.
