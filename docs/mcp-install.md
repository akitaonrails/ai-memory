# MCP install guide - additional clients

> All snippets below default to `http://127.0.0.1:49374` (local server). For a
> remote server (homelab, LAN box) substitute the appropriate URL AND add an
> `Authorization: Bearer <token>` header to the `headers` block when bearer auth
> is enabled. The MCP wire protocol expects the `/mcp` path suffix on the URL.

> **Transport is stateless by default.** Since v0.1.2 the HTTP transport
> answers each request independently (plain JSON, no `Mcp-Session-Id`
> required), so any client that points a remote URL at `/mcp` — including
> OpenCode `type: "remote"` and plain `curl` — works without an
> `mcp-remote` stdio shim (issue #3). The `mcp-remote` bridge is still
> needed for **Claude Desktop** specifically, because its config only
> supports stdio servers — not because of session state. If you run a
> client that *requires* MCP session continuity or server-initiated SSE
> streams, start the server with `ai-memory serve --transport http
> --http-stateful` to restore rmcp's session mode.

This page documents how to register ai-memory as an MCP server with
agent CLIs beyond the README quick start.

The hook-capable clients in the [README Support Matrix](../README.md#support-matrix)
have automatic capture integrations (host-native commands for supported local
profiles, plus generated TypeScript plugin/extension files for OpenClaw,
OpenCode, OMP, and Pi).
Claude Code may use its supported Windows exec form; other agents use native
single command strings according to their hook schema. PowerShell/Git Bash
script bundles are compatibility fallbacks and do not enforce capture-policy
v1. Grok and Zero capture lifecycle events, but both ignore
SessionStart stdout, so ai-memory does not auto-inject handoffs for them.
SessionStart handoff injection works only for clients that consume startup-hook
stdout (or their equivalent context-injection result); Grok and Zero must call
`memory_handoff_accept` when resuming.

Capture exclusions are separate from MCP registration. Native hook commands and
generated OpenCode/OMP/Pi/OpenClaw integrations enforce `[capture]
ignore_paths`; legacy shell/PowerShell and remote-only/Docker script bundles do
not. Reinstall/refresh an existing hook or plugin to gain it; see
[Capture exclusions](marker-file.md#capture-exclusions).

Claude Desktop, VS Code Copilot, and Zed are **MCP-only** here: they
expose long-term memory to their LLMs via ai-memory's MCP tools
(`memory_query`, `memory_recent`, `memory_handoff_accept`, etc.), but
they do not auto-capture session events into ai-memory's `/hook`
endpoint. The trade-off:

| | What you get | What you don't get |
|---|---|---|
| **MCP only** | LLM can query the wiki, accept handoffs, run memory_consolidate, and run `memory_auto_improve` learning reviews | No automatic session-end summaries; no auto-handoff at session boundaries |
| **MCP + hooks** | All of the above *plus* bounded sanitized prompt/tool-lifecycle observations captured automatically; handoffs surface at SessionStart with no human prompting **only when the client consumes startup-hook output or an equivalent context-injection result** | Hook observations are not complete native transcripts. Grok and Zero discard SessionStart stdout; ask them to call `memory_handoff_accept` when resuming. |

For MCP-only use, you can still cover the session-boundary gap by asking
the LLM to call `memory_handoff_begin` manually before quitting.

For proactive tool use in MCP-capable clients that read project instructions,
also install the managed routing package from
[`docs/usage.md`](usage.md#install-the-routing-snippet-and-agent-skills). The
slim instruction block stays in the agent rules file, while supported Agent
Skills carry the detailed ai-memory tool-routing guidance.

## Custom lifecycle bridges

Built-in integrations should use `ai-memory install-hooks` rather than
calling `/hook` directly. For a third-party bridge that has its own
lifecycle vocabulary, keep the core `event` query param on one of
ai-memory's canonical events when possible:

### Community-maintained Hermes Agent plugin

ai-memory does not currently ship a first-party Hermes Agent installer,
but a community-maintained
[`ai-memory-hermes-plugin`](https://github.com/MrLuciano/ai-memory-hermes-plugin)
is available. Treat it as a third-party bridge: verify the plugin's
documented Hermes and ai-memory version matrix, install/update/uninstall
behavior, platform coverage, and secret handling before enabling it on a
live ai-memory server. In particular, bearer tokens and endpoint settings
should stay in environment or local config references rather than generated
plugin source files.

The hook router does recognize `agent=hermes` as a concrete session kind and
accepts Hermes' documented shell-hook `tool_name` / `tool_input` envelope for
tool-family metadata and capture-exclusion enforcement. A custom bridge should
map `on_session_start`, `post_tool_call`, and `on_session_end` to ai-memory's
canonical `session-start`, `post-tool-use`, and `session-end` event names while
forwarding the original JSON object. This protocol recognition does not install
or trust a third-party plugin. Hermes ignores session-start hook stdout, so it
cannot consume an automatic handoff there; use MCP `memory_handoff_accept`.

The same lifecycle guidance below applies to Hermes or any other external
bridge: map known events onto ai-memory's canonical hook events where
possible, and use extension metadata for source-specific events instead of
expanding ai-memory's stored event enum for one client.

```bash
curl -X POST \
  'http://127.0.0.1:49374/hook?event=user-prompt&agent=other' \
  -H 'content-type: application/json' \
  -d '{"session_id":"sess-123","cwd":"/repo","prompt":"Fix auth"}'
```

If the source event has no canonical equivalent, opt in to extension
metadata instead of asking ai-memory to expand its stored event enum:

```bash
curl -X POST \
  'http://127.0.0.1:49374/hook?event=lead.contact&agent=other&extension=fstech' \
  -H 'content-type: application/json' \
  -d '{"session_id":"sess-123","title":"Lead contacted","message":"Lead Maria requested a proposal"}'
```

With `extension=<namespace>`, unknown events are still stored as the
canonical `other` observation kind, but ai-memory also preserves the
validated source event. You may pass `source_event=<name>` explicitly;
otherwise an unknown `event` value becomes the source event. Both tokens
must be ASCII letters, digits, `.`, `_`, `-`, or `:`; namespaces are
limited to 64 bytes and source-event names to 128 bytes. Unknown events
without `extension` intentionally collapse to `other` with no source
metadata.

> **One-shot tip:** every snippet below is also reachable from the
> CLI:
> ```bash
> ai-memory install-mcp --client gemini-cli   # or cursor / claude-desktop / openclaw / omp / pi / antigravity-cli / grok / kimi-code / kiro-cli / command-code / swival / devin / zero / zcode / vscode-copilot / zed
> ```

---

## Claude Code

**Status:** ✅ Native HTTP MCP supported. ✅ Optional session-aware stdio
bridge supported for concurrent sessions.

The default registration remains a static HTTP entry:

```bash
ai-memory install-mcp --client claude-code --apply
```

Static HTTP config cannot attach the current lifecycle-hook session id, so
`[auto_scope] mode = "per_session"` cannot isolate two concurrent Claude Code
sessions through that entry. Opt into ai-memory's local stdio bridge instead:

```bash
ai-memory install-mcp --client claude-code --session-aware --apply
```

The generated entry runs `ai-memory mcp-bridge`, connects to the same configured
local or remote `/mcp` endpoint, preserves bearer authentication, and adds
`X-Memory-Actor-Session-Id: <CLAUDE_CODE_SESSION_ID>` to every upstream request.
It supports ai-memory's default stateless HTTP mode and opt-in stateful mode.
The command fails closed if Claude did not supply a session id rather than
silently falling back to the shared single slot.

Pair the bridge with this server setting:

```toml
[auto_scope]
mode = "per_session"
```

Claude Code sets `CLAUDE_CODE_SESSION_ID` on stdio MCP subprocesses, but the
subprocess retains the id it was launched with across `/clear`. Also,
`--continue` or `--resume` without an explicit id may expose the startup id
instead of the resumed id. Restart Claude Code after `/clear` when exact
session-key continuity matters, and prefer `--resume <session-id>` over an
implicit resume. These are upstream lifecycle limits; the bridge does not guess
or switch identities behind Claude Code.

`--session-aware` is Claude-Code-only. Other clients keep their documented
native HTTP or generated bridge paths.

---

## Cursor

**Status:** ✅ MCP supported. ✅ Lifecycle hooks supported via
`ai-memory install-hooks --agent cursor --apply`.

**Config file:**
- Per-project: `.cursor/mcp.json` in the workspace root.
- Global: `~/.cursor/mcp.json`.

```json
{
  "mcpServers": {
    "ai-memory": {
      "url": "http://127.0.0.1:49374/mcp"
    }
  }
}
```

**Gotchas:**
- Cursor uses the `url` key for HTTP/SSE transports. Stdio uses
  `command` + `args` instead.
- Cursor hooks live in `~/.cursor/hooks.json` or `.cursor/hooks.json`.
  ai-memory maps `sessionStart`, `sessionEnd`, `beforeSubmitPrompt`,
  `preToolUse`, `postToolUse`, `postToolUseFailure`, `preCompact`, and
  `stop` to the shared capture path.
- Cursor watches `hooks.json` on save. For MCP config changes, restart
  Cursor or toggle the server off+on in **Settings → MCP**.
- Sources: <https://cursor.com/docs/mcp>, <https://cursor.com/docs/hooks.md>

---

## VS Code GitHub Copilot

**Status:** ✅ MCP supported (workspace-default). ❌ No lifecycle hooks
(Copilot's agent mode does not expose `PreToolUse` / `PostToolUse` /
`SessionStart` yet, so ai-memory's automatic capture is not active in
VS Code — call `memory_query`, `memory_write_page`, etc. from chat).

**Config file:**
- Workspace (recommended): `.vscode/mcp.json` in the repo root. Matches
  ai-memory's per-cwd auto-scoping.
- User profile: run **MCP: Open User Configuration** in VS Code and use
  the `mcp.json` file it opens. The exact path is platform- and
  profile-specific; pass it to `--config-file` if you want ai-memory to
  write that file directly.

**Schema (verified against VS Code's MCP reference):** top-level key is
`servers` (NOT `mcpServers`). HTTP endpoints use `type: "http"` and the
`url` field; the bearer token goes into an inline `headers` object.

```json
{
  "servers": {
    "ai-memory": {
      "type": "http",
      "url": "http://127.0.0.1:49374/mcp"
    }
  }
}
```

**With a bearer token** (rendered when `--auth-token` is passed):

```json
{
  "servers": {
    "ai-memory": {
      "type": "http",
      "url": "http://127.0.0.1:49374/mcp",
      "headers": {
        "Authorization": "Bearer <token>"
      }
    }
  }
}
```

**Install command:**

```bash
# Print the snippet:
ai-memory install-mcp --client vscode-copilot

# Or write .vscode/mcp.json in the current workspace directly:
ai-memory install-mcp --client vscode-copilot --apply

# Or write the user-profile mcp.json opened by VS Code directly:
ai-memory install-mcp --client vscode-copilot \
  --config-file /path/to/vscode-profile/mcp.json --apply
```

Aliases: `copilot`, `github-copilot`.

**Gotchas:**
- The top-level key must be `servers`. The `mcpServers` form (used by
  Claude Code / Cursor / Gemini CLI) is silently ignored by VS Code.
- After editing, open the MCP view in the Extensions sidebar and start
  the server (or use **MCP: Show installed servers**). VS Code does not
  auto-reload `.vscode/mcp.json` while the window is focused on another
  tab.
- Copilot Enterprise behaves the same as Copilot Individual/Business
  for MCP — your org may restrict which MCP servers Copilot is allowed
  to call; check **Settings → Copilot → MCP servers** if the server
  shows as blocked.
- Lifecycle hooks aren't possible until VS Code Copilot adds an agent
  hook surface. Until then, the auto-handoff flow that other agents
  enjoy (SessionStart auto-fetches a "where you left off" block) does
  not run here — ask the agent to call `memory_handoff_accept`
  manually if you want it.
- Sources:
  <https://code.visualstudio.com/docs/copilot/customization/mcp-servers>,
  <https://code.visualstudio.com/docs/agents/reference/mcp-configuration>

---

## Zed

**Status:** MCP supported through Zed's native remote context-server
configuration. No lifecycle hooks or managed-workstream adapter.

**Config file:** Zed stores MCP servers in its user `settings.json`:

- macOS: `~/.config/zed/settings.json`
- Linux: `$XDG_CONFIG_HOME/zed/settings.json`, defaulting to
  `~/.config/zed/settings.json`
- Windows: `%APPDATA%\Zed\settings.json`

The server map is the top-level `context_servers` key. Remote servers use a
`url` and may include bearer authentication in `headers`:

```json
{
  "context_servers": {
    "ai-memory": {
      "url": "http://127.0.0.1:49374/mcp",
      "headers": {
        "Authorization": "Bearer <token>"
      }
    }
  }
}
```

Print or apply the configuration with:

```bash
ai-memory install-mcp --client zed
ai-memory install-mcp --client zed --apply \
  --server-url "http://homelab:49374/mcp" \
  --auth-token "$TOKEN"
```

`--apply` preserves JSONC comments, trailing commas, unrelated Zed settings,
and other context servers. Zed can call ai-memory's MCP tools, but it does not
expose compatible session or tool lifecycle hooks. Automatic capture,
automatic handoff injection, and
`ai-memory run` continuity are therefore not available; ask the agent to call
`memory_handoff_begin` before leaving and `memory_handoff_accept` when
resuming when you need manual continuity.

Sources: <https://zed.dev/docs/ai/mcp>,
<https://zed.dev/docs/configuring-zed>.

---

## Claude Desktop

**Status:** ✅ MCP supported (via stdio shim for HTTP). ❌ No lifecycle hooks.

**Config file:**
- macOS: `~/Library/Application Support/Claude/claude_desktop_config.json`
- Windows: `%APPDATA%\Claude\claude_desktop_config.json` for an
  unpackaged install, or
  `%LOCALAPPDATA%\Packages\Claude_<id>\LocalCache\Roaming\Claude\claude_desktop_config.json`
  for a detected MSIX-packaged install. `install-mcp` checks for
  `Claude_*` package directories automatically and prefers one that
  already contains a config. If multiple candidates remain ambiguous,
  it stops and asks for an explicit `--config-file` instead of guessing.
- Linux: not officially distributed by Anthropic. Use Claude Code
  (terminal) instead.

**Important:** Claude Desktop's JSON config supports stdio MCP
servers only. To talk to ai-memory's HTTP endpoint, bridge through
the community [`mcp-remote`](https://www.npmjs.com/package/mcp-remote)
stdio shim. Requires Node.js installed on the same machine.

```json
{
  "mcpServers": {
    "ai-memory": {
      "command": "npx",
      "args": ["-y", "mcp-remote", "http://127.0.0.1:49374/mcp"]
    }
  }
}
```

**Gotchas:**
- After editing the config, **fully quit and relaunch** Claude
  Desktop. "Check for Updates…" is not enough.
- Claude Desktop also has account-level remote custom connectors and
  `.mcpb` desktop extensions. The ai-memory CLI manages the local
  JSON-config path because it works with localhost/LAN servers and does
  not require publishing an HTTPS connector.
- Claude Desktop exposes MCP tools but no lifecycle hooks, so automatic
  prompt/tool capture and session-boundary handoffs are not possible
  unless Anthropic adds a desktop hook/plugin surface.
- If the MCP indicator doesn't appear after restart, check the logs:
  `~/Library/Logs/Claude/mcp*.log` (macOS). On Windows, check
  `%APPDATA%\Claude\logs\` for an unpackaged install or the corresponding
  `LocalCache\Roaming\Claude\logs\` directory under the detected
  `%LOCALAPPDATA%\Packages\Claude_<id>\` package.
- **Windows MSIX packaging:** a packaged Claude Desktop is an
  AppContainer. Windows redirects its `%APPDATA%` writes into an isolated
  `AppData\Local\Packages\Claude_<id>\LocalCache\Roaming\` tree that an
  unpackaged process such as this CLI must address directly.
  `install-mcp --client claude-desktop --apply` detects this and writes
  to the packaged location automatically. On an older ai-memory build,
  pass `--config-file` pointed at the `LocalCache` path directly.
- Sources: <https://support.claude.com/en/articles/10949351-getting-started-with-local-mcp-servers-on-claude-desktop>,
  <https://support.claude.com/en/articles/11175166-how-to-connect-remote-mcp-integrations-to-claude>,
  <https://learn.microsoft.com/en-us/windows/msix/msix-containerization-overview>

---

## Gemini CLI

**Status:** ✅ MCP supported. ✅ Lifecycle hooks supported via
`ai-memory install-hooks --agent gemini-cli --apply`.

**Config file:**
- User: `~/.gemini/settings.json`
- Project: `.gemini/settings.json`

Gemini CLI uses `httpUrl` (not `url`) for streamable-HTTP MCP
endpoints. The `timeout` is in milliseconds.

```json
{
  "mcpServers": {
    "ai-memory": {
      "httpUrl": "http://127.0.0.1:49374/mcp",
      "timeout": 5000
    }
  }
}
```

**Hooks:**

```bash
ai-memory install-hooks --agent gemini-cli --apply
```

Gemini CLI's lifecycle event names differ from Claude Code's, so use
`install-hooks --agent gemini-cli` rather than copying another agent's
settings. ai-memory maps Gemini's `SessionStart`, `SessionEnd`,
`BeforeTool`, `AfterTool`, and `PreCompress` events to the shared hook
capture path; `SessionStart` also fetches pending handoffs.

**Gotchas:**
- Gemini supports stdio too via `command`/`args`, plus SSE via `url`.
  Only `httpUrl` covers streamable HTTP. Don't mix them in one entry.
- Restart the CLI session after changing `~/.gemini/settings.json` so
  both MCP servers and hooks are reloaded.
- Source: <https://github.com/google-gemini/gemini-cli/blob/main/docs/tools/mcp-server.md>

---

## Antigravity CLI (`agy`)

**Status:** ✅ MCP supported. ✅ Lifecycle hooks supported via
`ai-memory install-hooks --agent antigravity-cli --apply`.

**Config file (MCP):** `~/.gemini/config/mcp_config.json`

Antigravity CLI is the successor to Gemini CLI, built in Go with
parallel subagent support. It uses a separate `mcp_config.json`
(instead of Gemini CLI's combined `settings.json`) and uses
`serverUrl` (not `httpUrl`) for streamable-HTTP endpoints.

```bash
# Merge the MCP entry into the Antigravity config:
ai-memory install-mcp --client antigravity-cli --apply
```

The rendered snippet writes to `mcp_config.json` under `mcpServers`:

```json
{
  "mcpServers": {
    "ai-memory": {
      "serverUrl": "http://127.0.0.1:49374/mcp",
      "timeout": 5000
    }
  }
}
```

**Config file (hooks):** `~/.gemini/config/hooks.json`

Antigravity CLI uses a named-groups hook format. Each top-level key
is a hook group name; inside, event arrays map to handlers. Tool
events (`PreToolUse`, `PostToolUse`) use nested shape with matcher;
lifecycle events (`PreInvocation`, `Stop`) use flat shape.

```bash
# One-shot via CLI:
ai-memory install-hooks --agent antigravity-cli --apply
```

The rendered hooks config looks like:

```json
{
  "ai-memory": {
    "PreInvocation": [
      {
        "type": "command",
        "command": "AI_MEMORY_HOOK_URL=http://127.0.0.1:49374 /path/to/session-start.sh"
      }
    ],
    "PreToolUse": [
      {
        "matcher": "",
        "hooks": [
          {
            "type": "command",
            "command": "AI_MEMORY_HOOK_URL=http://127.0.0.1:49374 /path/to/pre-tool-use.sh"
          }
        ]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "",
        "hooks": [
          {
            "type": "command",
            "command": "AI_MEMORY_HOOK_URL=http://127.0.0.1:49374 /path/to/post-tool-use.sh"
          }
        ]
      }
    ],
    "Stop": [
      {
        "type": "command",
        "command": "AI_MEMORY_HOOK_URL=http://127.0.0.1:49374 /path/to/stop.sh"
      }
    ]
  }
}
```

**Gotchas:**
- Antigravity CLI uses `serverUrl` for HTTP MCP endpoints, not `url`
  or `httpUrl`. The `--apply` flag writes the correct key.
- MCP and hooks use separate files: MCP belongs in
  `~/.gemini/config/mcp_config.json`, while hooks belong in
  `~/.gemini/config/hooks.json`.
- Hook scripts are staged under `~/.local/share/ai-memory/hooks/antigravity-cli/`.
- Native Windows Docker-wrapper installs render hook entries as
  `powershell.exe ... -EncodedCommand <payload>` so Antigravity's outer command
  runner cannot expand the inner `$env:` setup. The child also forces text
  output and disables progress so PowerShell does not serialize progress records
  as `CLIXML` hook stderr. Rerun
  `install-hooks --agent antigravity-cli --apply` after upgrading to refresh
  existing entries.
- The `PreInvocation` event fires before each model call (not just at
  session start). ai-memory uses it as the closest equivalent to Gemini
  CLI's `SessionStart`; when a pending handoff exists, the hook injects
  it via Antigravity's `injectSteps[].ephemeralMessage` output.
- Antigravity CLI does not expose a true session-end hook. `Stop` records a
  stop observation only because it marks the end of one execution loop, not
  the conversation. After the final turn, run
  `ai-memory finalize-session --agent antigravity-cli` to close the session and,
  when it contains substantive events, create the final summary and automatic
  handoff and queue opt-in SessionEnd consolidation.
- `memory_handoff_begin` always creates an explicit manual handoff with no
  `from_session_id` and `from_agent = other`; it is project-wide for cwd
  matching but belongs to the creating operator by default. Pass `shared=true`
  only to publish it to every operator in the project. That session-neutral
  shape is the same for every MCP client. Handoffs carrying a
  Codex or Claude session id came from canonical SessionEnd processing, not
  from the manual tool. Use the explicit Antigravity finalizer when the
  session itself must end and produce an attributed automatic handoff.
- The built-in `/web` route displays compiled wiki pages, not raw session or
  observation rows. To verify hook capture, compare the `sessions` and
  `observations` counts from `ai-memory status` before and after a prompt.
- Source: <https://antigravity.google/docs/hooks>

---

## Zero (Gitlawb/zero)

Zero manages MCP servers in `~/.config/zero/config.json`
(`$XDG_CONFIG_HOME/zero/config.json` on non-default XDG setups) under an
`mcp.servers` map, with native HTTP transport + bearer headers:

```bash
ai-memory install-mcp --client zero --apply \
    --server-url "http://homelab:49374/mcp" --auth-token "$TOKEN"
```

which merges:

```json
{
  "mcp": {
    "servers": {
      "ai-memory": {
        "type": "http",
        "url": "http://homelab:49374/mcp",
        "headers": { "Authorization": "Bearer <token>" }
      }
    }
  }
}
```

Lifecycle capture is separate and script-free: `ai-memory install-hooks
--agent zero --apply` merges exec-form entries (the native `ai-memory hook`
command + args, JSON payload on stdin — no shell) into
`~/.config/zero/hooks.json`, covering `sessionStart`/`sessionEnd`/
`beforeTool`/`afterTool` plus `specialistStart`/`specialistStop` (mapped to
ai-memory's subagent events). Zero discards `sessionStart` hook stdout, so
capture and session-end handoff *creation* work, but handoff *injection*
does not — ask Zero to call `memory_handoff_accept` at the start of a
resumed session.

## ZCode (z.ai)

**Status:** MCP supported. Lifecycle hooks are not installed by this command;
they are tracked in issue #512, so until then capture is not active and
ai-memory only sees the sessions where an agent calls its MCP tools.

**Config file:** ZCode keeps its user-scope config at
`~/.zcode/cli/config.json`, with servers under the nested `mcp.servers` map
(the same shape Zero and OpenClaw use). Workspace scopes
(`.zcode/config.json`, `zcode.json`, `.agents/mcp.json`) also exist; pass
`--config-file` to target one of them explicitly.

```bash
ai-memory install-mcp --client zcode --apply \
    --server-url "http://homelab:49374/mcp" --auth-token "$TOKEN"
```

which merges:

```json
{
  "mcp": {
    "servers": {
      "ai-memory": {
        "type": "http",
        "url": "http://homelab:49374/mcp",
        "headers": { "Authorization": "Bearer <token>" }
      }
    }
  }
}
```

The entry schema is strict: an entry carrying any key outside
`type`/`url`/`headers`/`enabled`/`timeoutMs` is dropped silently, so the
generated registration carries exactly those keys. The default stateless
`/mcp` endpoint needs no flavor marker; auth goes in the `headers` map.
Servers from every scope auto-connect at session start.

## Swival CLI

**Status:** MCP supported. Lifecycle hooks and managed workstreams are not
supported: Swival invokes one shared startup/exit callback without exposing a
stable session identifier, so concurrent sessions cannot be correlated safely.

**Config file:** Swival reads the project-scoped `.swival/mcp.json` by
default (its own documented default lookup), so `install-mcp --client
swival --apply` merges under that path at the nearest `.git` or `swival.toml`
ancestor, matching Swival's own project-root discovery. Pass `--config-file`
only to target a different MCP JSON file.

```bash
ai-memory install-mcp --client swival --apply \
  --server-url "http://homelab:49374/mcp" \
  --auth-token "$TOKEN"
```

which merges into `.swival/mcp.json`:

```json
{
  "mcpServers": {
    "ai-memory": {
      "type": "http",
      "url": "http://homelab:49374/mcp",
      "headers": { "Authorization": "Bearer <token>" }
    }
  }
}
```

`uninstall --only mcp --apply --yes` removes the matching ai-memory entry from
that file and preserves all unrelated MCP servers.

## Grok Build CLI

**Status:** ✅ MCP supported. ✅ Lifecycle hooks supported via
`ai-memory install-hooks --agent grok --apply`. ❌ No automatic handoff
injection (Grok ignores SessionStart stdout — same policy as Zero).

**Config file:** `install-mcp --client grok --apply` writes the user config at
`$GROK_HOME/config.toml` (default `~/.grok/config.toml`). To use a project or
custom config file, pass its exact path with `--config-file`; the CLI does not
infer a project config location. Provide the MCP URL and token explicitly for
that lane, and remove a custom config entry manually when uninstalling.

```bash
ai-memory install-mcp --client grok --apply \
    --server-url "http://homelab:49374/mcp" --auth-token "$TOKEN"
```

which merges:

```toml
[mcp_servers.ai-memory]
url = "http://homelab:49374/mcp"
enabled = true

[mcp_servers.ai-memory.headers]
Authorization = "Bearer <token>"
```

**Schema notes (do not confuse with Codex):**
- Grok uses `[mcp_servers.<name>.headers]`; Codex uses `http_headers`.
- `enabled = true` is the documented per-server toggle.
- String fields support `${VAR}` expansion, so you can write
  `Authorization = "Bearer ${AI_MEMORY_AUTH_TOKEN}"` instead of embedding
  the token.
- CLI alternative: `grok mcp add --transport http ai-memory <url>` (plus
  `--header` for bearer auth).

Lifecycle capture is separate: `ai-memory install-hooks --agent grok
--apply` writes `$GROK_HOME/hooks/ai-memory.json` (default
`~/.grok/hooks/ai-memory.json`; Grok discovers every
`$GROK_HOME/hooks/*.json`, so third-party hook files stay untouched). Events
mirror Claude Code's vocabulary (`SessionStart`, `UserPromptSubmit`,
`PreToolUse`, `PostToolUse`, `PreCompact`, `Stop`, `SessionEnd`,
`SubagentStart`, `SubagentStop`) with a Grok-specific script bundle /
native `ai-memory hook --event … --agent grok` commands. Session-end
handoff *creation* works; handoff *injection* does not — ask Grok to
call `memory_handoff_accept` (or install the managed routing skills under
`.grok/skills` / `$GROK_HOME/skills` (default `~/.grok/skills`)) at the start
of a resumed session.

Grok can also load MCP from Claude Code / Cursor compat sources when those
compat flags are enabled, but first-party `install-mcp --client grok` is
the supported path for uninstall isolation and hooks URL inference.

## Devin CLI

Devin manages MCP servers in `~/.devin/config.json` under `mcpServers`:

```bash
ai-memory install-mcp --client devin --apply \
    --server-url "http://homelab:49374/mcp" --auth-token "$TOKEN"
```

which merges:

```json
{
  "mcpServers": {
    "ai-memory": {
      "url": "http://homelab:49374/mcp",
      "transport": "http",
      "headers": { "Authorization": "Bearer <token>" }
    }
  }
}
```

Lifecycle capture is separate:

```bash
ai-memory install-hooks --agent devin --apply \
    --server-url "http://homelab:49374" --auth-token "$TOKEN"
```

By default this writes `~/.devin/hooks.v1.json`, whose root object is the
event map. If you want the hook entries inside `~/.devin/config.json` instead,
pass that path with `--config-file`; ai-memory then merges them under the
`hooks` key.

Devin's supported lifecycle events are `SessionStart`, `UserPromptSubmit`,
`PreToolUse`, `PostToolUse`, `PostCompaction`, `Stop`, and `SessionEnd`.
`PostCompaction` is a Devin post-compaction event with a `summary` field; it is
not Claude Code's `PreCompact`. Devin does not currently expose subagent
start/stop hooks, so there is no Devin subagent capture surface to install.

Session-start handoff injection is built in: the generated Devin SessionStart
hook returns `hookSpecificOutput.additionalContext` when a pending handoff is
available.

Real Devin hook payloads may omit `session_id` and `cwd`; the installed hooks
fill both in:

- **cwd** — the payload's `cwd` wins when present, then the
  `DEVIN_PROJECT_DIR` environment variable (when Devin's launcher provides
  it), then the hook process working directory.
- **session id** — when the payload has none, the hook mints one at
  `SessionStart`, stores it in a single per-host slot
  (`<data-dir>/hook-state/devin-session-id`), reuses it for every later
  event, and clears it at `SessionEnd`. Set `AI_MEMORY_SESSION_ID` in the
  hook environment to pin an externally managed run id instead. Because the
  slot is per host+agent, two Devin sessions running *concurrently* on the
  same machine share it — the newest `SessionStart` wins and earlier
  sessions' remaining events are attributed to it (same graceful-degradation
  stance as the single-slot `/handoff` fallback). A payload that does carry
  its own `session_id` always wins over both.

## Kimi Code

**Status:** ✅ MCP supported. ✅ Lifecycle hooks supported via
`ai-memory install-hooks --agent kimi-code --apply`.

**Config file (MCP):** `~/.kimi-code/mcp.json`
(`$KIMI_CODE_HOME/mcp.json` when `KIMI_CODE_HOME` is set).

Kimi Code treats any `mcpServers` entry with a `url` field and no
`transport` field as a streamable-HTTP server, so the entry needs no
transport key:

```bash
ai-memory install-mcp --client kimi-code --apply \
    --server-url "http://homelab:49374/mcp" --auth-token "$TOKEN"
```

which merges:

```json
{
  "mcpServers": {
    "ai-memory": {
      "url": "http://homelab:49374/mcp?flavor=moonshot",
      "headers": { "Authorization": "Bearer <token>" }
    }
  }
}
```

`install-mcp` appends the `?flavor=moonshot` query itself (idempotently, so
re-runs don't duplicate it). The Moonshot API validates tool parameter
schemas against a restricted dialect ("moonshot flavored json schema") that
rejects root-level `anyOf`/`oneOf`/`allOf` combinators — including the
`anyOf` on `memory_read_page` — and fails the whole session with a 400 at
`tools/list`. The ai-memory server answers requests carrying this flavor
with flat schemas; every other client keeps receiving the upstream schemas
unchanged.

> **Do not register ai-memory with Kimi's own `mcp add`.** Kimi Code's
> documented command —
>
> ```bash
> kimi mcp add --transport http ai-memory http://127.0.0.1:49374/mcp
> ```
>
> writes the plain URL, with no `?flavor=moonshot`. The server then serves
> the upstream schemas, Moonshot rejects `memory_read_page`'s root-level
> `anyOf`, and **every model turn fails with a 400** — including turns that
> use no tools at all, because tool schemas ship with each request. Use
> `ai-memory install-mcp --client kimi-code --apply` instead, which writes
> the flavored URL for you.
>
> The failure is unusually hard to attribute: `kimi mcp test ai-memory`
> **passes**, because it only lists tools and never sends them upstream. The
> server looks healthy while every real turn dies.
>
> Already registered that way? Either re-run `install-mcp` as above, or set
> the server-side floor and leave the client entry alone:
>
> ```bash
> AI_MEMORY_STRIP_ROOT_COMBINATORS=true   # or `strip_root_combinators = true`
> ```
>
> That serves the restricted dialect on every `tools/list` regardless of the
> `?flavor=` marker, so any strict client that skips the marker is covered —
> not just Kimi. A request's marker can only raise the dialect further, never
> lower it. Reported in #474.

**Config file (hooks):** `~/.kimi-code/config.toml` (same `$KIMI_CODE_HOME`
base). Kimi Code stores hooks as `[[hooks]]` array entries in the same TOML
file that holds its provider/model settings; `install-hooks` merges
ai-memory's entries and preserves everything else:

```bash
ai-memory install-hooks --agent kimi-code --apply \
    --server-url "http://homelab:49374" --auth-token "$TOKEN"
```

The installed entries cover 10 events — `SessionStart`, `SessionEnd`,
`UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PostToolUseFailure`
(Kimi Code fires `PostToolUse` on successful calls only), `Stop`,
`SubagentStart`, `SubagentStop`, and `PreCompact` — with a Kimi Code-specific
script bundle /
native `ai-memory hook --event … --agent kimi-code` commands (native is the
default for local installs; the staged scripts under
`~/.local/share/ai-memory/hooks/kimi-code/` are the compatibility fallback).
Capture is fire-and-forget; a pending handoff is injected at
`UserPromptSubmit` via the hook's stdout (Kimi Code discards `SessionStart`
stdout but prepends successful user-prompt hook output to the turn).

**Gotchas:**
- Do not add a `transport` field for HTTP servers: `url` alone means
  streamable HTTP; `transport: "sse"` selects the legacy SSE transport.
- Do not strip the `?flavor=moonshot` query from the installed URL: without
  it the Moonshot API rejects `memory_read_page`'s root `anyOf` and every
  session fails at `tools/list`.
- Hook entries accept only `event`, `matcher`, `command`, and `timeout`
  (seconds, 1-600, default 30). Any extra field makes Kimi Code fail to load
  the entire `config.toml`, so prefer `install-hooks --apply` over hand
  edits.
- Kimi Code runs identical commands only once when multiple rules match the
  same event. `PostToolUse` and `PostToolUseFailure` reuse one handler command,
  but are mutually exclusive event triggers, so successful and failed calls
  are both captured once.

## Command Code

**Status:** MCP and the four stable shell-hook events are supported. Managed
workstreams and experimental Mods are not installed.

**Config files:** `~/.commandcode/mcp.json` for MCP and
`~/.commandcode/settings.json` for lifecycle hooks.

```bash
ai-memory install-mcp --client command-code --apply \
    --server-url "http://homelab:49374/mcp" --auth-token "$TOKEN"
ai-memory install-hooks --agent command-code --apply \
    --server-url "http://homelab:49374" --auth-token "$TOKEN"
```

The MCP installer writes the documented user-scope remote shape:

```json
{
  "mcpServers": {
    "ai-memory": {
      "transport": "http",
      "enabled": true,
      "url": "http://homelab:49374/mcp",
      "headers": { "Authorization": "Bearer <token>" }
    }
  }
}
```

The hook installer registers `SessionStart`, `PreToolUse`, `PostToolUse`, and
`Stop`. It omits the outer `matcher` from every definition because Command
Code documents omission as matching every tool and says a matcher prevents
the non-tool `SessionStart` and `Stop` events from firing. Native installs
spool events locally, enforce capture exclusions for the documented tool
envelope, and inject pending handoffs with
`hookSpecificOutput.additionalContext` at `SessionStart`.

`Stop` is only a turn boundary. Run `ai-memory finalize-session --agent
command-code` after the final turn (or add `--session-id <uuid>` when several
sessions share the project). ai-memory does not generate a Mod: that API is
experimental and unsandboxed. Command Code officially documents its
project-scoped append-only JSONL location, native resume selectors, `--yolo`,
and Windows aliases, but not the JSONL record schema. A managed adapter remains
deferred until a real logged-in client acceptance test supplies sanitized
structural fixtures and validates checkout ownership, incremental visible-event
import, resume identity, normal-exit finalization, and native Windows execution.

Sources: <https://commandcode.ai/docs/mcp>,
<https://commandcode.ai/docs/hooks>,
<https://commandcode.ai/docs/mods>, and
<https://commandcode.ai/docs/reference/cli>, plus the native-session contract at
<https://commandcode.ai/docs/sessions>.

## Kiro CLI

**Status:** MCP, v2 lifecycle hooks, and explicit v2 managed workstreams are
supported. Bare automatic workstream selection remains unsupported pending a
logged-in current-format v2 acceptance run. V3 hooks and managed sessions need
their own documented, fixture-tested payload and store contracts.

**Config file:** `$KIRO_HOME/settings/mcp.json`, defaulting to
`~/.kiro/settings/mcp.json`. Use `--config-file .kiro/settings/mcp.json` when
you intentionally want Kiro's lower-scope project configuration instead.

```bash
ai-memory install-mcp --client kiro-cli --apply \
    --server-url "https://memory.example/mcp" --auth-token "$TOKEN"
```

The `kiro` alias is equivalent. The command preserves unrelated settings and
servers, and merges this entry idempotently:

```json
{
  "mcpServers": {
    "ai-memory": {
      "url": "https://memory.example/mcp?flavor=bedrock",
      "headers": { "Authorization": "Bearer <token>" }
    }
  }
}
```

Kiro sends MCP tool schemas through Amazon Bedrock, which rejects root-level
`anyOf`, `oneOf`, and `allOf`. The installer appends `?flavor=bedrock`; the
server strips only those root combinators for that request while preserving
nested schemas and the handlers' runtime validation. Kimi Code's existing
`?flavor=moonshot` behavior remains supported independently.

Kiro permits remote MCP URLs over HTTPS. Plain HTTP is accepted only for
`localhost`, `127.0.0.1`, or another loopback address; `install-mcp` rejects a
non-loopback HTTP URL before writing the config. See
[HTTPS via reverse proxy](https-via-proxy.md) for a homelab deployment.

ai-memory supports both documented Kiro hook registration formats through
explicit engine targets:

```bash
# v2: update every existing global agent definition.
ai-memory install-hooks --agent kiro-cli --apply

# v2 project-local agent: target the active definition explicitly.
ai-memory install-hooks --agent kiro-cli --apply \
    --config-file .kiro/agents/<agent-name>.json

# v3: standalone global registration.
ai-memory install-hooks --agent kiro-cli-v3 --apply

# v3 project-local registration.
ai-memory install-hooks --agent kiro-cli-v3 --apply \
    --config-file .kiro/hooks/ai-memory.json
```

The standalone format was acceptance-tested with an interactive Kiro CLI
2.16.2 `--v3` session.

The v2 installer refuses to create a synthetic agent file and parses every
target before changing any of them. Project-local Kiro agents override global
agents, so `--config-file` is required when the active definition lives under
`.kiro/agents/`. The integration remains fail-open, injects pending handoffs
through `agentSpawn` stdout, and honors `$KIRO_HOME`. The v3 installer writes
the standalone `v1` file with PascalCase triggers, preserves third-party
entries, and shares the same fail-open sanitizer and capture-exclusion
boundary. `ai-memory run kiro` (alias `kiro-cli`) manages the default v2
engine; add `--v3`, `--mode`, or `--agent-engine v3` for the incompatible v3
store. Once linked, later plain Kiro launches recover that engine
transparently, and bare `ai-memory run` considers checkout-local sessions from
both engines without cross-resuming them.

Sources: <https://kiro.dev/docs/mcp/configuration/>,
<https://kiro.dev/docs/reference/settings/>,
<https://kiro.dev/docs/hooks/>,
<https://kiro.dev/docs/hooks/types/>, and
<https://kiro.dev/docs/cli/v3/hooks-migration/>.

## OpenClaw

**Status:** ✅ MCP supported. ✅ Lifecycle hooks supported via a native
OpenClaw plugin generated by `ai-memory install-hooks --agent openclaw --apply`.

**Config file:** `~/.openclaw/config.json` (the OpenClaw docs reference
this path indirectly; verify with your `openclaw config show`).

OpenClaw distinguishes transports explicitly. Use
`"transport": "streamable-http"` for ai-memory's HTTP endpoint.

```json
{
  "mcp": {
    "servers": {
      "ai-memory": {
        "url": "http://127.0.0.1:49374/mcp",
        "transport": "streamable-http"
      }
    }
  }
}
```

**Gotchas:**
- `install-hooks --agent openclaw --apply` writes a local plugin package
  under ai-memory's data dir, then runs `openclaw plugins install --link
  <dir> --force` when the `openclaw` CLI is on `PATH`. If the CLI is not
  available, it prints the exact install command.
- The plugin registers OpenClaw `session_start`, `session_end`,
  `before_prompt_build`, `before_tool_call`, `after_tool_call`,
  `before_compaction`, and `agent_end` hooks. `before_prompt_build`
  injects pending handoffs via OpenClaw's `prependContext` hook result.
- Plugin installs or updates require a Gateway restart unless your
  managed OpenClaw Gateway auto-restarts after plugin source changes.
- Sources: <https://docs.openclaw.ai/cli/mcp>,
  <https://docs.openclaw.ai/plugins/hooks>,
  <https://docs.openclaw.ai/plugins/manage-plugins>

---

## Oh My Pi / OMP

**Status:** ✅ MCP supported via `install-mcp --client omp` (or
`--client oh-my-pi`). ✅ Lifecycle capture supported via
`ai-memory install-hooks --agent omp --apply` (or `--agent oh-my-pi`).

**Config file:**
- User: `~/.omp/agent/mcp.json`
- Project: `.omp/mcp.json`

The current Oh My Pi package exposes the `omp` binary and native
`.omp` config directories. Use `omp` (or `oh-my-pi`) for this integration;
real `pi` is recognized separately and uses the generated bridge extension below.

```json
{
  "mcpServers": {
    "ai-memory": {
      "type": "http",
      "url": "http://127.0.0.1:49374/mcp",
      "enabled": true
    }
  }
}
```

**Lifecycle extension:**

```bash
ai-memory install-hooks --agent omp --apply
# or: ai-memory install-hooks --agent oh-my-pi --apply
```

This writes `~/.omp/agent/extensions/ai-memory-omp.ts`, which OMP discovers
as a direct TypeScript extension on startup. Restart `omp` after
installing or changing the file. When `PI_CODING_AGENT_DIR` is set
(it relocates OMP's whole `~/.omp/agent` home), the extension is written
to `$PI_CODING_AGENT_DIR/extensions/ai-memory-omp.ts` instead, and
`--profile <name>` (or `OMP_PROFILE`) targets
`~/.omp/profiles/<name>/agent/extensions/` — note `PI_CODING_AGENT_DIR`
takes precedence over a profile, since it names the agent directory
outright.

Pi and OMP honour the *same* `PI_CODING_AGENT_DIR`, and each agent loads
every direct `*.ts` in its extensions directory. Pointing both at one
directory therefore makes each load both extensions and capture every
event twice, once under each agent identity. `install-hooks` warns when it
detects this; give the two agents separate homes, or scope OMP to a
profile.

**Gotchas:**
- OMP extensions are TypeScript modules, not shell hooks; stdout is not
  used for context injection.
- The extension uses OMP lifecycle events for prompt/tool capture and
  `before_agent_start` to inject pending ai-memory handoffs.

## Pi

**Status:** ✅ MCP and lifecycle capture supported via generated bridge
extension. Pi has no native `mcp.json`; use `install-hooks --agent pi --apply`
to write `~/.pi/agent/extensions/ai-memory-pi.ts`. When `PI_CODING_AGENT_DIR`
is set (it relocates Pi's whole `~/.pi/agent` home), the extension is
written to `$PI_CODING_AGENT_DIR/extensions/ai-memory-pi.ts` instead.

```bash
ai-memory install-hooks --agent pi --apply
```

The generated extension posts lifecycle events to `/hook`, fetches pending
handoffs in `before_agent_start`, initializes ai-memory's HTTP `/mcp` endpoint,
lists tools, and registers each one with `pi.registerTool`. `install-mcp
--client pi` intentionally prints this bridge guidance instead of writing an
ignored `~/.pi/agent/mcp.json`.

OMP / Oh My Pi remains separate: use `--client omp` / `--agent omp` (or
`oh-my-pi`) for `.omp` paths.

---

## Schema dialects for strict upstreams

Some model APIs validate MCP tool parameter schemas against a narrower dialect
than JSON Schema and reject the whole `tools/list` with a 400. ai-memory can
serve a relaxed dialect per request, via a `?flavor=` query on the MCP URL, or
server-wide via config for clients that cannot carry one. Runtime argument
validation is identical in every dialect — only the advertised schema changes.

| Marker | Config key | What it changes | Who needs it |
| --- | --- | --- | --- |
| `?flavor=moonshot` | `strip_root_combinators` | Drops root-level `anyOf`/`oneOf`/`allOf` | Kimi Code (Moonshot); appended by `install-mcp` |
| `?flavor=bedrock` | `strip_root_combinators` | Same as above | Kiro CLI (Bedrock); appended by `install-mcp` |
| `?flavor=gemini` (alias `vertex`) | `gemini_safe_schemas` | The above, plus nullable unions collapsed to a single `type` + `nullable: true` | Clients that forward schemas verbatim to Gemini/Vertex, e.g. OpenCode on a Vertex model |

The Gemini dialect exists because `schemars` renders every optional tool
argument as a nullable union:

```json
"max_proposals": {
  "description": "Override the maximum validated proposal count for this run.",
  "type": ["integer", "null"], "format": "uint", "minimum": 0
}
```

Google's `Schema` (Vertex/Gemini `functionDeclaration.parameters`) takes one
`type` and treats `any_of` as exclusive with every sibling key, so a converter
that forwards this untouched produces `any_of` with `description` next to it and
Vertex refuses the request:

```
Unable to submit request because `ai-memory_memory_auto_improve` functionDeclaration
`parameters.max_proposals` schema specified other fields alongside any_of.
When using any_of, it must be the only field set.
```

The dialect collapses it to `"type": "integer"` plus `nullable: true` — the same
normalization Gemini CLI performs client-side, which is why Gemini CLI and
Antigravity CLI work on Vertex without any marker and do not need this. Reach
for it when a pass-through client fails at `tools/list` with that error.

`install-mcp` does not append `?flavor=gemini` for any client: OpenCode is
provider-agnostic, so the right lever there is the server-side key.

```bash
# server-wide, for clients that cannot carry a query marker
AI_MEMORY_GEMINI_SAFE_SCHEMAS=true ai-memory serve
# or `gemini_safe_schemas = true` in config.toml

# or per client, by hand in its MCP config
#   "url": "http://homelab:49374/mcp?flavor=gemini"
```

Confirm which dialect a request gets by inspecting `tools/list` directly:

```bash
curl -s 'http://127.0.0.1:49374/mcp?flavor=gemini' \
    -H 'Accept: application/json, text/event-stream' \
    -H 'Content-Type: application/json' \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' \
  | jq '.result.tools[] | select(.name=="memory_auto_improve")
        | .inputSchema.properties.max_proposals'
```

---

## After registering MCP - verify it works

Regardless of which client you used, the first sanity check is the
same: ask the model to list its available MCP tools, or to call
`memory_status` explicitly.

```
You: List the MCP tools you can call. Use one of them to check
     ai-memory's status.

Model (any client): I can call: memory_query, memory_recent,
     memory_status, memory_briefing, memory_explore,
     memory_handoff_accept, memory_handoff_begin, memory_handoff_cancel,
     memory_consolidate, memory_auto_improve, memory_write_page,
     memory_read_page, memory_read_session_observations,
     memory_delete_page, memory_feedback,
     memory_lint, memory_forget_sweep, memory_install_self_routing.
     memory_status reports: 0 pages, 0 observations, 0 sessions.
```

If the model sees the tools but does not call them proactively, refresh the
managed routing package. The `memory_install_self_routing` tool is read-only:
it returns the slim markered instruction block, marker strings, agent filename
hints, managed skill payloads (`name`, `description`, `relative_path`,
`content`), and authoritative project/global target hints for `.claude/skills`,
`.agents/skills`, `.grok/skills`, and `$GROK_HOME/skills` (default
`~/.grok/skills`), plus overwrite guidance. Agents should use their own file
editing tools to write those artifacts while preserving unrelated user content.

If the model doesn't see any of those tools, the MCP registration
isn't being picked up. Check:

1. **Is the server running?** `curl http://127.0.0.1:49374/mcp` should
   return a JSON-RPC error (not a connection refused). If refused,
   start ai-memory: `docker start ai-memory` or
   `ai-memory serve --transport http`.
2. **Did the client reload the config?** Claude Desktop and OMP need a
   restart. Cursor watches hooks but usually needs MCP reload/toggle.
   OpenClaw plugin changes need a Gateway restart unless it auto-restarted.
3. **Are you on the right port?** ai-memory's default is **49374**
   (`0xC0DE` in hex). If you remapped, update the URL in every
   client's config.

If the model sees the tools but they all error, the server is
probably running in a different data dir than expected. Check
`docker logs ai-memory` or `ai-memory status --json` for the data
dir on disk.

---

## When does the auto-handoff actually work?

The cross-agent handoff feature (the "headline" pitch in the README)
requires both sides - the agent that *ends* a session, and the agent
that *starts* the next one - to play nicely with ai-memory:

| Side | What's needed | Covered by |
|---|---|---|
| **Ending side** | The agent must create a handoff through a true session-end hook, the manual finalizer, or `memory_handoff_begin`. | Built-in automatically for Claude Code, Devin CLI, Cursor, Gemini CLI, Grok Build CLI, Zero, Kimi Code, OpenClaw, OpenCode, and OMP. Codex, Antigravity CLI, both Kiro CLI engines, and Command Code have no reliable true session-end event; run `ai-memory finalize-session` with the corresponding `--agent` after the final turn. MCP-only clients such as Swival must call `memory_handoff_begin` explicitly. |
| **Starting side** | Either (a) the session-start/plugin path injects the handoff via `/handoff`, OR (b) the model proactively calls `memory_handoff_accept` on first turn. | (a) is built-in for Claude Code / Codex / Devin CLI / Cursor / Gemini CLI / Antigravity CLI / Kimi Code / both Kiro CLI engines / Command Code / OpenClaw / OpenCode / OMP. It requires a client that consumes startup-hook stdout or an equivalent context-injection result. Grok and Zero discard SessionStart stdout; Swival is MCP-only. Use (b) for those clients. (b) works for any MCP-capable client if you nudge the model - see [the managed routing package](usage.md#install-the-routing-snippet-and-agent-skills). |

OpenCode uses its official `session.deleted` plugin event for true session-end
delivery. Its generated plugin also sends a deduped best-effort close for any
still-active sessions from `dispose` during normal plugin teardown; abrupt
process exits can still lose that fallback, so `session.deleted` remains the
primary close path.

Codex and Antigravity `Stop` events are not session ends. Their hook installs
intentionally omit `SessionEnd`; `ai-memory finalize-session` defaults to
Codex, while `--agent antigravity-cli` selects Antigravity. The command finds
the latest matching open session for the current workspace/project and posts a
synthetic `session-end` event through the same server path as real hook clients.
Use `--all` only when you want to close every matching open session for the
selected agent in that scope.

So a typical mixed workflow looks like:

- **Claude Code → Cursor.** Claude Code's `SessionEnd` creates the
  handoff automatically. Cursor's `sessionStart` hook fetches and
  prepends it when `install-hooks --agent cursor --apply` is installed.
- **Claude Desktop → Claude Code.** Claude Desktop doesn't write a
  handoff (no hooks). To resume in Claude Code, you'd have had to
  call `memory_handoff_begin` manually in Claude Desktop before
  quitting. ai-memory's wiki content via `memory_query` is still
  available either way.
