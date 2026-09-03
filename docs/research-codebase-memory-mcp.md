# codebase-memory-mcp — Research Report

*Repo:* <https://github.com/DeusData/codebase-memory-mcp> · MIT · pure **C**,
single static binary. *Snapshot:* 2026-09-03 — ~42k stars, ~3.4k forks,
created 2026-02, pushed daily. Preprint: *"Codebase-Memory: Tree-Sitter-Based
Knowledge Graphs for LLM Code Exploration via MCP"* (arXiv:2603.27277).

> **Category caveat, up front.** codebase-memory-mcp ("cbm") is **not a
> session-memory competitor** — it is *code-structure* memory. It answers
> *"what is this code and how does it connect?"*, ai-memory answers *"what did
> we do and decide, and why?"*. They are **complementary**: an agent can run
> both — cbm to navigate the code, ai-memory to recall the work. This report
> reads it for insights, not as a rival.

## 1. Purpose & Scope

*"The fastest and most efficient code intelligence engine for AI coding
agents."* It parses a repository into a persistent knowledge graph so an agent
can ask structural questions (call chains, routes, impact of a change) with a
single graph query instead of dozens of grep/read cycles. Headline claim:
**~120× fewer tokens** — *"5 structural queries: ~3,400 tokens vs ~412,000 via
file-by-file search"* (99.2% reduction) — and it indexes the Linux kernel
(28M LOC) in ~3 minutes, answering Cypher queries in <1ms.

## 2. Architecture

- **Indexing:** multi-pass **tree-sitter** AST over 162 languages, plus a
  *"lightweight C implementation of language type-resolution algorithms"*
  ("Hybrid LSP") that resolves imports/generics/inheritance/type inference
  tree-sitter alone can't — structurally compatible with tsserver / pyright /
  gopls / Roslyn / rust-analyzer but embedded and offline.
- **Pipeline:** *"RAM-first: LZ4 compression, in-memory SQLite, fused
  Aho-Corasick pattern matching. Memory released after indexing."*
- **Storage:** **SQLite** at `~/.cache/codebase-memory-mcp/` (a familiar
  choice — ai-memory is single-SQLite too).
- **Graph model:** nodes (Project, Package, File, Function, Class, Route, …)
  and edges (`CALLS`, `IMPORTS`, `IMPLEMENTS`, `HTTP_CALLS`, `DATA_FLOWS`,
  `SIMILAR_TO`, `SEMANTICALLY_RELATED`), with **bundled `nomic-embed-code`
  embeddings (768d int8) compiled into the binary** for `semantic_query`.
- **Zero runtime, zero network:** pure C, vendored grammars + embeddings
  compiled in, *"makes no network request of its own accord"* — no telemetry,
  no version check. All local.

## 3. Agent Interface (15 MCP tools)

`search_graph`, `trace_path` (BFS call chains, depth 1–5), `query_graph`
(read-only openCypher subset), `get_architecture` (languages/routes/hotspots/
clusters in one call), `detect_changes` (git diff → affected symbols with risk
classification), `semantic_query` (vector search over the graph),
`search_code`, `get_code_snippet`, `ingest_traces` (runtime-trace validation of
`HTTP_CALLS`), `manage_adr` (Architecture Decision Records CRUD), plus
`index_repository` / `index_status` / `list_projects` / `delete_project` /
`get_graph_schema`.

## 4. Freshness & Team Sharing

- **Auto-sync watcher** re-indexes on file change (`auto_watch` default true) —
  the same "index stays live" instinct as ai-memory's wiki watcher.
