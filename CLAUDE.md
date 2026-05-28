# CLAUDE.md — ai-memory project directives

> Read this every session before touching code. The long-form research and
> design specs live under [`docs/`](docs/); this file is the operating rules.

## What this project is

A self-contained Rust binary providing long-term memory for AI coding agents
(Claude Code, OpenAI Codex, OpenCode) over the Model Context Protocol.
Storage = markdown-in-git wiki (source of truth) + SQLite (derived index).
Capture = automatic via agent lifecycle hooks, never manual `write_note`.
Consolidation = Karpathy "LLM Wiki" pattern with versioned supersession.

Read [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) first for the
operational summary. Read [`docs/design-decisions.md`](docs/design-decisions.md)
for the full v1 spec. Read [`docs/research-karpathy-llm-wiki.md`](docs/research-karpathy-llm-wiki.md)
for what "Karpathy-faithful" means in practice.

## Stack (do not deviate without updating `docs/design-decisions.md` §4)

- **Runtime:** Rust 1.95 (pinned in `rust-toolchain.toml`), edition 2024,
  resolver 3, async via `tokio`.
- **MCP:** `rmcp` (official `modelcontextprotocol/rust-sdk`).
- **Store:** `rusqlite` + `refinery` migrations, FTS5 in v1, `sqlite-vec` in
  v0.2. **One file**, one writer actor, one read pool.
- **Wiki:** markdown on disk, `notify-debouncer-full` watcher with heartbeat
  + reconciliation, `git2` for versioning.
