# Design — `ai-memory uninstall` command

**Date:** 2026-05-24
**Branch:** `feat/uninstall-command`
**Status:** design approved, pending spec review

## 1. Problem

`ai-memory` ships a rich install surface — `install-hooks`, `install-mcp`,
`install-instructions`, `setup-agent` — but **no inverse**. Removing the
integration today means the user must, by hand:

- delete the 7 hook entries from `~/.claude/settings.json` /
  `~/.codex/hooks.json` / `~/.cursor/hooks.json` / `~/.gemini/settings.json`,
- delete the OpenCode plugin file `~/.config/opencode/plugins/ai-memory.ts`,
- delete the MCP server registration from each client config,
- delete the `<!-- ai-memory:start -->`…`<!-- ai-memory:end -->` block from
  `CLAUDE.md` / `AGENTS.md`,
- and remember to wipe the data dir.

This is error-prone and undiscoverable. `agentmemory` has a `remove` command;
`ai-memory` should have a symmetric one, scoped to its own distribution model.

The v0.3 roadmap (`docs/v0.3-roadmap.md`) does **not** list this; it is an
unplanned gap, not a documented non-goal.

## 2. Scope

**In scope (the "wiring"):**
- Remove ai-memory hook entries from every supported agent's config.
- Remove the ai-memory MCP server registration from every supported client.
- Remove the ai-memory instruction block from `CLAUDE.md` / `AGENTS.md`.
- Optionally (`--purge-data`) wipe `wiki/`, `db/`, `raw/` via the existing
  `reset` path.