- **Committed, compressed derived index:** *".codebase-memory/graph.db.zst is
  a zstd-compressed snapshot of the knowledge graph"* checked into the repo;
  new clones decompress and run incremental indexing instead of a full
  reindex. Two-tier compression (`zstd -9` + `VACUUM INTO` on explicit index,
  `zstd -3` for the watcher's low-latency updates).
- **Session-coordination daemon:** one per-account daemon shared across ~45
  agent surfaces — first session starts it, each registers its work, last one
  shuts it down.

## 5. Distinctive Ideas Worth Noting

- **Committing a compressed *derived* index for team sharing** (`graph.db.zst`
  in-repo). ai-memory deliberately keeps its DB derived/rebuildable and shares
  through the **server** instead; cbm's git-artifact model is the offline,
  server-less counterpart — a real trade-off to keep in mind (see the ECC and
  landscape notes on git-shared vs server-mediated).
- **Multi-tier agent profiles** — auto-generated **Scout** (fast discovery) /
  **Verify** (task-directed evidence) / **Auditor** (exhaustive) profiles,
  each with an *"exact per-tier graph-tool list"* to **prevent
  over-permissioning**. This is the most transferable idea: a principled way
  to expose *different subsets* of tools to different task phases.
- **`detect_changes` → risk classification** — mapping a git diff to affected
  symbols and a risk score. Adjacent to ai-memory's typed-edge/contradiction
  lint, but over *code* rather than *knowledge*.
- **Token-efficiency as the headline metric**, benchmarked and published — the
  same discipline ai-memory applies with LongMemEval; cbm makes "queries vs
  grep tokens" its north star.

## 6. What Good / What's Missing — Honest Take

**Not a competitor — a complement.** cbm indexes the *codebase*; ai-memory
indexes the *work*. The clean division of labour: cbm knows the code as it is
*right now*; ai-memory knows the decisions, gotchas, sessions, and handoffs —
the *why* and the *history* that no AST carries. An agent wanting both "where
is this function called" and "why did we build it this way" needs both tools.

**Ideas ai-memory could borrow:**
1. **Tiered tool exposure** (Scout/Verify/Auditor). ai-memory ships ~18 MCP
   tools to every client at every phase; a task-phase-scoped subset (e.g. a
   read-only "recall" tier vs a full "curate" tier) would cut prompt surface
   and over-permissioning — worth considering alongside the rules-promotion
   work (`docs/design-rules-promotion.md`).
2. **The token-efficiency framing** as a first-class, published metric for the
   *retrieval* side (ai-memory already benchmarks recall; "tokens to answer"
   is the complementary axis).
3. **A committed, compressed derived-index option** for server-less small
   teams — the same idea flagged in `research-ecc.md`'s git-shared team scope.

**Where ai-memory is simply doing a different (harder-for-its-domain) job:**
- Automatic, sanitized **session capture** and cross-agent **handoff**; LLM
  **consolidation** into human-readable knowledge; **temporal `as_of`**; a
  **server** for multi-user/multi-machine. None of that is cbm's problem —
  cbm never watches a session; it watches files.

**Bottom line.** codebase-memory-mcp is an excellent, narrowly-scoped
code-intelligence engine and a natural companion to ai-memory rather than a
rival. Its best lessons for us are operational, not architectural: **tiered
tool profiles**, a **published token-efficiency metric**, and (optionally) a
**git-committed compressed index** for teams that won't run a server.

## 7. 2.1 Feasibility — grounded in ai-memory's code

Each borrowable idea was checked against the current tree so the release call
rests on real choke points, not analogy. Verdicts: one **2.1 feature**, one
**land-now dev metric**, one **deferred design**.

### 7.1 Tiered tool profiles — **recommend for 2.1**

*Where it plugs in.* The MCP server exposes **19 tools** through the rmcp
`#[tool_router]` macro (`crates/ai-memory-mcp/src/server.rs:1239`), and
`list_tools` (`server.rs:3966`) is the **single choke point** — it returns
`tool_router.list_all()` unfiltered, with only a per-dialect schema *reshape*
(`restricted_schema_tool_list`, `server.rs:4021`) that never drops a tool. A
read/write partition **already exists**: `tool_call_is_write` (`server.rs:757`)
classifies 8 tools as read-only (query, read_page, read_session_observations,
recent, briefing, explore, status, install_self_routing) — today used only for
rate-limit accounting, not visibility.

*Why 2.1.* 19 tools is heavy MCP prompt surface pushed to every client every
turn. A **read-only "recall" tier** (the 8 already-classified tools) versus the
full "curate" tier cuts prompt surface and over-permissioning for the common
case (an agent that only *recalls* never needs `delete_page`/`forget_sweep`).
It is small, self-contained, and **pairs directly with the rules-promotion
work** (`docs/design-rules-promotion.md`) — both are about giving the agent
*less, better-scoped* authority by default.

*Shape.* Add a `--tool-profile recall|full` flag to `ServeArgs`
(`crates/ai-memory-cli/src/cli.rs:2119`), or a per-request `?profile=` marker
mirroring the existing `?flavor=` mechanism (`server.rs:3980`); filter
`list_all()` by `tool_call_is_write` before returning. No new classification
logic — the partition is already written.

### 7.2 Token-efficiency metric — **land on main now (dev tooling, not a gated feature)**

*Where it plugs in.* The `evals/` crate measures **hit@k / recall@k only**
(`evals/src/retrieval/score.rs`). The full `memory_query` JSON payload is
already materialized in exactly one place before parsing —
`evals/src/retrieval/query.rs:114` (`text` holds the complete content string) —
so `text.len()` / a tokenizer pass yields a per-question "tokens to answer"
number with **zero server changes**, threaded into `report.rs`.

*Why not gated to 2.1.* It changes no runtime behavior and ships nothing
user-facing — it is an eval/benchmark improvement that strengthens our
measurement story (the complement to LongMemEval recall: *recall* says we find
the answer, *tokens-to-answer* says we deliver it cheaply). It can go to `main`
as a quality improvement whenever, independent of the 2.1 train.

### 7.3 Committed compressed derived-index — **defer (design-only until a server-less use-case exists)**

*Where it plugs in.* The DB is **contractually derived and rebuildable**:
markdown-in-git is the source of truth, SQLite `db/` is the index
(`crates/ai-memory-wiki/src/lib.rs:3`, `docs/companion-crates.md:14`), with a
working rebuild (`ai-memory reindex`, `crates/ai-memory-cli/src/commands/reindex.rs`)
and an online-backup tarball producer (`POST /admin/backup`,
`crates/ai-memory-mcp/src/admin.rs:746`). So a committed `graph.db.zst`-style
artifact *fits the model* mechanically.

*Why defer.* It is a **different distribution philosophy**, not a small
feature. ai-memory's multi-user/multi-machine story is deliberately
**server-mediated** (attributed writes, per-user scope, live handoff); a
git-committed binary index is the *server-less* counterpart, and it drags in
snapshot-consistency, staleness, regeneration cadence, and binary-blob merge
conflicts. Building it before there is a concrete server-less-small-team
customer would be speculative — it belongs in the same "consider a git-shared
team scope" bucket as `research-ecc.md` §8.4, to be designed only if that path
is chosen.

### Recommendation summary

| Insight | Verdict | Effort | Where |
|---|---|---|---|
| Tiered tool profiles | **2.1 feature** | small (choke point + existing partition) | `release/2.1` |
| Token-efficiency metric | **land now** | low (additive, one choke point) | `main`, any time |
| Committed compressed index | **defer** | large (distribution model) | design-only, needs use-case |

### Sources
- <https://github.com/DeusData/codebase-memory-mcp> (README, metadata via GitHub API)
- Preprint arXiv:2603.27277 — *Codebase-Memory: Tree-Sitter-Based Knowledge Graphs for LLM Code Exploration via MCP*
