# MCP install guide - additional clients

For a custom tool, start with the [programmatic memory guide](programmatic-memory.md).
It shows direct MCP writes, queries and handoffs without a native hook adapter.

Warp, Kilo Code, Goose and OpenHuman have no
`install-mcp` client; their hand-written recipes are in
[`install.md`](install.md#clients-without-an-install-mcp-writer).

> All snippets below default to `http://127.0.0.1:49374` (local server). For a
> remote server (homelab, LAN box) substitute the appropriate URL AND add an
> `Authorization: Bearer <token>` header to the `headers` block when bearer auth
> is enabled. The MCP wire protocol expects the `/mcp` path suffix on the URL.

> **Token kept in the data dir.** When this install's bearer comes from
> `[auth].bearer_token_file` (every fresh `ai-memory init`, and
> `ai-memory auth enable`), `install-mcp` never writes it into a client's
> config. Claude Code gets the session-aware `ai-memory mcp-bridge` stdio
> entry, and Muse Code always gets one (`mcp-bridge --agent muse`, which reads
> the token file itself); Codex (`bearer_token_env_var`), Prime Agent (`bearerTokenEnvVar`) and
> Factory Droid (`Bearer ${AI_MEMORY_AUTH_TOKEN}` in `headers`) name
> `AI_MEMORY_AUTH_TOKEN`, which you export where you start them. Every
> other client gets a local stdio entry, in its own format, that runs
> `ai-memory mcp-bridge --no-session-id --server-url <url>`: the bridge reads
> the token file at launch and forwards no session id, so the client passes
> `workspace` and `project` like any static client. Stdio entries name the
> `ai-memory` binary by its absolute path, so the client does not need it on
> its `PATH`; the Docker wrapper keeps the bare `ai-memory`. When the install
> was given a data dir (`--data-dir`, `AI_MEMORY_DATA_DIR` or `data_dir` in
> the config), every bridge entry names it with `--data-dir`, as that
> install's hooks do, so the bridge reads that install's config and token
> rather than the default data dir's. ZCode keeps
> HTTP entries only and is refused. Hermes Agent gets the bridge entry
> whatever the token source, so its config never holds a token. Passing `--auth-token` explicitly (or
> `AI_MEMORY_AUTH_TOKEN`) keeps the HTTP snippets below, header included.
> The two entries that run the bridge with an explicit key are the
> exception: `install-mcp --apply --auth-token <key>` for Claude Code with
> `--session-aware`, and for Muse, stores the key in `<data_dir>/auth-token`,
> the file `install-hooks --apply --auth-token` writes, and writes the entry
> with `--data-dir` and no key. The bridge reads that file whenever no
> bearer is configured, ahead of the root token it discovers on a loopback
> install, so a client whose hooks already hold a key needs no
> `--auth-token` here at all, and rotating the key with `install-hooks` also
> rotates the bridge's. The key stays in the entry, as before, when the
> config already resolves a bearer, a different key is stored, the hooks
> sign in with OIDC, or the snippet is only printed.

> **Transport is stateless by default.** Since v0.1.2 the HTTP transport
> answers each request independently (plain JSON, no `Mcp-Session-Id`
> required), so any client that points a remote URL at `/mcp`, including
> OpenCode `type: "remote"` and plain `curl`, works without an
> `mcp-remote` stdio shim (issue #3). The `mcp-remote` bridge is still
> needed for **Claude Desktop** specifically, because its config only
> supports stdio servers; session state has nothing to do with it. If you run a
> client that *requires* MCP session continuity or server-initiated SSE
> streams, start the server with `ai-memory serve --transport http
> --http-stateful` to restore rmcp's session mode.

This page documents how to register ai-memory as an MCP server with
agent CLIs beyond the README quick start. For OpenCode, the generic
`install-mcp --client opencode` command probes `opencode --version` and writes
the matching V1 direct `mcp` or V2 nested `mcp.servers` entry. Strict semantic
major 1 and 2 outputs are accepted; malformed, timed-out, and future-major
probes fail rather than guessing. During a V1/V2 transition, hook and MCP
installers reuse an ownership-verified endpoint and bearer from either known
schema; conflicting generated entries fail for explicit resolution, and custom
entries are preserved. The `opencode2`, `opencode-v2`, and `open-code2`
spellings remain force-V2 compatibility aliases. A Docker
container cannot inspect the host executable; use `setup-agent --agent opencode
--opencode-dialect v1|v2` there to print the selected plugin and MCP artifacts.

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
stdout (or their equivalent context-injection result). Grok's first-turn rule
(`$GROK_HOME/rules/ai-memory-handoff.md`, installed with the Grok hooks and
with global Grok skills) has the model call `memory_handoff_list` through
`use_tool` with the qualified catalog key `ai-memory__memory_handoff_list`, so
the first `PostToolUse` can claim the baton; Zero must call
`memory_handoff_list` then
`memory_handoff_accept` when resuming.

Capture exclusions are separate from MCP registration. Native hook commands and
generated OpenCode/OMP/Pi/OpenClaw integrations enforce `[capture]
ignore_paths`; legacy shell/PowerShell and remote-only/Docker script bundles do
not. The Linux/macOS Docker wrapper's ordinary `install-hooks` command uses its
checksum-verified native host client and is covered; this limitation applies to
manual container/script extraction and explicit compatibility overrides.
Reinstall/refresh an existing hook or plugin to gain it; see
[Capture exclusions](marker-file.md#capture-exclusions).

Claude Desktop's ordinary Chat surface, VS Code Copilot and Zed
are **MCP-only** here:
they expose long-term memory to their LLMs via ai-memory's MCP tools
(`memory_query`, `memory_recent`, `memory_handoff_accept`, etc.), but
they do not auto-capture session events into ai-memory's `/hook`
endpoint. The trade-off:

| | What you get | What you don't get |
|---|---|---|
| **MCP only** | LLM can query the wiki, accept handoffs, run memory_consolidate, and run `memory_auto_improve` learning reviews | No automatic session-end summaries; no auto-handoff at session boundaries |
| **MCP + hooks** | All of the above *plus* bounded sanitized prompt/tool-lifecycle observations captured automatically; handoffs surface at SessionStart with no human prompting **only when the client consumes startup-hook output or an equivalent context-injection result** | Hook observations are not complete native transcripts. Grok delivers the handoff on the first `PostToolUse`; its first-turn rule calls `memory_handoff_list` so that tool exists. Zero discards SessionStart stdout; ask it to call `memory_handoff_list` then `memory_handoff_accept`. |

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

### Hermes Agent

ai-memory ships a first-party Hermes integration: lifecycle hooks and an MCP
entry, both printed for you to merge into `~/.hermes/config.yaml`
(`$HERMES_HOME/config.yaml`), which ai-memory never writes.

```bash
ai-memory install-hooks --agent hermes --server-url "http://homelab:49374"
ai-memory install-mcp --client hermes
ai-memory install-skills --scope global --agent hermes
```

The hooks block runs the native `ai-memory hook` command in exec form, since
Hermes splits each `command` with `shlex.split` and runs it with no shell. It
wires the tool events and the session lifecycle: `on_session_start`,
`pre_llm_call` (the prompt, plus any pending handoff as `{"context": ...}` on
the next primary-session turn, including after a native resume),
`on_session_end` (a turn boundary, because Hermes fires
it after every turn) and `on_session_finalize` (the session end). Hermes gates
user hooks behind its own acceptance prompt (`hooks_auto_accept`), so paste the
block and let Hermes accept it. [install.md](install.md#hermes-agent-nous-research)
has the event table, the `TERMINAL_CWD` routing, the per-profile marker for a
multiplexing gateway's bots and the
`finalize-session --agent hermes` step for idle gateway sessions.

`install-mcp --client hermes` (alias `hermes-agent`) prints an entry for the
`mcp_servers:` map that runs `ai-memory mcp-bridge --no-session-id` over stdio
with the install's `--data-dir`. The file gets no token: the bridge sends the
install's own, which matters because Hermes passes MCP servers a filtered
environment without `AI_MEMORY_AUTH_TOKEN`. Hermes is a static client, so the
model passes `workspace` and `project` on project-scoped calls. `--apply` is
refused. Hermes does not read MCP server instructions, so the routing rules
reach the model only through the skills in `~/.hermes/skills` and the handoff
in the next primary-session turn.

The community-maintained
[`ai-memory-hermes-plugin`](https://github.com/MrLuciano/ai-memory-hermes-plugin)
memory provider is an alternative for prompts, session ends, recall and the
handoff. Treat it as a third-party bridge: verify its documented Hermes and
ai-memory version matrix, install/update/uninstall behavior, platform coverage,
and secret handling before enabling it on a live ai-memory server. Bearer tokens
and endpoint settings should stay in environment or local config references
rather than generated plugin source files. Run only one lifecycle path: when the
plugin is installed in `$HERMES_HOME/plugins/ai-memory`, `install-hooks --agent
hermes` prints only `pre_tool_call` and `post_tool_call`, so prompts are not
captured twice and the handoff is not claimed twice. The check is the
directory alone, as seen with the command's own `HERMES_HOME`; see
[install.md](install.md#hermes-agent-nous-research) for a disabled plugin or
one installed under another profile.

The hook router recognizes `agent=hermes` as a concrete session kind and
accepts Hermes' shell-hook `tool_name` / `tool_input` envelope for tool-family
metadata and capture-exclusion enforcement. A custom bridge should map
`pre_llm_call`, `post_tool_call`, `on_session_end` and `on_session_finalize` to
ai-memory's canonical `user-prompt`, `post-tool-use`, `stop` and `session-end`
events, sending the prompt text as `prompt` rather than Hermes' whole `extra`
object, which carries the conversation history. This protocol recognition does
not install or trust a third-party plugin.

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
> ai-memory install-mcp --client gemini-cli   # or cursor / claude-desktop / openclaw / omp / pi / antigravity-cli / grok / kimi-code / kiro-cli / command-code / swival / devin / zero / zcode / copilot-cli / vscode-copilot / zed / muse / dsh / cline / freebuff / prime-agent
> ```

---

## Cline CLI

```bash
ai-memory install-mcp --client cline --apply
ai-memory install-hooks --agent cline --apply
```

The MCP writer targets
`${CLINE_MCP_SETTINGS_PATH:-~/.cline/data/settings/cline_mcp_settings.json}`
and writes Cline's top-level `type`, `url`, and optional `headers` fields with
`type: "streamableHttp"`; omitting that type makes Cline try legacy SSE. The
hook writer creates Cline CLI's supported event files under `~/.cline/hooks`.
Use `--scope project` for `.cline/hooks` in the current checkout. Existing
user-owned event files and symlinked targets are refused, and generated files
that carry a bearer token are owner-only.

Cline runs task-start hooks detached, so accept a pending handoff through MCP
(`memory_handoff_list`, then `memory_handoff_accept`). Restart Cline after
installing. Cline's yolo mode disables hooks. The VS Code extension has a
separate hook directory and is not changed by this CLI integration.

## FreeBuff

```bash
ai-memory install-mcp --client freebuff --apply
```

FreeBuff reads `mcpServers` from `.agents/mcp.json` while walking from the
current directory toward the home directory. The installer writes the home
file and registers a local `ai-memory mcp-bridge --no-session-id` stdio
process, which forwards to the configured server and reads a file-backed token
without copying it into FreeBuff's config. An explicit `--auth-token` is stored
in the stdio entry's `AI_MEMORY_AUTH_TOKEN` environment map, like a bearer
header in an HTTP-only client. FreeBuff has no lifecycle-hook or
managed-workstream integration; it already reads `AGENTS.md`.

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

The generated entry runs `ai-memory mcp-bridge`, naming the binary by its
absolute path, connects to the same configured
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

**Marketplace plugin.** The ai-memory Claude Code plugin
(`claude plugin install ai-memory@ai-memory`, see
[install.md](install.md#marketplace-plugins)) registers this same
session-aware bridge for you, as server `plugin:ai-memory:ai-memory`, so its
tools appear as `mcp__plugin_ai-memory_ai-memory__<tool>`. The plugin passes
its `server_url` option to `mcp-bridge --server-url`; left empty (the
default), the bridge uses the server in your ai-memory config. While the
plugin is enabled, `ai-memory setup` does not also write an `ai-memory`
entry to `~/.claude.json`, and `ai-memory doctor` flags one that is already
there. The Codex plugin likewise registers `ai-memory mcp-bridge
--no-session-id`.

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

## GitHub Copilot CLI

**Status:** MCP and lifecycle hooks are supported, the `SessionStart` hook
injects the pending handoff (see below), and `ai-memory run copilot` launches
Copilot CLI as a managed workstream harness (see
[managed workstreams](managed-workstreams.md)).

**Config files:** `$COPILOT_HOME/mcp-config.json` for MCP and
`$COPILOT_HOME/hooks/ai-memory.json` for lifecycle hooks (`COPILOT_HOME`
defaults to `~/.copilot`). Not to be confused with `--client vscode-copilot`
(alias `copilot`), the VS Code agent-mode client below.

```bash
ai-memory install-mcp --client copilot-cli --apply \
    --server-url "http://homelab:49374/mcp" --auth-token "$TOKEN"
ai-memory install-hooks --agent copilot-cli --apply \
    --server-url "http://homelab:49374" --auth-token "$TOKEN"
```

The MCP installer merges the documented remote entry into the root
`mcpServers` map, preserving other servers (including your own `github`
entry):

```json
{
  "mcpServers": {
    "ai-memory": {
      "type": "http",
      "url": "http://homelab:49374/mcp",
      "headers": { "Authorization": "Bearer <token>" },
      "tools": ["*"]
    }
  }
}
```

The equivalent interactive registration is `copilot mcp add --transport http
ai-memory http://homelab:49374/mcp` (add `--header "Authorization: Bearer
<token>"` when bearer auth is on). Copilot also reads project-level
`.mcp.json` and `.github/mcp.json` files; pass that path via `--config-file` to
write one, keeping in mind that `.github/mcp.json` is meant to be committed,
so it should not carry a bearer token. `install-hooks --agent copilot-cli`
run without `--server-url` reads the URL and token back from this entry.

The hook installer wires the same ten events as Claude Code, `PostToolUseFailure`
included, with PascalCase names, which makes Copilot send its VS Code/Claude-compatible
payload; native installs spool events locally and enforce capture exclusions. The
`SessionStart` hook delivers the prior session's handoff: Copilot reads a
top-level `additionalContext` from `SessionStart` stdout (not Claude Code's
`hookSpecificOutput` envelope), and the hook prints exactly that, or `{}` when
nothing is pending.

- Sources: <https://docs.github.com/en/copilot/how-tos/copilot-cli/customize-copilot/add-mcp-servers>,
  <https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-config-dir-reference>,
  <https://docs.github.com/en/copilot/reference/hooks-reference>

---

## Factory Droid

**Status:** MCP and lifecycle hooks are supported (#1240); the `SessionStart`
hook injects the pending handoff and Droid's `SessionEnd` closes the session.
No managed workstream.

**Config files:** `~/.factory/mcp.json` for MCP and `~/.factory/hooks.json`
for lifecycle hooks.

```bash
ai-memory install-mcp --client droid --apply \
    --server-url "http://homelab:49374/mcp" --auth-token "$TOKEN"
ai-memory install-hooks --agent droid --apply \
    --server-url "http://homelab:49374" --auth-token "$TOKEN"
```

The MCP installer merges an HTTP entry into the root `mcpServers` map,
preserving other servers. Droid expands `${NAME}` only in `headers`, so the
URL stays literal and the bearer is a variable reference, never the token:

```json
{
  "mcpServers": {
    "ai-memory": {
      "type": "http",
      "url": "http://homelab:49374/mcp",
      "headers": { "Authorization": "Bearer ${AI_MEMORY_AUTH_TOKEN}" }
    }
  }
}
```

Export `AI_MEMORY_AUTH_TOKEN` where you launch droid; an unset variable makes
Droid fail the connection. Without a bearer the entry has no `headers`.
`install-hooks --agent droid` run without `--server-url` reads the URL back
from this entry (not the token, which is only a reference there). The hook
installer, its native commands and the `settings.json` refusal are described
in [`install.md`](install.md#factory-droid).

- Sources: <https://docs.factory.com/cli/configuration/mcp>,
  <https://docs.factory.com/harness/hooks>

---

## VS Code GitHub Copilot

**Status:** ✅ MCP supported (workspace-default). ❌ No lifecycle hooks
(Copilot's agent mode does not expose `PreToolUse` / `PostToolUse` /
`SessionStart` yet, so ai-memory's automatic capture is not active in
VS Code; call `memory_query`, `memory_write_page`, etc. from chat).

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
  for MCP. Your org may restrict which MCP servers Copilot is allowed
  to call; check **Settings → Copilot → MCP servers** if the server
  shows as blocked.
- Lifecycle hooks aren't possible until VS Code Copilot adds an agent
  hook surface. Until then, the auto-handoff flow that other agents
  enjoy (SessionStart auto-fetches a "where you left off" block) does
  not run here. Ask the agent to call `memory_handoff_accept`
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

## Muse Code

**Status:** MCP and lifecycle hooks are supported. The MCP entry is a
session-aware stdio bridge, the `SessionStart` hook injects the pending
handoff, and Muse's `SessionEnd` closes the session. No managed workstream.

**Config file:** `${XDG_CONFIG_HOME:-~/.config}/muse/settings.json`, the same
file `install-hooks --agent muse` writes its hooks into.

```bash
ai-memory install-mcp --client muse --apply
ai-memory install-hooks --agent muse --apply
```

Muse gives its stdio MCP servers `MUSE_SESSION_ID`, while a streamable-HTTP
entry carries no session id, so the entry runs the bridge, which forwards that
id as the actor session:

```json
{
  "schema_version": 1,
  "mcpServers": {
    "ai-memory": {
      "type": "stdio",
      "command": "/home/you/.local/bin/ai-memory",
      "args": ["mcp-bridge", "--agent", "muse",
               "--server-url", "http://127.0.0.1:49374/mcp",
               "--data-dir", "/home/you/.local/share/ai-memory"],
      "mode": "optional"
    }
  }
}
```

Muse starts stdio servers with a cleared environment, so the entry names the
install's data dir and the bridge reads the server's token from there at
launch. A token passed explicitly with `--auth-token` is written to the
entry's `env` as `AI_MEMORY_AUTH_TOKEN`; treat the file as private then. The
bridge reads only `MUSE_SESSION_ID` when started with `--agent muse` (and only
`CLAUDE_CODE_SESSION_ID` for Claude Code), so an id inherited from another
harness's terminal is never forwarded, and it refuses to start without its
own id.

Three details of Muse's schema are worth knowing before hand-editing the file:

- **`"schema_version": 1` is mandatory.** A settings file that omits it fails
  every `muse` command at startup with `malformed settings file`. `--apply`
  adds the key when it is missing and never rewrites an existing value, so a
  future schema is not silently downgraded.
- **`mcpServers` or `mcp_servers`, never both.** Muse reads servers from
  `mcpServers` (`"type": "stdio"`) or the legacy `mcp_servers`
  (`"transport": "stdio"`), and ignores every MCP server when both maps are
  present. `--apply` uses `mcp_servers` when the file already has it, else
  `mcpServers`, and refuses a file that already holds both;
  `ai-memory doctor` reports that state.
- **`mode` defaults to `required`**, and a required server that fails to start
  aborts the whole Muse run. ai-memory writes `"mode": "optional"`, so an
  unreachable memory server only costs you recall and the session goes on.

`install-hooks --agent muse` run without `--server-url` reads the URL back
from this entry. The hook installer and its events are described in
[`install.md`](install.md#meta-muse-code).

**Skills:** no extra step is needed. Muse Code loads user skills from
`$XDG_CONFIG_HOME/muse/skills` and `~/.agents/skills`, and
`ai-memory install-skills` already writes the cross-client `~/.agents/skills`
root, so the ai-memory routing skills are visible to Muse. Confirm with
`muse skills list`.

Sources: <https://dev.meta.ai/docs/muse-code/configuration>,
<https://dev.meta.ai/docs/muse-code/extending>.

---

## DeepSeek Harness (DSH)

**Status:** MCP supported (print-only) + lifecycle hooks via the official
Claude Code hooks bridge. No managed workstream.

DSH has no fixed JSON/TOML MCP config: the MCP client is a Cordis plugin entry
(`@deepseek-ai/dsh-mcp-client`) inside a profile's `cordis.patch.yml`.
`install-mcp` therefore prints a ready-to-merge patch fragment and refuses
`--apply`:

```bash
ai-memory install-mcp --client dsh
```

Merge the printed `- insert:` item into
`$DSH_HOME/profiles/<profile>/cordis.patch.yml` (the `web` profile
reloads its patch live), then reload or restart the profile. The fragment registers
the default stateless HTTP endpoint and carries `Authorization: Bearer <token>`
when `--auth-token` (or a configured server token) is present.

Lifecycle capture runs through DSH's official Claude Code hooks bridge
(`@deepseek-ai/dsh-hooks-claude-code`), which consumes a Claude Code
`hooks.json`. ai-memory ships a DSH hook bundle and a first-party installer:

```bash
ai-memory install-hooks --agent dsh --apply \
    --server-url "$AI_MEMORY_SERVER" --auth-token "$TOKEN"
```

`--apply` stages the `hooks/dsh` scripts under the data dir and writes a
generated Claude Code `hooks.json` next to them (native `ai-memory hook`
commands by default, so capture enforces capture exclusions and keeps the
local spool). It then prints the bridge patch fragment — a `- insert:` item
mounted under the DSH profile patch — which you merge into
`$DSH_HOME/profiles/<profile>/cordis.patch.yml` by hand, because that file is
a hand-maintained YAML patch layer (the same reason `install-mcp --client
dsh` refuses to write it):

```yaml
- insert:
    - id: ai-memory-hooks
      name: '@deepseek-ai/dsh-hooks-claude-code'
      config:
        configPath: '<data-dir>/hooks/dsh/hooks.json'
```

Install the bridge package into the profile first (`dsh plugin --profile
<profile> add @deepseek-ai/dsh-hooks-claude-code`), then reload or restart
the profile. Without `--apply`, `install-hooks --agent dsh` prints the same
`hooks.json` and patch fragment for a manual merge.

Two bridge constraints shape DSH capture:

- The bridge delivers only seven Claude Code events (`SessionStart`,
  `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`, `SubagentStart`,
  `SubagentStop`). `PreCompact` and `SessionEnd` are not delivered, so a
  finished session stays open until `ai-memory finalize-session --agent dsh`
  closes it (the same close path as Command Code, Kiro CLI and Antigravity).
- `SessionStart` runs detached, so its stdout cannot reliably deliver a
  fetched handoff; the DSH hook bundle therefore never claims one. Recover a
  pending handoff with `memory_handoff_list` then `memory_handoff_accept`, or
  ask the model to. DSH sessions are stored as `agent=dsh`, never as
  `claude-code`.

---

## Prime Agent

**Status:** MCP supported. No lifecycle hooks or managed workstream claimed.

[Prime Agent](https://github.com/PrimeIntellect-ai/prime-agent) reads user MCP
servers from the `mcpServers` map of its agent dir's `settings.json`:
`$PRIME_AGENT_CODING_AGENT_DIR/settings.json`, default
`~/.prime/agent/settings.json`. A project's `.prime/agent/settings.json`
overrides it; pass that path with `--config-file`.

```json
{
  "mcpServers": {
    "ai-memory": {
      "type": "http",
      "url": "http://127.0.0.1:49374/mcp",
      "bearerTokenEnvVar": "AI_MEMORY_AUTH_TOKEN"
    }
  }
}
```

```bash
ai-memory install-mcp --client prime-agent --apply \
  --server-url "http://homelab:49374/mcp" \
  --auth-token "$TOKEN"
export AI_MEMORY_AUTH_TOKEN="$TOKEN"   # in the shell you launch prime-agent from
```

How Prime Agent 0.10 treats the entry shapes the generated one:

- **The token is referenced, not written.** Prime Agent counts a user HTTP
  server as connected, and names it in the model's prompt
  (`Enabled generic MCP servers: ai-memory`), only when the variable in
  `bearerTokenEnvVar` is set or an OAuth grant exists. A literal
  `headers.Authorization` still reaches the server but leaves it unlisted, so
  `install-mcp` writes `"bearerTokenEnvVar": "AI_MEMORY_AUTH_TOKEN"` and the
  token stays out of `settings.json`. If the variable is unset, Prime Agent
  shows the server as disconnected and calls fail.
- **Without a token** the entry is just `type` + `url`. Calls through
  `mcp.call_tool("ai-memory", ...)` still work, but the server is not listed in
  the prompt or shown as connected in `/mcp`. Run the server with
  `AI_MEMORY_AUTH_TOKEN` and pass `--auth-token` to get it listed.
- **Use the camelCase key.** Prime Agent 0.10.0's own
  `prime-agent mcp add --bearer-token-env-var` writes `bearer_token_env_var`,
  which its settings loader ignores. Use `install-mcp` or edit the file.

Prime Agent does not expose MCP tools as native tools. The model calls them
from its persistent Python REPL:
`await mcp.list_tools("ai-memory")`, then
`await mcp.call_tool("ai-memory", "memory_query", {...})`. Requests carry no
session id or working directory, so Prime Agent is a static client: pass
`workspace` and `project` on project-scoped calls (the routing snippet from
`ai-memory install-instructions --target AGENTS.md` says so, and Prime Agent
loads `AGENTS.md`).

Prime Agent 0.10 is a Rust rewrite: the TypeScript extensions of its 0.9
releases, which were built on Pi, are gone, and it has no lifecycle hook
surface. Automatic capture and handoff
injection are therefore not claimed, and `install-hooks --agent pi` does not
apply. Ask the agent to call `memory_handoff_begin` before leaving and
`memory_handoff_accept` when resuming.

Verified against Prime Agent 0.10.0. Sources:
<https://github.com/PrimeIntellect-ai/prime-agent> (`crates/pa-core/src/mcp/mod.rs`,
`crates/pa-cli/src/mcp_command.rs`, `prime-agent-runtime/src/rlm/mcp.py`,
`skills/mcp/SKILL.md`).

---

## Claude Desktop

**Status:** ✅ MCP supported (via stdio shim for HTTP). ❌ No lifecycle hooks
in the Chat tab. The Code tab runs Claude Code itself, so its local sessions
are captured by the [Claude Code](#claude-code) hooks.

**Config file:**
- macOS: `~/Library/Application Support/Claude/claude_desktop_config.json`
- Windows: `%APPDATA%\Claude\claude_desktop_config.json` for an
  unpackaged install, or
  `%LOCALAPPDATA%\Packages\Claude_<id>\LocalCache\Roaming\Claude\claude_desktop_config.json`
  for a detected MSIX-packaged install. `install-mcp` checks for
  `Claude_*` package directories automatically and prefers one that
  already contains a config. If multiple candidates remain ambiguous,
  it stops and asks for an explicit `--config-file` instead of guessing.
- Linux: `~/.config/Claude/claude_desktop_config.json`. Anthropic ships
  Claude Desktop for Linux as a beta for Debian-based distributions
  (Ubuntu 22.04+, Debian 12+). `install-mcp --client claude-desktop` writes
  it there (`$XDG_CONFIG_HOME/Claude/` when that is set).

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
  `.mcpb` desktop extensions, which Anthropic now presents as the main way
  to install local MCP servers. The ai-memory CLI manages the local
  JSON-config path because it works with localhost/LAN servers and does
  not require publishing an HTTPS connector.
- Servers in `claude_desktop_config.json` also reach the Code tab's local
  sessions, alongside `~/.claude.json` and `.mcp.json`; when both define a
  server with the same name, the Code tab uses the
  `claude_desktop_config.json` entry.
- Claude Desktop's ordinary Chat surface exposes MCP tools but does not run
  lifecycle hooks, so ai-memory cannot automatically capture its
  prompts/tools or inject session-boundary handoffs.
- The Code tab reads the same `~/.claude/settings.json` as the CLI, so
  `install-hooks --agent claude-code` already captures its local sessions
  (as `claude-code`). A Code session started without a project folder runs
  in a scratch directory under
  `~/.config/Claude/scratch-workspaces/`, so each one lands in its own
  auto-named project.
- Cowork is a distinct surface whose capture ai-memory does not claim. On
  Linux, Anthropic runs Cowork tasks inside a QEMU/KVM virtual machine;
  whether your host hooks reach it has not been verified.
- If the MCP indicator doesn't appear after restart, check the logs:
  `~/Library/Logs/Claude/mcp*.log` (macOS), `~/.config/Claude/logs/`
  (Linux). On Windows, check
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
- Sources: <https://code.claude.com/docs/en/desktop-linux>,
  <https://code.claude.com/docs/en/desktop#shared-configuration>,
  <https://support.claude.com/en/articles/10949351-getting-started-with-local-mcp-servers-on-claude-desktop>,
  <https://support.claude.com/en/articles/11175166-how-to-connect-remote-mcp-integrations-to-claude>,
  <https://support.claude.com/en/articles/13837440-use-plugins-in-claude>,
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

## Qwen Code

**Status:** MCP and lifecycle hooks are supported. Managed workstreams and
transcript backfill are not claimed.

**Config file:** `~/.qwen/settings.json` (user). Qwen also accepts a
project-level `.qwen/settings.json`; pass that path with `--config-file`
when wanted. MCP servers live under the root `mcpServers` map; hooks live
under the root `hooks` object of the same file.

```bash
ai-memory install-mcp --client qwen-code --apply \
    --server-url "https://memory.example/mcp" --auth-token "$TOKEN"
ai-memory install-hooks --agent qwen-code --apply \
    --server-url "https://memory.example" --auth-token "$TOKEN"
```

The aliases `qwen` and `qwencode` are accepted by both commands. Qwen Code
uses `httpUrl` (not `url`) for streamable-HTTP MCP endpoints, with `timeout`
in milliseconds. The plain `url` key is the legacy SSE transport; do not mix
them in one entry. Qwen expands `$NAME` and `${NAME}` in every string of
`settings.json`. Existing servers, settings and third-party hooks are
preserved, and applying either command again is a no-op. A token-file
install (every fresh `init`) writes a stdio `ai-memory mcp-bridge
--no-session-id` entry instead of embedding the bearer, so Qwen is a static
client: project-scoped calls still need `workspace` and `project`.

The hook installer registers ten events: `SessionStart`, `UserPromptSubmit`,
`PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `PreCompact`, `Stop`,
`SessionEnd`, `SubagentStart` and `SubagentStop`. Definitions use an empty
`matcher`, which Qwen treats as match-all. Each command receives Qwen's
Claude-shaped JSON on stdin (`session_id`, `cwd`, `tool_name`, `tool_input`,
`tool_use_id`) and enforces capture exclusions over Qwen's Gemini-like
tools. For `UserPromptSubmit`, ai-memory captures only Qwen's optional
`submitted_prompt`, which identifies an actual user submission; it does not
fall back to the generic `prompt` field used by continuation and retry paths.
A pending handoff is printed as `hookSpecificOutput.additionalContext` on
`SessionStart`. Capture-only `SessionStart` and `UserPromptSubmit` events stay
silent so `{}` is never injected into the model context. When the payload has
no `cwd`, the hook uses `QWEN_PROJECT_DIR`.

Qwen fires a real `SessionEnd`, so finished sessions close without
`finalize-session`. There is no `ai-memory run qwen` adapter: Qwen does not
document a stable transcript-store schema and path. Qwen reads `AGENTS.md`
and `QWEN.md`, so the normal ai-memory routing snippet and Agent Skills need
no Qwen-specific instruction file. Restart the CLI session after changing
`~/.qwen/settings.json` so both MCP servers and hooks are reloaded.

Sources: [Qwen Code MCP](https://qwenlm.github.io/qwen-code-docs/en/users/features/mcp/),
[Qwen Code hooks](https://qwenlm.github.io/qwen-code-docs/en/users/features/hooks/),
and [Qwen Code settings](https://qwenlm.github.io/qwen-code-docs/en/users/configuration/settings/).

---

## Qoder

**Status:** MCP and lifecycle hooks are supported. Managed workstreams and
transcript backfill are not claimed.

**Config file:** `~/.qoder/settings.json` (user), or a project
`.qoder/settings.json` supplied with `--config-file`. MCP servers live under
`mcpServers`; hooks live under `hooks` in the same document.

```bash
ai-memory install-mcp --client qoder --apply \
    --server-url "https://memory.example/mcp" --auth-token "$TOKEN"
ai-memory install-hooks --agent qoder --apply \
    --server-url "https://memory.example" --auth-token "$TOKEN"
```

Qoder's MCP configuration accepts stdio `command`, `args`, and `env`. ai-memory
always writes a sessionless `mcp-bridge` entry, so it can use the configured
remote server without a second server process contending for the data-dir lock.
The bridge has no lifecycle identity; pass explicit `workspace` and `project`
on every project-scoped MCP call. Applied token installs use ai-memory's
owner-only token file when possible; printed configuration carries the token in
the entry's `AI_MEMORY_AUTH_TOKEN` environment.

The hook installer registers the eight documented events that carry useful
memory state: `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`,
`PostToolUseFailure`, `PreCompact`, `Stop`, and `SessionEnd`. Qoder sends
Claude-shaped fields such as `session_id`, `cwd`, `tool_name`, `tool_input`, and
`tool_use_id`, so capture exclusions are enforced by the same closed parser.
SessionStart delivers a pending handoff through
`hookSpecificOutput.additionalContext`; a missing payload `cwd` falls back to
`QODER_PROJECT_DIR`. Qoder requires a restart after hook changes.

Qoder reads `AGENTS.md` and shared `~/.agents/skills`, so the normal routing
snippet and installed Agent Skills need no Qoder-specific copy. There is no
`ai-memory run qoder` adapter or native transcript backfill because no stable
native transcript-store contract is documented.

Sources: [Qoder hooks](https://docs.qoder.com/cli/hooks) and
[Qoder MCP](https://docs.qoder.com/cli/mcp-reference).

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
lifecycle events (`SessionStart`, `PreInvocation`, `Stop`) use flat shape.

```bash
# One-shot via CLI:
ai-memory install-hooks --agent antigravity-cli --apply
```

The rendered hooks config looks like:

```json
{
  "ai-memory": {
    "SessionStart": [
      {
        "type": "command",
        "command": "AI_MEMORY_HOOK_URL=http://127.0.0.1:49374 /path/to/session-start.sh"
      }
    ],
    "PreInvocation": [
      {
        "type": "command",
        "command": "AI_MEMORY_HOOK_URL=http://127.0.0.1:49374 /path/to/pre-invocation.sh"
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
- `SessionStart` delivers a pending handoff through Antigravity's
  `injectSteps[].systemMessage` output, which stays in the conversation for
  later turns. `PreInvocation` fires before each model call; at
  `invocationNum = 0`, the start of a user turn, the native hook records the
  turn's prompt from the conversation's transcript, and on builds without
  `SessionStart` the first turn's `PreInvocation` delivers the handoff
  instead.
- Antigravity has no conversation-end hook. A `Stop` that finished the turn
  with nothing left running writes a turn checkpoint (summary page and one
  refreshed handoff, conversation left open), which a new session in the
  project receives once the conversation has been quiet for 10 minutes. Run
  `ai-memory finalize-session --agent antigravity-cli` to close the session at
  once and queue opt-in SessionEnd consolidation. If the conversation
  continues after that first finalize, re-close it with
  `ai-memory finalize-session --agent antigravity-cli --reopen --session-id <uuid>`
  so the session-end path re-runs over the new observations (re-running with
  nothing new is a harmless no-op). See
  [`docs/install.md`](install.md#antigravity-lifecycle-hooks) for the
  transcript read's limits.
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
command + args, JSON payload on stdin, no shell) into
`~/.config/zero/hooks.json`, covering `sessionStart`/`sessionEnd`/
`beforeTool`/`afterTool` plus `specialistStart`/`specialistStop` (mapped to
ai-memory's subagent events). Zero discards `sessionStart` hook stdout, so
capture and session-end handoff *creation* work, but handoff *injection*
does not. Ask Zero to call `memory_handoff_accept` at the start of a
resumed session.

Zero runs hook commands inside its execution sandbox. The default policy
denies all network, loopback included (a separate network namespace on
Linux, `(deny network*)` on macOS), mounts `/` read-only with writes limited
to the workspace and temp, and kills a hook's detached children when the
hook exits. Hooks therefore cannot reach `server_url`: events land in
`<data_dir>/hook-spool/` and stay there. Zero only lets the user's global
`~/.config/zero/config.json` relax this (a project config can only tighten
it), and ai-memory does not edit that file. Either add

```json
"sandbox": {
  "network": "allow",
  "additionalWriteRoots": ["<ai-memory data dir>"]
}
```

(network access is all-or-nothing and applies to every shell command Zero
runs, not only hooks), or set `"sandbox": {"enabled": false}`. With network
allowed, `sessionEnd` delivers the spool from the hook process itself within a
fixed 10-second budget, because the sandbox would kill the detached drainer
other agents use; whatever does not make it stays spooled. Without either, run `ai-memory hook-drain` from a normal shell to deliver the
spooled events. A leftover empty `hook-spool/.drain.lock` does not block
that drain: the lock is an OS file lock released when its holder exits.

## ZCode (z.ai)

**Status:** MCP supported. Lifecycle hooks are installed separately with
`ai-memory install-hooks --agent zcode --apply` (see the
[support matrix](support-matrix.md)). On a token-file install (every fresh
`init`) `install-mcp --client zcode` is refused, because ZCode keeps only a
literal header; add the entry by hand with the header read from
`<data_dir>/auth-tokens/default`.

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
`ai-memory install-hooks --agent grok --apply`. Handoff injection is the
first `PostToolUse` (`additionalContext` after the tool result). Grok
ignores `SessionStart` stdout and discards an allowing `UserPromptSubmit`,
so those events do not accept the handoff. `--apply` also writes
`$GROK_HOME/rules/ai-memory-handoff.md` (default `~/.grok/rules/ai-memory-handoff.md`)
so the first turn calls `memory_handoff_list` through `use_tool` with the
qualified catalog key `ai-memory__memory_handoff_list` (Grok exposes MCP tools
only through that meta-tool), before any file read, and corrects the scope from
`./.ai-memory.toml` only when that list is empty or a scope error; PostToolUse then claims the
row.

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
handoff *creation* works. Injection is the first `PostToolUse`: JSON
`additionalContext` with the pending handoff and an opted-in `[briefing]`.
`memory_handoff_list` is the first-turn path when the managed Grok rule is
installed (skills live under `.grok/skills` / `$GROK_HOME/skills`, default
`~/.grok/skills`; the rule lives under `$GROK_HOME/rules`).
`memory_handoff_accept` remains the recovery path when that rule is absent
and the session has not called a tool yet.

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

- **cwd**: the payload's `cwd` wins when present, then the
  `DEVIN_PROJECT_DIR` environment variable (when Devin's launcher provides
  it), then the hook process working directory.
- **session id**: when the payload has none, the hook mints one at
  `SessionStart`, stores it in a single per-host slot
  (`<data-dir>/hook-state/devin-session-id`), reuses it for every later
  event, and clears it at `SessionEnd`. Set `AI_MEMORY_SESSION_ID` in the
  hook environment to pin an externally managed run id instead. Because the
  slot is per host+agent, two Devin sessions running *concurrently* on the
  same machine share it: the newest `SessionStart` wins and earlier
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
rejects root-level `anyOf`/`oneOf`/`allOf` combinators (including the
`anyOf` on `memory_read_page`) and fails the whole session with a 400 at
`tools/list`. The ai-memory server answers requests carrying this flavor
with flat schemas; every other client keeps receiving the upstream schemas
unchanged.

> **Do not register ai-memory with Kimi's own `mcp add`.** Kimi Code's
> documented command,
>
> ```bash
> kimi mcp add --transport http ai-memory http://127.0.0.1:49374/mcp
> ```
>
> writes the plain URL, with no `?flavor=moonshot`. The server then serves
> the upstream schemas, Moonshot rejects `memory_read_page`'s root-level
> `anyOf`, and **every model turn fails with a 400**, including turns that
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
> `?flavor=` marker, so it covers any strict client that skips the marker,
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

The installed entries cover 10 events (`SessionStart`, `SessionEnd`,
`UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PostToolUseFailure`
(Kimi Code fires `PostToolUse` on successful calls only), `Stop`,
`SubagentStart`, `SubagentStop`, and `PreCompact`) with a Kimi Code-specific
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

**Status:** MCP, the four stable shell-hook events, and opt-in managed
workstreams are supported. Experimental Mods are not installed.

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
sessions share the project). A managed `ai-memory run command-code` launch
finalizes automatically on normal exit when it can tie the native session to
the run. Its read-only adapter validates the observed v3 header, UUID filename,
and canonical checkout before importing visible events, and resumes with the
exact native `--session <uuid>` selector. Unknown transcript versions fail
closed. ai-memory does not generate a Mod: that API is experimental and
unsandboxed.

Sources: <https://commandcode.ai/docs/mcp>,
<https://commandcode.ai/docs/hooks>,
<https://commandcode.ai/docs/mods>, and
<https://commandcode.ai/docs/reference/cli>, plus the native-session contract at
<https://commandcode.ai/docs/sessions>.

## IBM Bob

**Status:** MCP and lifecycle hooks are supported for IBM Bob IDE and Bob
Shell. Managed workstreams and transcript backfill are not claimed.

**Config files:** `~/.bob/settings/mcp.json` for MCP and
`~/.bob/settings/settings.json` for lifecycle hooks. Bob also accepts a
project-level `.bob/mcp.json`; pass that path with `--config-file` when wanted.

```bash
ai-memory install-mcp --client ibm-bob --apply \
    --server-url "https://memory.example/mcp" --auth-token "$TOKEN"
ai-memory install-hooks --agent ibm-bob --apply \
    --server-url "https://memory.example" --auth-token "$TOKEN"
```

The alias `bob` is accepted by both commands. The MCP entry uses Bob's native
`streamable-http` transport with `protocolNegotiation: "legacy"`. Bob's
automatic negotiation first sends a non-standard `server/discover` request;
the documented legacy setting skips that probe and starts with the standard
MCP initialization ai-memory implements. Existing servers and settings are
preserved, and applying either command again is a no-op.

The hook installer registers the five events shared by Bob IDE and Bob Shell:
`SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, and `Stop`.
Definitions omit `matcher`, which Bob documents only for tool events. The
native command receives Bob's JSON on stdin, enforces capture exclusions over
the Claude-shaped tool fields, and prints a pending handoff as plain
`SessionStart` context. Capture-only `SessionStart` and `UserPromptSubmit`
events stay silent so `{}` is never injected into the model context.

Bob does not expose a true `SessionEnd`; `Stop` is a turn boundary. After the
last turn, finalize explicitly when an immediate summary and handoff matter:

```bash
ai-memory finalize-session --agent ibm-bob
```

There is no `ai-memory run ibm-bob` adapter. Bob Shell documents resume
selectors but not a stable transcript-store schema and path, so claiming
cross-harness transcript import would be unsafe without fixtures and a logged-in
acceptance test. Bob reads `AGENTS.md`, so the normal ai-memory routing snippet
and Agent Skills need no Bob-specific instruction file.

Sources: [Bob MCP configuration](https://bob.ibm.com/docs/ide/configuration/mcp/mcp-in-bob),
[Bob IDE lifecycle hooks](https://bob.ibm.com/docs/ide/configuration/lifecycle-hooks),
and [Bob Shell lifecycle hooks](https://bob.ibm.com/docs/shell/configuration/lifecycle-hooks).

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
- User: `mcp.json` in OMP's agent dir: `~/.omp/agent/mcp.json` by default,
  `~/.omp/profiles/<name>/agent/mcp.json` under a named profile
  (`OMP_PROFILE`, or the legacy `PI_PROFILE`), or
  `$PI_CODING_AGENT_DIR/mcp.json` when that variable relocates the default
  profile.
- Project: `.omp/mcp.json`

`PI_CONFIG_DIR` changes the `.omp` root relative to your home. An explicit
`PI_CODING_AGENT_DIR` for the default profile keeps its own path.

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
installing or changing the file. `--profile <name>` (or `OMP_PROFILE`)
targets `~/.omp/profiles/<name>/agent/extensions/`. Named profiles ignore
`PI_CODING_AGENT_DIR`. For the default profile,
`PI_CODING_AGENT_DIR` relocates OMP's whole `~/.omp/agent` home, so the
extension and `mcp.json` move to `$PI_CODING_AGENT_DIR` instead.

Pi and OMP honour the *same* `PI_CODING_AGENT_DIR`, and each agent loads
every direct `*.ts` in its extensions directory. Pointing both at one
directory therefore makes each load both extensions and capture every
event twice, once under each agent identity. `install-hooks` and the
`ai-memory run` auto-wire warn when they detect this; give the two agents
separate homes, or put OMP on a named profile, which leaves
`PI_CODING_AGENT_DIR` to Pi.

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
validation is identical in every dialect; only the advertised schema changes.

| Marker | Config key | What it changes | Who needs it |
| --- | --- | --- | --- |
| `?flavor=moonshot` | `strip_root_combinators` | Drops root-level `anyOf`/`oneOf`/`allOf`, plus inlines every `#/$defs/*` reference and drops the emptied `$defs` table | Kimi Code (Moonshot); appended by `install-mcp`. The generated Pi extension's MCP bridge always asks for it, since Pi may run on Moonshot |
| `?flavor=bedrock` | `strip_root_combinators` | Drops root-level `anyOf`/`oneOf`/`allOf` | Kiro CLI (Bedrock); appended by `install-mcp` |
| `?flavor=gemini` (alias `vertex`) | `gemini_safe_schemas` | Drops root-level `anyOf`/`oneOf`/`allOf`, plus nullable unions collapsed to a single `type` + `nullable: true` | Clients that forward schemas verbatim to Gemini/Vertex, e.g. OpenCode on a Vertex model |

Moonshot's validator never resolves `$ref`: any reference, at the root or
nested, fails the request with "detected infinite recursion without termination
condition". Inline combinators such as the nullable union on
`Option<T>` arguments do pass. Codex forwards MCP input schemas into Responses
`tools.function.parameters` verbatim, so it needs the Moonshot marker even
though it is not Kimi Code. `?flavor=bedrock` and `?flavor=gemini` keep
`$defs`/`$ref` pairs, which their upstreams resolve.

`install-mcp` appends the first two itself for the clients whose upstream is
fixed. For any other client, pass the dialect explicitly:

```bash
ai-memory install-mcp --client command-code --flavor gemini --apply
```

This is an operator choice rather than another per-client default because a
client like Command Code or OpenCode fronts several models: it needs the Gemini
dialect only when routed to Vertex, and pinning it there for everyone would
narrow the advertised schema for models that never had the problem. `--flavor`
overrides a client's built-in default rather than stacking a second marker, and
`uninstall` matches every marker on every client, so an entry pinned this way is
still removed.

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

The dialect collapses it to `"type": "integer"` plus `nullable: true`, the same
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
     memory_handoff_list,
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
`.agents/skills`, `.grok/skills`, `$GROK_HOME/skills` (default
`~/.grok/skills`), GitHub Copilot CLI's `.github/skills` and
`$COPILOT_HOME/skills` (default `~/.copilot/skills`), plus overwrite guidance. Agents should use their own file
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
| **Ending side** | The agent must create a handoff through a true session-end hook, the manual finalizer, or `memory_handoff_begin`. | Built-in automatically for Claude Code, Codex (native `SessionEnd`, Codex CLI 0.145.0+), Devin CLI, Cursor, Gemini CLI, Grok Build CLI, Zero, Kimi Code, OpenClaw, OpenCode, and OMP. Antigravity CLI, both Kiro CLI engines, and Command Code have no reliable true session-end event; run `ai-memory finalize-session` with the corresponding `--agent` after the final turn (also the fallback on Codex older than 0.145.0). MCP-only clients such as Swival must call `memory_handoff_begin` explicitly. |
| **Starting side** | Either (a) the session-start/plugin path injects the handoff via `/handoff`, OR (b) the model inspects with `memory_handoff_list` then claims with `memory_handoff_accept` (`handoff_id` from the list). | (a) is built-in for Claude Code / Codex / Devin CLI / Cursor / Gemini CLI / Antigravity CLI / Kimi Code / Grok Build CLI / both Kiro CLI engines / Command Code / OpenClaw / OpenCode / OMP. Grok's (a) is the first `PostToolUse` `additionalContext`, not SessionStart; `install-hooks --agent grok --apply` also installs a first-turn `memory_handoff_list` user rule so that tool exists. Zero discards SessionStart stdout; Swival is MCP-only. Use (b) for those. (b) works for any MCP-capable client if you nudge the model - see [the managed routing package](usage.md#install-the-routing-snippet-and-agent-skills). |

OpenCode uses its official `session.deleted` plugin event for true session-end
delivery. The OpenCode V2 plugin subscribes to the same event name on the
V2 event stream and injects handoffs through the `context` hook instead of
v1's removed `experimental.chat.system.transform`. Its generated plugin also sends a deduped best-effort close for any
still-active sessions from `dispose` during normal plugin teardown; abrupt
process exits can still lose that fallback, so `session.deleted` remains the
primary close path.

Codex and Antigravity `Stop` events are not session ends. Codex's hook install
registers native `SessionEnd` for CLI 0.145.0 and later; Antigravity still needs
explicit finalization. `ai-memory finalize-session` defaults to Codex for older
clients or a missed native end, while `--agent antigravity-cli` selects
Antigravity. The command finds the latest matching open session for the current
workspace/project and posts a synthetic `session-end` event through the same
server path as real hook clients.
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