**Out of scope (printed as a hint, never executed):**
- Docker teardown (`docker compose down -v`, `docker volume rm`,
  removing the `bin/ai-memory` wrapper). `ai-memory` runs inside the
  container; the CLI is a thin client (invariant #16) and does not own the
  container lifecycle. We print the commands for the user to run.

**Explicitly rejected (YAGNI / unbounded):**
- A `--force-remove-all` flag that deletes user-edited hooks. User-modified
  entries are preserved by design and reported as skipped; no force escape
  hatch in v1.
- Scanning per-project config files (`.cursor/mcp.json`, project-local
  `AGENTS.md`) across the filesystem. Unbounded search; these files are in
  the user's repos, git-visible, and their own concern. We document the
  limitation. `uninstall` touches `$HOME`-rooted locations only.

## 3. CLI surface

New **local** (non-HTTP) subcommand, in the invariant-#16 exception bucket
alongside `install-*` and `reset` (pre/post-server local setup; no running
server required).

```
ai-memory uninstall [--apply] [--purge-data] [--only <kind>]
                     [--config-file <PATH>] [--yes]
```

| Flag | Default | Effect |
|---|---|---|
| *(none)* | — | **Dry-run.** Detect and print the removal plan; write nothing; exit 0. Mirrors `install-* without --apply` and `reset` without `--confirm`. |
| `--apply` | off | Execute the plan. Each touched file is rewritten via `apply_atomic` (tmp+rename+fsync) with a `.bak-<unix-ts>` backup. |
| `--purge-data` | off | After the wiring removal, wipe `wiki/`, `db/`, `raw/` through the `reset` path. Only meaningful with `--apply`. |
| `--only <kind>` | all | Limit to one concern: `hooks` \| `mcp` \| `instructions`. Omitted = all three. |
| `--yes` | off | Skip the single interactive confirmation when a TTY is attached. |

There is **no `--config-file` flag**. With the default "remove from all agents
at once", a single path override cannot disambiguate which agent's many config
files it applies to (Claude's `settings.json`, Codex's `hooks.json`, …). Path
override exists only as a **function parameter** on each `plan_removal(...)`
(see §5), used by tests — not exposed on the CLI.

### Why `--only` instead of `--agent`

The codebase has **two distinct enums** that do not line up:

- `AgentChoice` (cli.rs) — hooks/instructions targets: ClaudeCode, Codex,
  Cursor, GeminiCli, OpenCode, Openclaw.
- `McpClient` (cli.rs) — MCP targets: ClaudeCode, Codex, OpenCode, Cursor,
  **ClaudeDesktop**, **Pi**, GeminiCli, … — includes MCP-only clients with
  no hooks.

A single `--agent` flag cannot address both axes (it would silently miss
ClaudeDesktop/Pi for MCP). `uninstall` defaults to "remove everything we
detect" and offers `--only` to narrow by *concern*, not by agent. Detection
loops over **both** enums: `AgentChoice` for hooks/instructions, `McpClient`
for MCP. This keeps the user from needing to know which enum an agent lives in.

## 4. Detection & safe identification (the core)

The command scans known `$HOME`-rooted config locations and removes **only
entries it can positively attribute to ai-memory**, never third-party config.

### 4.1 Hooks — shape-aware, signature-based

Hook JSON is **not** a flat key. Per `render_shared.rs`:

- `HookShape::Nested` (Claude Code, Codex, Gemini CLI):
  `"<Event>": [ { "matcher":"", "hooks":[ {"type":"command","command":"…"} ] } ]`
- `HookShape::Flat` (Cursor):
  `"<event>": [ { "type":"command","command":"…","matcher":"" } ]`

Removal therefore operates **inside the array**, and the `command` lives at a
different depth per shape:

- Nested: `["<Event>"][i].hooks[j].command`
- Flat: `["<event>"][i].command`

Find the entry whose `command` matches the ai-memory signature, remove that
entry (the innermost `hooks[j]` object for Nested; the array element for Flat),
and prune the event key only if its array becomes empty. It never blindly
deletes an event key.

**Signature — primary signal: the `command` string contains the literal
`AI_MEMORY_HOOK_URL=`.** Install **inlines** the env vars into the command
string rather than a JSON `env` field (verified at `render_shared.rs:225-243`;
Claude Code does not honour an `env` field at this level). The rendered command
is `AI_MEMORY_HOOK_URL=<url> [AI_MEMORY_AUTH_TOKEN=<t> ] <abs-path>/<script>.sh`,
and the `AI_MEMORY_HOOK_URL=` prefix is written **unconditionally**
(`render_shared.rs:239`, before the auth-token branch). So this signal is
present even in the **no-auth default** (invariant #13) and is independent of
`--server-url`, `--hooks-dir`, and `--host-prefix`. There is **no** JSON `env`
block to match — an earlier draft of this spec wrongly relied on one.

The 7 known script basenames (`session-start.sh`, `user-prompt-submit.sh`,
`pre-tool-use.sh`, `post-tool-use.sh`, `pre-compact.sh`, `stop.sh`,
`session-end.sh`) are a **secondary corroboration**, not the primary key —
basename-only would risk a false positive on a coincidentally-named third-party
`stop.sh`. An entry whose `command` lacks the `AI_MEMORY_HOOK_URL=` prefix is
treated as **not ours** (user-modified or third-party): preserved and reported
as skipped.

**Stale events:** an entry carrying the ai-memory signature is removed **even
if its event name is outside the agent's current vocabulary** (e.g. a Codex
`SessionEnd` left by an older install — Codex's current vocab has no
`SessionEnd`; see `install_hooks.rs` stale-key cleanup). Detection is by
signature, not by the current event list. Note that install already proactively
removes such stale keys, so uninstall may never encounter one — but the
signature rule covers it if it does.

### 4.2 MCP — matched by endpoint, not just name

`install-mcp` writes `mcpServers.<name>` (default `ai-memory`, overridable via
`--name`). Removal must not assume the default name. An MCP server entry is
ai-memory's when **either**:

- it carries a `"url"` field equal to the ai-memory `server_url` (HTTP-transport
  clients: Claude Code, Cursor, Gemini CLI, Openclaw; Codex's `config.toml`
  uses `[mcp_servers.<name>]` with `url = "…"`), **or**
- it is a stdio shim — `"command": "npx"` with an `"args"` array that contains
  both `"mcp-remote"` and the ai-memory `server_url` as elements (Claude
  Desktop). Matching scans the variadic `args` array for the URL element; extra
  `--header` args (when auth is set) do not affect the match.

This is name-independent and survives a custom `--name` install.

Per-client location nuances (settings.json `mcpServers`, Codex `config.toml`
`[mcp_servers]`, Cursor `~/.cursor/mcp.json`, OpenCode `opencode.json`,
Claude Desktop via `mcp-remote`) are resolved by the same per-`McpClient`
path logic the install path already encodes. `Pi` is MCP-unsupported (install
bails); for uninstall it is a **silent no-op**, never a bail (§6).

### 4.3 Instructions — marker block

Strip the text between `MARKER_START` (`<!-- ai-memory:start -->`) and
`MARKER_END` (`<!-- ai-memory:end -->`), inclusive, from `CLAUDE.md` /
`AGENTS.md`, collapsing the surrounding blank line. This is the exact inverse
of `install-instructions` and reuses the markers from
`ai-memory-core/src/routing_snippet.rs`. The rest of the file is untouched.

### 4.4 OpenCode — file deletion, not key removal

OpenCode's hooks are a **plugin file**: `install_hooks.rs` writes
`~/.config/opencode/plugins/ai-memory.ts`. Removal is a **file delete** of
that exact path (a `RemovalAction` of kind `PluginFile`), not a JSON-key edit.
OpenCode's MCP entry (in `opencode.json`) is handled by §4.2.

The plugin file is deleted **unconditionally** if present — hand-edits to it
are not detected. This is symmetric with install, which **overwrites** the
plugin unconditionally (`install_hooks.rs:314`: "Overwrite unconditionally —
the schema is versioned"). Hashing the file against the rendered template to
skip user-edited copies would be inconsistent with install and is YAGNI for
v1.

## 5. Architecture

Interface = single `uninstall` subcommand (approach A). Reversal logic lives
**next to** the install logic it mirrors (approach C): each `install_*` module
gains a `plan_removal(...)` that returns typed actions; `commands/uninstall.rs`
is a thin orchestrator.

```
commands/uninstall.rs        # orchestrator: loop enums, collect, print, apply
commands/install_hooks.rs    # + plan_removal_hooks(agent, cfg_override)
commands/install_mcp.rs      # + plan_removal_mcp(client, cfg_override)
commands/install_instructions.rs # + plan_removal_instructions(target)
```

Per-agent path resolution currently inlined inside the `apply_to_*` functions
is **extracted into small local helper functions within the same module**
(e.g. `claude_settings_path()`, `codex_hooks_path()`), so both the install and
the removal path call them. This is light, in-module refactoring in service of
the feature — no new cross-cutting registry (would be scope creep per
workflow rule #6). If removal logic turns out identical across several agents
during implementation, minor consolidation may happen then.

### RemovalAction (typed plan)

```rust
struct RemovalAction {
    file: PathBuf,
    kind: RemovalKind,   // HookEntry | McpServer | InstructionBlock | PluginFile
    detail: String,      // human-readable: which event / server name / file
    status: ActionStatus // WillRemove | SkippedUserModified
}
```

## 6. Execution flow

1. **Collect.** Loop `AgentChoice` (hooks + instructions) and `McpClient`
   (MCP); call each module's `plan_removal`; gather `RemovalAction`s. Missing
   config / absent key = no action (not an error). Agents with no relevant
   surface produce **no actions and never bail**: `Openclaw` has no hooks
   (install prints and mutates nothing), and `Pi` has no MCP (install bails) —
   for uninstall both are silent no-ops, so iterating the full enums adds no
   confusing errors, just absent entries.
2. **Present.** Print the plan grouped by file, one line per action, marking
   `remove` vs `skipped (user-modified)`. Format mirrors `reset.rs`
   (one-per-line + a trailing hint). Without `--apply`, **stop here**, exit 0.
3. **Confirm.** With `--apply` and an interactive TTY, ask one confirmation
   (`--yes` skips). Non-interactive (docker/CI) proceeds — consistent with the
   project's docker-first, non-interactive posture.
4. **Apply wiring.** Rewrite each affected file via `apply_atomic` +
   `mutate_json` / `mutate_toml`; delete `PluginFile`s. When removing the last
   ai-memory entry empties a JSON parent object (e.g. `hooks` → `{}`), remove
   that parent key but **never delete the user's config file**. For TOML,
   remove the leaf key/table only; leave an emptied parent table header in
   place (cosmetic, syntactically valid).

   **Asymmetry with install (intentional).** Install writes each event with
   `hooks.insert(event, value)` — it **overwrites the whole event key**, so it
   cannot preserve a pre-existing third-party hook under that event. Uninstall
   does the opposite: it removes only the *matching* array entry and preserves
   any sibling third-party entry. The two are deliberately not mirror images —
   install can't know what's third-party; uninstall must try to preserve it.
   Entry-level removal is therefore the safe choice even though, in the common
   case (no manual post-install edits), the event array holds only ai-memory's
   entry and the whole key is pruned anyway.
5. **Purge data (optional).** Only if `--purge-data` and `--apply`. Runs
   **after** wiring. Reuses `commands::reset`: `process_guard::sibling_processes()`
   refuses with `busy_message` if any `ai-memory` is alive, else
   `remove_dir_all` on `wiki/`, `db/`, `raw/`.
6. **Report.** Summary of removed / skipped actions + backup paths. Then the
   Docker teardown hint (printed, never executed). If `--purge-data` was
   refused (live process), the report states clearly that **wiring removal
   succeeded but data was not purged**, and the command exits non-zero — the
   wiring success is not masked by the purge failure.

## 7. Error handling

| Situation | Behaviour |
|---|---|
| Config file or key absent | Not an error. Reported as "nothing to remove"; idempotent no-op; exit 0. |
| Malformed JSON/TOML in user's file | Fail with `anyhow::Context`, **write nothing**, suggest manual edit. Same defensive stance as install ("a bad merge is very user-visible"). |
| `--purge-data` with live sibling process | `bail!(busy_message(...))` for the purge step; wiring already applied — report both facts; exit non-zero. |
| `$HOME` unresolvable | Clear error, as in `install-*`. |
| Partial failure across files | Each file is atomic in isolation; an earlier file's backup is intact if a later file fails. Final report lists done vs not-done. |
| Hook matches basename but no corroborating signal | Preserved; reported as `skipped (user-modified)`. No force override in v1. |

## 8. Testing (TDD — test before implementation, rule #5)

Unit tests per `plan_removal` and for the orchestrator:

- **Detects** an ai-memory hook entry (nested shape) and (flat shape, Cursor).
- **Detects the no-auth default**: a hook whose command is
  `AI_MEMORY_HOOK_URL=<url> <path>/stop.sh` (no `AI_MEMORY_AUTH_TOKEN`) is still
  matched via the unconditional `AI_MEMORY_HOOK_URL=` prefix.
- **Preserves** a third-party hook sharing a generic basename (`stop.sh`) whose
  command lacks the `AI_MEMORY_HOOK_URL=` prefix → not removed (not reported as
  ours at all).
- **Removes** an ai-memory hook entry whose event is outside the current
  vocabulary (stale `SessionEnd` for Codex).
- **Prunes** an event key only when its array empties; leaves sibling
  third-party entries intact.
- **MCP**: removes the server matched by endpoint URL even under a custom
  `--name`; preserves an unrelated MCP server.
- **Instructions**: removes the marker block, leaves text before/after intact;
  idempotent on a file with no block.
- **OpenCode**: deletes the `ai-memory.ts` plugin file; no-op if absent.
- **Idempotency**: second run is a no-op, exit 0.
- **Dry-run** writes nothing (content + mtime unchanged); `--apply` writes and
  produces a `.bak-<ts>`.
- **Emptied parent**: JSON parent key removed, file kept; TOML leaf removed,
  table header kept.
- **`--purge-data`** refuses with a live sibling (mock `process_guard`) and the
  report shows wiring-done / data-not-purged with non-zero exit.

## 9. Open limitations (documented, not bugs)

- Per-project config files are not scanned (see §2, rejected).
- Docker/volume/wrapper teardown is printed, not executed (see §2).
- A hook the user redirected to a non-ai-memory wrapper script is preserved
  (no force removal in v1).
