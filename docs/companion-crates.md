# Optional companion crates and projects

This page records the boundary for feature ideas that are useful around
ai-memory, but should not become core ai-memory surface area. PR #118 and PR
#123 are the historical motivation: both are valid product ideas, but both patch
too much import, chat, UI, and mutation behavior into the core server. The better
shape is optional companion software that orchestrates ai-memory through public
APIs.

ai-memory stays a memory substrate:

- one server binary owns hooks, MCP, the markdown wiki, SQLite indexes, auth,
  admission webhooks, and the built-in read-only browser;
- the wiki remains markdown-in-git as source of truth;
- SQLite remains a derived index;
- writes go through the existing wiki mutation path, admin endpoints, or MCP
  tools;
- the built-in `/web` and `/api/v1` surfaces stay read-oriented.

Companion crates can be richer products. They should integrate through public
HTTP/MCP surfaces instead of patching handlers, routes, or command surfaces into
the core workspace.

## Integration rules for companions

Companion projects may:

- call the read-only `/api/v1` endpoints for workspaces, projects, pages,
  search, graph, recent pages, and briefing/overview snapshots;
- call existing MCP tools such as `memory_write_page`, `memory_delete_page`,
  `memory_read_page`, and `memory_query` when running as an agent/client;
- call existing admin endpoints such as `/admin/write-page` and
  `/admin/delete-page` when running as an operator-side server process with an
  appropriate bearer token;
- use `--web-ui-dir` to let ai-memory serve an alternate static SPA, as long as
  the SPA still uses public HTTP APIs and does not require in-process plugins;
- run their own LLM prompts, import transforms, queues, confirmation flows,
  UI state, and project-specific policies;
- ship their own CLI binary, web server, Docker image, tests, release cadence,
  and docs.

Companion projects should not:

- become Cargo workspace members of core ai-memory by default;
- add core MCP tools, admin endpoints, or CLI subcommands unless a missing seam
  is independently useful to ai-memory itself;
- write wiki files or SQLite rows directly;
- bypass `AuthLevel::authorize`, admission webhooks, actor attribution, scope
  resolution, or the single-writer store boundary;
- require ai-memory to host arbitrary plugin code in-process.

Companion features should be treated as separate products, not rejected ideas.
They can move faster than core, have their own UX, and carry source-specific or
workflow-specific behavior without widening ai-memory's default install.

If a companion exposes browser writes, it must implement its own server-side
mutation broker. Browsers should talk to the companion; the companion should talk
to ai-memory with an operator token. That keeps CSRF, confirmation, audit, rate
limits, and UI-specific policy outside the core server.

## `ai-memory-importer`: migration and ingestion companion

This is the companion shape for PR #118. The first implemented companion lives
at [`companions/ai-memory-importer`](../companions/ai-memory-importer) as a
standalone Cargo package with its own `[workspace]`; it is not a member of the
root workspace and is not covered by root `cargo test --workspace`.

### Goal

Import or normalize existing memory corpora without making ai-memory core own
every source format and migration workflow.

Initial source support is intentionally narrow:

- oh-my-claudecode / OMC flat markdown wiki directories.
- generic external-conversation JSON envelopes (`project`, `source`,
  `session_id`, and bounded `role`/`content` messages), replayed through the
  public ordered hook-batch surface. Product-specific export adapters remain
  outside this repository.

Future sources can include:

- Claude Code memory graph exports such as `memory.jsonl` from
  `@modelcontextprotocol/server-memory`;
- Qdrant-backed memory collections, when a user supplies a collection URL and
  schema mapping;
- future one-off importers maintained on the companion's release cadence.

### Validation

Run companion checks explicitly from the repository root:

```bash
cargo fmt --check --manifest-path companions/ai-memory-importer/Cargo.toml
cargo test --manifest-path companions/ai-memory-importer/Cargo.toml
cargo clippy --manifest-path companions/ai-memory-importer/Cargo.toml --all-targets -- -D warnings
```

Root hygiene checks remain separate:

```bash
cargo fmt --check
git diff --check
```

### Product shape

Prefer a separate repository and binary crate, for example:

```text
companions/ai-memory-importer/
├── Cargo.toml
├── src/main.rs
└── README.md
```

It can share Rust libraries later only if those libraries are published with a
stable API and are useful outside ai-memory. It should not need to be a member of
this workspace.

### How it talks to ai-memory

Read and plan:

- use `/api/v1/workspaces`, `/api/v1/projects`, `/api/v1/pages`,
  `/api/v1/search`, and `/api/v1/graph` to inspect the destination;
- default to dry-run, printing planned page writes without mutating ai-memory.

Write:

- write imported or normalized pages through `/admin/write-page` or MCP
  `memory_write_page`;
- do not delete in v1;
- use `memory_query` / `memory_read_page` or `/api/v1/search` / page reads for
  duplicate detection and context checks;
- optionally call `memory_consolidate` or `memory_auto_improve` after import for
  post-import refinement, rather than building that refinement into core;
- for bulk operations, loop over the public single-page operation unless ai-memory
  later adds a generic bulk-mutation seam for its own reasons.

Re-home by kind:

- compute the move/link-rewrite plan in the companion;
- apply moves as normal writes to the new path plus deletes of the old path;
- preserve frontmatter that ai-memory returns through page reads;
- fail closed on collisions, missing pages, or changed source hashes.

### Safety requirements

- Never open ai-memory's SQLite database or wiki directory directly.
- Require an explicit destination workspace/project.
- Preserve only metadata supported by the public write surface (`title`, `kind`,
  `tier`, `tags`, `pinned`, and body) unless a future generic core seam adds
  broader frontmatter support. Do not claim arbitrary frontmatter or author
  preservation in companion imports.
- Carry idempotency keys or source fingerprints in companion-side state so failed
  imports can be resumed safely.
- Validate and sanitize a complete external-conversation envelope before the
  first HTTP request; use a stable imported session identity and per-event
  idempotency keys, and send the dedicated `external-import` wire identity
  rather than impersonating a live coding harness.
- Surface all destructive actions in dry-run output before live mode.
- Treat non-overwrite checks as best-effort unless/until core exposes a generic
  compare-and-write seam; companion v1 re-checks before each write but cannot make
  `/admin/write-page` atomic with that read.
- Keep LLM normalization optional; deterministic import should work with no LLM.
- Keep provider-specific performance tweaks, such as model parameter changes, out
  of importer PRs. If ai-memory core needs a provider bugfix or optimization,
  land it as a small standalone core change.

### Implementation plan

1. Build a read-only planner for one source format and snapshot fixtures.
2. Add dry-run output and collision detection.
3. Add live writes through existing ai-memory public write/delete surfaces.
4. Add optional LLM normalization as a companion-side pass.
5. Add re-home/link-rewrite as a separate subcommand after import is stable.
6. Only after repeated usage, consider whether ai-memory core lacks a small,
   generic API seam; do not start by patching core endpoints.

## `ai-memory-web-editor`: browser chat/editor companion

This is the companion shape for PR #123.

Status: **undecided**. Do not implement this companion yet without a fresh design
review. The useful writable version is larger than it first appears: it needs a
safe core compare-and-write seam, a companion mutation broker, browser auth/CSRF,
confirmation state, diffing, conflict handling, audit, and later LLM proposal
policy. It is not clear that this complexity brings enough benefit for
ai-memory's main goal. The system is meant to auto-improve its own memory through
capture, consolidation, review, pending writes, and eval gates; manual memory
editing may be less valuable than it seems, and could distract from improving the
automatic loop.

### Goal

Offer a richer browser product for chat, editing, and curation without turning
the built-in `/web` browser into a write-capable application.

The core built-in browser remains intentionally small: project list, tree view,
markdown rendering, search, and other read-oriented inspection. A separate web
editor can move faster and make stronger product decisions.

### Product shape

Prefer a separate repository with a backend plus frontend, for example:

```text
ai-memory-web-editor/
├── crates/server/        # auth, CSRF, mutation broker, LLM orchestration
├── crates/client/        # UI or generated assets
├── src/                  # if kept as a single binary crate initially
└── tests/e2e/
```

The companion can be deployed next to ai-memory and reverse-proxied under a
separate path or host, for example `https://memory.example.com/editor`, while
ai-memory remains at `/api/v1`, `/mcp`, `/admin`, `/hook`, and `/web`.

### How it talks to ai-memory

Read:

- use `/api/v1` for project lists, pages, recent pages, search, graph, briefing,
  and overview data;
- use the companion's own LLM provider for chat orchestration if it needs more
  than raw page/search context.

Write:

- browser requests go to the companion backend, not directly to ai-memory admin
  routes;
- the companion backend performs CSRF checks, user/session policy, rate limiting,
  and confirmation state;
- after approval, it calls ai-memory's existing write/delete surfaces with a
  server-side token.

