# Antigravity IDE (research notes)

> Research for RFC #878. Verified against official docs at
> antigravity.google and the local install. The IDE is closed source
> (Windsurf/VS Code fork); its agent stack shares `~/.gemini` with the
> Antigravity CLI (successor to Gemini CLI).

## Product shape (as of 2026-10)

Google ships Antigravity 2.0 (the desktop agent app), the Antigravity
IDE (standalone Windsurf-derived editor), the Antigravity CLI (`agy`),
and an SDK, all documented at <https://antigravity.google/docs/>. The local
machine runs the IDE (`/usr/share/applications/antigravity-ide.desktop`,
`antigravity-ide-url-handler.desktop`, Electron/VS Code profile at
`~/.config/Antigravity IDE/`, where `User/globalStorage/state.vscdb`,
`workspaceStorage` and `languagepacks.json` confirm the VS Code fork layout).
`~/.gemini/` exists locally with `antigravity-cli/` and `antigravity-ide/`
subdirs plus `config/`, `settings.json`, `oauth_creds.json`.

## MCP client support — yes, store-backed and file-configurable

Official: <https://antigravity.google/docs/mcp/>

- Global config: **`~/.gemini/config/mcp_config.json`**; workspace config:
  **`.agents/mcp_config.json`** in the project root (same file for 2.0, CLI,
  and IDE).
- Schema: `mcpServers` object; stdio entries use `command`/`args`/`env`/
  `cwd`; remote entries use **`serverUrl`** (not `url`/`httpUrl`) +
  `headers`/`authProviderType`/`oauth`. Optional per-server `disabled`,
  `disabledTools`.
- Auth: Google ADC (`authProviderType: "google_credentials"`), OAuth (DCR
  automatic, or manual `clientId`/`clientSecret` with redirect
  `https://antigravity.google/oauth-callback`), tokens at
  `~/.gemini/antigravity/mcp_oauth_tokens.json`.
- IDE GUI: agent side panel → … → **MCP Servers** (MCP Store + "View raw
  config" edits the same `mcp_config.json`). CLI: `/mcp` overlay.
- Permission model keys: `mcp(server/tool)`, `mcp(server/*)`, `mcp(*)`.
- ai-memory status today: `install-mcp --client antigravity-cli` writes this
  file (repo `docs/mcp-install.md`). An HTTP ai-memory server fits the
  `serverUrl` entry shape directly.

## Lifecycle hooks — YES (the standout alongside Claude/Codex/Cursor)

Official: <https://antigravity.google/docs/hooks/>

- Config: workspace **`.agents/hooks.json`** or global
  **`~/.gemini/config/hooks.json`** (both read by 2.0, CLI, and IDE);
  plugin-bundled hooks also supported.
- Events: `PreToolUse`, `PostToolUse` (matcher = tool name regex:
  `run_command`, `view_file`, `write_to_file`, `replace_file_content`,
  `grep_search`, `invoke_subagent`, …), `PreInvocation`, `PostInvocation`
  (around each model call), `Stop`.
- Payload (stdin JSON, camelCase): `conversationId`, `workspacePaths`,
  **`transcriptPath`**, `artifactDirectoryPath`, `modelName`, plus
  `toolCall`, `stepIdx`, etc. Handlers are shell commands with a `timeout`
  (default 30s).
- The payload names the transcript file:
  `<app_data_dir>/brain/<conversationId>/.system_generated/logs/transcript.jsonl`
  where `<app_data_dir>` = `~/.gemini/antigravity` (2.0),
  `~/.gemini/antigravity-cli` (CLI), **`~/.gemini/antigravity-ide` (IDE)**.

**Consequence for ai-memory:** `install-hooks --agent antigravity-cli` writes
`~/.gemini/config/hooks.json`, which the IDE also loads, so global
capture likely already extends to IDE sessions, with the IDE distinguishable
by its `transcriptPath` prefix (`antigravity-ide/brain/...`). Differences to
handle: no `SessionStart`/`SessionEnd` events (session boundaries must be
inferred from PreInvocation/Stop), and `cwd` comes as `workspacePaths[]`
rather than a single `cwd`. Needs a live verification pass on the IDE.

**Verified with the CLI (`agy` 1.3.0, 2026-10-08; see
`verification-2026-10.md`):** a workspace `.agents/hooks.json` runs with no
trust prompt; `invocationNum` (and `executionNum`) restart at 0 on every
user turn of one conversation, so `invocationNum == 0` marks a turn, not a
new conversation; a `PreToolUse` reply without a `decision` denies the tool;
`PostToolUse` carries the call but not its output; `transcriptPath` names
`transcript_full.jsonl`, with `transcript.jsonl` beside it. The IDE has not
been verified: its agent has never run on this machine.

## Transcript store (local, readable)

`~/.gemini/antigravity-ide/brain/<conversationId>/` per conversation:
`transcript.jsonl` + artifacts (per hooks doc). The CLI writes both
`transcript.jsonl` and `transcript_full.jsonl` under
`brain/<conversationId>/.system_generated/logs/`, each opening with a
`USER_INPUT` step that holds the prompt, and keeps per-conversation state in
`conversations/<conversationId>.db` (verified 2026-10-08). A watcher/poll importer over
`brain/*/` is therefore possible as a fallback capture path.

## Plugin/extension surface

Docs list Skills, Rules, custom subagents, hooks, **Sidecars**, a
Marketplace, and an SDK; IDE extensions for VS Code/JetBrains/Zed/Xcode also
exist. (Not yet researched in depth; the marketplace/plugin format was not
needed for capture conclusions.)

## Open-source status

- Antigravity IDE: **closed** (Windsurf/VS Code-derived binary).
- Antigravity CLI (`agy`): closed binary per repo docs ("built in Go",
  per ai-memory's `docs/mcp-install.md`); Gemini CLI
  (`google-gemini/gemini-cli`) is the open ancestor, and a "GCLI migration"
  doc exists. Not cloned (nothing directly informative beyond docs).
- SDK: documented Python SDK (<https://antigravity.google/docs/sdk/overview/>).

## Sources

- <https://antigravity.google/docs/mcp/>, <https://antigravity.google/docs/hooks/>,
  <https://antigravity.google/docs/cli/overview/>
- Local: `~/.config/Antigravity IDE/` (VS Code-fork layout,
  `User/globalStorage/state.vscdb`; no MCP keys configured yet),
  `~/.gemini/` (config root shared by CLI + IDE),
  `/usr/share/applications/antigravity-ide*.desktop`
- ai-memory repo: `docs/mcp-install.md` (Antigravity CLI section)
