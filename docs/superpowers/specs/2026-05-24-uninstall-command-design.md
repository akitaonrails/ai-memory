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
| `--config-file <PATH>` | auto | Override a config path (testing / non-standard setups). |
| `--yes` | off | Skip the single interactive confirmation when a TTY is attached. |

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

Removal therefore operates **inside the array**: find the entry whose command
matches the ai-memory signature, remove that entry, and prune the event key
only if its array becomes empty. It never blindly deletes an event key.

**Signature** (composite — path prefix alone is unreliable because
`install-hooks --hooks-dir` and `setup-agent --host-prefix` let the user choose
the directory): an entry is ai-memory's when its `command`'s **basename** is
one of the 7 known scripts (`session-start.sh`, `user-prompt-submit.sh`,
`pre-tool-use.sh`, `post-tool-use.sh`, `pre-compact.sh`, `stop.sh`,
`session-end.sh`) **AND** at least one corroborating signal holds:

- the entry's `env` block contains an `AI_MEMORY_*` key (e.g.
  `AI_MEMORY_AUTH_TOKEN`), or
- the `command`/`env` references the ai-memory server URL or
  `AI_MEMORY_HOOK_URL`, or
- the command path's parent layout is `<dir>/<agent>/<script>.sh` matching
  the staged hooks layout.

Basename-only is **not** sufficient (a third-party hook could coincidentally
be named `stop.sh` → false-positive removal). An entry matching the basename
but no corroborating signal is treated as **user-modified**: preserved and
reported as skipped.

**Stale events:** an entry carrying the ai-memory signature is removed **even
if its event name is outside the agent's current vocabulary** (e.g. a Codex
`SessionEnd` left by an older install — Codex's current vocab has no
`SessionEnd`; see `install_hooks.rs` stale-key cleanup). Detection is by
signature, not by the current event list.

### 4.2 MCP — matched by endpoint, not just name

`install-mcp` writes `mcpServers.<name>` (default `ai-memory`, overridable via
`--name`). Removal must not assume the default name. An MCP server entry is
ai-memory's when its `url` equals the ai-memory `server_url` **or** its
`args` invoke `mcp-remote` against that URL. This is name-independent and
survives a custom `--name` install.

Per-client location nuances (settings.json `mcpServers`, Codex `config.toml`
`[mcp_servers]`, Cursor `~/.cursor/mcp.json`, OpenCode `opencode.json`,
Claude Desktop via `mcp-remote`) are resolved by the same per-`McpClient`
path logic the install path already encodes.

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
   config / absent key = no action (not an error).
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
- **Preserves** a third-party hook sharing a generic basename (`stop.sh`) but
  lacking any ai-memory signal → reported skipped, not removed.
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