Mutation flow:

1. The LLM proposes a patch, create, or delete as a pending action.
2. The UI shows an explicit diff and the target workspace/project/path.
3. The user confirms or rejects the pending action.
4. The companion re-reads the current page and verifies the expected base hash.
5. The companion applies the write/delete through ai-memory's public mutation
   path and records its own audit trail.

### Safety requirements

- No auto-applied browser writes from an LLM response.
- Deletes always require explicit confirmation.
- Edits preserve existing metadata unless the user deliberately changes it.
- Folder or search scope is a context limit, not a mutation boundary; the backend
  must independently authorize the target page before applying a change.
- If the UI advertises folder-scoped editing, the companion must enforce that
  target paths stay inside the allowed folder or project on the server side.
- The companion must not rely on cookie/basic auth to perform non-GET ai-memory
  mutations from the browser. Use a server-side token and companion CSRF/session
  protection.
- In multi-user mode, `/admin/*` is root-only. A companion must either run with an
  operator token or use MCP/tooling flows appropriate to the actor; it must not
  assume normal user tokens can admin-write.
- Propagate actor/author context where the public write surface supports it so
  admission webhooks and audit stay meaningful.
- Keep `/api/v1` read-only; do not ask core ai-memory to expose writable CORS
  browser endpoints for this product.

### Implementation plan

This plan is intentionally parked until the benefit is clearer.

1. Build a read-only editor shell against `/api/v1` first.
2. Add chat over selected page/search context, still read-only.
3. Add pending edit proposals with diff preview, but no apply button.
4. Add confirmed writes through the companion backend and ai-memory public write
   endpoints.
5. Add confirmed deletes last.
6. Keep the built-in `/web` UI unchanged unless core ai-memory independently
   needs a small read-only API enhancement.

## Two kinds of companion: data-seam vs. independent hook consumer

`ai-memory-importer` and `ai-memory-web-editor` are *data-seam* companions: they
talk to ai-memory's public HTTP/MCP surfaces and build on the data it stores.
Not every adjacent tool is that shape.

**Working-tree coordination is out of core, and is an *independent hook
consumer*, not a data-seam companion** (decided in #620). Several agent sessions
sharing one checkout collide over the single git index — one session's
`git add .` sweeps up another's staged work, a `--fix` run rewrites an unclaimed
tree — and the natural instinct is to build the guard on ai-memory's captured
`PreToolUse`/`PostToolUse` signal. That does not work, for two deliberate
reasons:

- **ai-memory cannot block a tool action.** The `/hook` path is capture-only and
  fire-and-forget (it returns `202`/`429`, never allow/deny/ask, and processes
  after responding). Hooks that await a REST round-trip can deadlock the engine
  (agentmemory #221) — so there is no synchronous veto channel back to the
  harness, by design.
- **ai-memory does not retain the file paths.** For closed-tool agents the
  stored observation is reduced to a `tool_family` label plus outcome; raw
  arguments, paths, and tool names are extracted only transiently for
  denylist matching, then dropped (`CaptureDecision` "never retains raw
  arguments, paths, or arbitrary tool names"). A consumer can see *that* a file
  op happened in a session, never *which file*.

Reversing either — persisting paths, or adding a blocking hook — trades away a
privacy/bounding invariant or the anti-deadlock invariant. Both stay.

So a working-tree coordinator installs its **own** `PreToolUse` hook alongside
ai-memory's, reads the raw `tool_input`, and arbitrates synchronously in its own
process with its own ephemeral ownership state (a lock file or small store —
never the wiki or SQLite). It is a *sibling on the same hook event*, not a
seam-consumer. It may still live under `companions/` for discoverability, but it
depends on the harness's hook mechanism, not on ai-memory's surfaces. Scope it
to the shared-index / file-ownership class; stale-tree builds and host
saturation are build/CI-orchestration, a separate problem.

## When to move a seam into core

A companion may reveal a missing primitive that belongs in ai-memory. Move only
small, generic seams into core, and only after the companion proves the need.

Good core candidates:

- a read-only API field needed by several clients;
- a narrowly-scoped mutation endpoint that is equivalent to an existing MCP tool;
- a capability check or scope-resolution helper that prevents duplicated security
  logic.

Poor core candidates:

- source-specific import parsers;
- UI workflows;
- LLM chat prompts for editing;
- project-specific scoring, pruning, or normalization policies;
- companion-only admin commands.

This keeps ai-memory stable while still allowing richer tools to grow around it.