- **HTTP:** `axum` for hook ingress + MCP HTTP/SSE.
- **LLM:** typed clients per provider (Anthropic, OpenAI, OpenAI-compat) via
  `reqwest`. **Never** a generic gateway like LiteLLM (cognee #2840 lesson).
- **Config:** `figment`, one read at startup, passed by `&Arc<Config>`.
- **Logging:** `tracing` with module filters; never let the appender's own
  module log at INFO+ (agentmemory #519 lesson).

## Repository layout

```
crates/
  ai-memory-core/        # domain types, errors. NO IO.
  ai-memory-store/       # SQLite, single-writer actor, migrations.
  ai-memory-wiki/        # markdown read/write, watcher, git.
  ai-memory-mcp/         # rmcp transport + tool router.
  ai-memory-hooks/       # hook payload schemas + HTTP ingress.
  ai-memory-llm/         # LlmProvider trait + 3 impls.
  ai-memory-consolidate/ # Karpathy ingest/query/lint pipeline.
  ai-memory-cli/         # `ai-memory` binary entry point.
hooks/                   # vendored hook scripts per agent.
docker/                  # Dockerfile + compose.
docs/                    # research + design (DO NOT delete).
tests/                   # workspace integration tests.
```

## Workflow rules

1. **Milestone by milestone.** Do not start M(n+1) until every "Done when"
   bullet in M(n) passes. See [`docs/design-decisions.md`](docs/design-decisions.md)
   for the milestone list. No mixing.
2. **No dead code, no half-built features.** If a feature is not finished,
   it does not land. If you must stub something, document it as `M(n) TODO`
   in the relevant module's doc-comment with the milestone number.
3. **Tests before claiming done.** Every milestone requires:
   - `cargo fmt --all -- --check` (no diffs)
   - `cargo clippy --workspace --all-targets -- -D warnings` (no warnings)
   - `cargo test --workspace` (all green)
   - Manual exercise of the new feature against a real agent CLI when applicable.
4. **Document the why in code, not the what.** No comments restating the line
   above; only comments explaining a constraint, an incident, or a non-obvious
   invariant.
5. **Add a unit test before the implementation, not after.** Especially for
   parsers, ID derivation, and any retention/decay math.
6. **MCP tool surface changes must update every prompt surface.** When adding,
   removing, or renaming an MCP tool, update `MEMORY_INSTRUCTIONS`,
   `ai_memory_core::SNIPPET_BODY`, README/docs tool references, and regression
   tests that assert every tool appears in both prompt surfaces.
7. **Don't refactor outside the milestone.** Touch only what the current
   milestone requires; resist scope creep.

## Cross-cutting invariants (carved in, never violated)

These come straight from issue-tracker research on agentmemory, basic-memory
and cognee — every one of them is in `docs/design-decisions.md` §14 with
issue citations. **Treat any code review that violates one of these as a
blocking issue:**

1. One config-read path. No `std::env::var` outside `Config::load()`.
2. Single-writer SQLite actor. All writes through one `mpsc` channel.
3. Indexes commit in the same transaction as the data. No
   background-task-indexing-after-return.
4. Typed `(WorkspaceId, ProjectId, PagePath)` identity in every layer.
5. Hooks are fire-and-forget. Sub-second timeouts. Return 202 immediately,
   or 429 when saturated.
6. Privacy strip is a typed boundary (`RawHookPayload → Sanitized<Observation>`).
7. JSON-schema structured outputs only. No XML, no `instructor`-style wrapping.
8. `{provider, model, dim}` stored next to every embedding. Refuse on mismatch.
9. Live-process check (`sysinfo`) before any destructive op.
10. Atomic file writes (tmp + rename + fsync). Watcher ignores own writes.
11. Default data dir is an absolute canonical platform path.
    Logged loudly on startup.
12. No global singletons / `lazy_static` configs.
13. Zero-LLM default path. LLM features opt-in via env.
14. Tracing subscribers explicitly filter their own module.
15. **Every command + storage operation is namespaced by the
    project's stable surrogate `(workspace_id, project_id)` — all
    the way down to the on-disk wiki layout.** Two layers:

    a. **Surface (commands)**: the per-cwd router derives the project
    name from `basename(cwd)`; bootstrap / lint / embed /
    forget-sweep / purge-project / rename-project / consolidate /
    write-page ALL resolve through `commands::resolve_project_name`
    (or the router's `resolve_project_ids` on the server side). There
    is no generic `scratch` fallback in the happy path — `scratch`
    exists only as a defensive default for hook events that arrive
    without a `cwd` (e.g. early startup or misconfigured agents).
    Commands/handlers that bake in a `workspace_id`/`project_id` at
    construction time MUST look up the session's actual project (via
    `ReaderPool::session_project_ids`) before writing. The MCP read
    tools have no cwd of their own (the protocol carries none), so they
    resolve through `AiMemoryServer::effective_ids`: explicit `project`
    arg → the hook-published `ActiveProject` (the cwd the agent is
    working in, shared in-process between `/hook` and `/mcp`) → the
    baked-in default. Never query a read tool against the static
    `--project` alone — that re-introduces issue #2 (reads land in
    `scratch` while hooks populate the real project).

    b. **Storage layout**: wiki files live at
    `<wiki_root>/<workspace_id>/<project_id>/<page-path>`. The mutable
    project NAME never appears in any disk path; the stable UUID
    surrogate does. Consequences this rule enforces:
    - Renaming a project is a single column update on
      `projects.name`; no file moves ever.
    - Purging a project is `std::fs::remove_dir_all(project_root)` —
      atomic, no shared-file footgun.
    - Two projects can have the same `pages.path` (e.g.
      `decisions/0001.md`) without on-disk collision.
    - `log.md` and `bootstrap.md` are per-project files inside the
      project's namespaced dir, NOT shared at the wiki root.

    Every `Wiki` API call carries `(workspace_id, project_id)`
    either as explicit args (read_page, delete_page) or via
    `WritePageRequest`. No call ever reads/writes at
    `<wiki_root>/<path>` directly; always go through
    `Wiki::project_root(ws, proj).join(path)`. The filesystem
    watcher parses the first two path segments of every event as
    UUIDs and uses those to stamp the reindex — events outside the
    namespaced layout are ignored.
16. **The CLI is always a thin HTTP client to the running MCP /
    admin server.** The server is the ground truth and the sole
    writer of wiki + SQLite. CLI commands NEVER call `Store::open`,
    `Wiki::new`, `build_provider`, or `build_embedder`. State
    mutations go through `/admin/*` HTTP routes; reads go through
    MCP tools or `/admin/*` GETs. Exceptions to "always an HTTP client":
    - `init`, `generate-auth-token`, `install-*`, `setup-agent` —
      pre-server local setup (no state mutation).
    - `serve` — IS the server.
    - `llm-test` — pre-server credential smoke test (hits the external
      LLM only; no ai-memory state).
    - `reset`, `restore` — lifecycle ops that fundamentally require the
      server stopped (a live writer would race with the wipe/extract).
      Both use `sysinfo` to refuse if any sibling `ai-memory` is alive.
    Use the shared `crate::http_client::{ServerEndpoint, get_json,
    post_json}` plumbing for every new client subcommand.
17. **Wiki-structure changes require a migration.** Any change to the
    on-disk wiki layout (path scheme, directory names, mass page rewrites)
    MUST be accompanied by a `WikiMigration` implementation registered in
    `crates/ai-memory-wiki/src/migrations/mod.rs`. The migration runs at
    server startup (after refinery, before the watcher), is tracked in the
    `wiki_migrations` SQL table, and runs at most once per data directory.
    Rules:
    - Migration names are `YYYY_MM_DDTHH_MM_<snake_case>` (UTC, chosen at
      authoring time). Never change a name after it ships.
    - Implementations must be idempotent: detect whether the work is already
      done and return `Ok(())` immediately if so.
    - No LLM calls and no destructive deletes without a graveyard step
      (move to `<wiki_root>/_graveyard/<migration_name>/…` first).
    - No direct SQL — use `WriterHandle` methods (single-writer invariant).
    - Append to the registry; never reorder or remove entries.
    See [`docs/wiki-migrations.md`](docs/wiki-migrations.md) for the full
    guide and an example implementation.

## Mistakes documented in the research — do NOT repeat

- [`docs/issues-agentmemory.md`](docs/issues-agentmemory.md): install/ops
  landmines (iii-engine sidecar, distroless volumes, cwd-relative paths).
- [`docs/issues-basic-memory.md`](docs/issues-basic-memory.md): file watcher
  pain, manual-capture friction, multi-workspace retrofit.
- [`docs/issues-cognee.md`](docs/issues-cognee.md): LiteLLM/instructor wire
  drift, multi-store sync bugs, dependency landmines.

When in doubt about a design decision, search those files for the keyword.

## Quick commands

```bash
# Build everything.
cargo build --workspace

# Lint + format + test (run before every commit).
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace

# Auto-format.
cargo fmt --all

# Exercise the binary.
./target/debug/ai-memory --version
./target/debug/ai-memory init
AI_MEMORY_DATA_DIR=/tmp/x ./target/debug/ai-memory init
./target/debug/ai-memory status --json

# CI parity (requires cargo-deny + cargo-audit installed).
cargo install cargo-deny cargo-audit
cargo deny check
cargo audit
```

## What this project is NOT (v1 non-goals)

See [`docs/design-decisions.md`](docs/design-decisions.md) §13 for the full
list. Highlights: no multi-tenant auth, no web UI, no Postgres backend in v1,
no alternative vector backends, no remote sync (use `git remote` on the wiki
dir), no multimodal.

## Plan & status

The current execution plan is at
[`/home/akitaonrails/.claude/plans/cuddly-moseying-karp.md`](/home/akitaonrails/.claude/plans/cuddly-moseying-karp.md)
(local to the maintainer's `~/.claude/`).
Live progress is tracked via the TaskList tool inside Claude Code sessions.

<!-- ai-memory:start -->
## Long-term memory (ai-memory)

This project uses [ai-memory](https://github.com/akitaonrails/ai-memory)
for cross-session continuity.

**Default to the current project — always.** Every ai-memory tool
auto-scopes to the project resolved from your session's working
directory. **Do NOT pass `project` or `cwd` arguments unless the user
explicitly references a *different* project by name** (e.g. "what did we
decide in the `other-app` project?"). Phrases like "this project",
"here", "we", "our work", "where did we leave off" all mean the *current*
project — call the tool with no scoping args. If the user asks about a
handoff and the SessionStart auto-fetched block is already in your
context, just answer from it; do not re-call the tool to "find it again"
in another project.

**Lifecycle hooks already capture every prompt + tool call
automatically.** You never need to manually write routine notes; the
SessionStart hook auto-fetches pending handoffs, and on session end
ai-memory writes a session-summary page and a handoff.
LLM consolidation (compiling observations into topical wiki pages) runs
on PreCompact, on demand via `memory_consolidate`, and at session end
only when the server sets `AI_MEMORY_CONSOLIDATE_ON_SESSION_END`. Only
write a durable wiki page when the user explicitly asks to remember or
annotate something permanently.

### When to reach for each tool

The user can express any of the intents below in plain English —
match the intent to the tool. They do not need to name the tool.

| User says / situation | Tool |
|---|---|
| "have we discussed X?" / "search memory for Y" / before proposing architecture | `memory_query` |
| "what's been going on" / "show recent activity" (light) | `memory_recent` |
| "is ai-memory healthy?" / "how big is the wiki?" | `memory_status` |
| "give me the stats" / structured snapshot for the agent to consume | `memory_briefing` |
| "catch me up" / "I've been away" / "what's important right now?" / open-ended exploration | `memory_explore` |
| "where did we leave off?" — and you see a `📥 ai-memory: pending handoff` block in your context | already done — answer from that block; do NOT re-call `memory_handoff_accept` |
| "where did we leave off?" — and no such block is visible | `memory_handoff_accept` (rare; the SessionStart hook usually got there first) |
| "save context for the next session" / wrapping up | `memory_handoff_begin` (single-use handoff; terse summary; put detail in `open_questions` + `next_steps` bullets) |
| "consolidate this session" / "compile what we learned" (also runs on PreCompact; at session end only if `AI_MEMORY_CONSOLIDATE_ON_SESSION_END` is set) | `memory_consolidate` |
| "remember this permanently" / "save a note" / "add an annotation" / durable project knowledge | `memory_write_page` (write a wiki page; do **not** use handoff for permanent notes) |
| "audit the wiki" / "find contradictions" / "what rules should we add?" | `memory_lint` |
| "prune old pages" / "memory cleanup" | `memory_forget_sweep` |

`memory_explore` is the right default for the "I want to know what's
going on" use case — it returns a prose digest whose verbosity
scales automatically to how long it's been since the last activity
(< 1 h → one line; > 30 days → full catchup).

### When you write a project rule, write it here

If you're about to write a durable project rule ("always X", "never
Y", "all PRs must …"), this rules file (CLAUDE.md for Claude Code;
AGENTS.md for Codex / OpenCode / Cursor / Gemini CLI; whichever
convention your agent uses) is where it belongs. ai-memory's lint
pass surfaces the same hint automatically when a `kind: rule` page
lands in `_rules/`.

### Refreshing this snippet

This block is maintained by ai-memory. Two ways to refresh it with
the latest binary's recommended copy:

- **From the agent** (no terminal needed): ask "refresh the ai-memory
  routing in this project" — the agent calls
  `memory_install_self_routing`, picks the right filename for itself
  (Claude Code → `CLAUDE.md`; Codex / OpenCode / Cursor / Gemini →
  `AGENTS.md`), and uses its Write / Edit tool to land the block.
- **From the CLI**: `ai-memory install-instructions` (defaults to
  `CLAUDE.md`; pass `--target AGENTS.md` for non-Claude agents).

Both are idempotent: re-runs replace the block bracketed by
`<!-- ai-memory:start -->` / `<!-- ai-memory:end -->` markers
without disturbing the rest of the file.
<!-- ai-memory:end -->
