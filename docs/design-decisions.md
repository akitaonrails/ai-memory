# ai-memory - Design Decisions (Synthesis)

> Historical rationale distilled from the original research and issue-tracker
> reports. For the current operational map, read
> [`ARCHITECTURE.md`](ARCHITECTURE.md); for current client support, use the
> [README Support Matrix](../README.md#support-matrix). The research files are
> the historical receipts.

## 1. Product shape

A self-contained Rust binary that:

1. Runs as an **MCP server** (stdio + HTTP/SSE) for coding-agent CLIs (Claude Code, OpenAI Codex, Cursor, Gemini CLI, Antigravity CLI, OpenClaw, OpenCode, OMP, and MCP-capable clients).
2. Captures sanitized, bounded lifecycle observations **automatically** - no `write_note` ceremony - via hook scripts or generated extensions that agent CLIs invoke. Optional `ai-memory run` workstreams additionally read visible native transcript tails through host-side, read-only adapters.
3. Maintains a **Karpathy-style wiki**: incrementally-compiled markdown pages with cross-links, supersession, an `index.md` and a `log.md`.
4. Serves retrieval via the MCP `tools/list` to coding agents: a handful of *narrow* tools, not 50.
5. Ships a **Docker image** (`docker run -v ai-memory-data:/data -p 49374:49374 ai-memory`) so it can move between desktop and homelab.
6. Is *self-healing*: schema migrations on startup, vector-index dim/provider check, write-ahead durability, periodic integrity audit, single-writer queue to avoid `database is locked`.

## 2. Hard requirements (extracted from the prompt)

- Rust, clean architecture, modular, unit-tested.
- Cargo-format clean.
- Docker-deployable, easy backup, easy move desktop↔homelab.
- MCP server for coding agents.
- **Automatic** memory capture/fetch - minimal manual tool invocations.
- Differentiates **short-term** vs **long-term** memory temporally (like agentmemory).
- Self-healing memory management.
- Helps with handoffs between agent CLIs (resume from Codex where Claude Code left off).
- Iteratively planned - each feature working before the next starts. No dead code.

## 3. Storage model - the biggest architectural decision

Three options surveyed:

| Option | Source-of-truth | DB used for | Pros | Cons |
|---|---|---|---|---|
| **A. DB-primary** | SQLite | Everything | Single transaction boundary, fast search, no FS race conditions | Opaque to humans; harder backup story |
| **B. Markdown-in-git** primary | Files in repo | Derived index | Diff-able, grep-able, portable, Karpathy-faithful | Watcher correctness (basic-memory #580/#758/#798), inode races (#765), startup cost |
| **C. DB-primary with on-demand export** | SQLite | Everything | Best of both | Two formats to keep coherent; user must remember to export |

**Decision: Option B - markdown in a git repo is source of truth, SQLite is derived index.**

**Why:**
- Backup/move story is trivial - `git clone` or `rsync` a directory. The user explicitly asked for this.
- Karpathy's pattern *is* the wiki on disk. Faking it with an export step loses the inspect-in-Obsidian property.
- DB is rebuildable from files - corruption is recoverable.
- Cross-tool compatibility for free: any agent that reads `~/.ai-memory/wiki/*.md` works without an MCP integration.

**How we avoid basic-memory's watcher pain:**
- Watcher has a heartbeat + reconciliation pass (full diff every 30s to catch missed events).
- We *own* writes through the MCP server's `wiki_write` path; the watcher is a *safety net* for external edits, not the primary input.
- Inode-locking advisory + psutil-style live-process check before destructive ops (`reset`, `purge`). Lesson from basic-memory #765/#776.
- Hidden-directory paths handled explicitly (basic-memory #798).

**How we avoid the "files-and-DB drift" overhead:**
- DB stores `(path, mtime, size, sha256, indexed_at, provider, model, dim)` per page. On startup, fast scan vs. cached SHAs; only changed files re-parsed.
- Embeddings keyed by `sha256(content) + provider + model + dim`. Re-embed only when content changes.

**Consistency contract:** markdown is primary and SQLite is derived. There is no
real cross-resource transaction between the filesystem and SQLite. Wiki writes
must go through `Wiki::write_page`, `Wiki::apply_batch`, or the existing
destructive helpers so sanitization, admission, attribution, rollback, and
store updates stay together. Runtime store failures roll installed files back
best-effort; crash windows are resolved by the existing markdown reindex path.
Handlers must not write wiki files directly.

## 4. Database choice - single SQLite file

**Decision: one SQLite file with FTS5, packed-vector embeddings, and SQL tables for graph edges.**

Why not Postgres/pgvector? Cognee's #2717 and basic-memory's #830/#831 show Postgres is a real-deployment-only pain. v1 ships embedded.

Why not LanceDB/Qdrant/Kuzu/CozoDB/SurrealDB?
- LanceDB: cognee #2702/#2720 (file-format drift, filter propagation failures). Pyarrow underneath.
- Kuzu / Ladybug: cognee #2098/#2768 (upstream archived, fork-risk realized).
- CozoDB: small bus factor.
- SurrealDB: heavy, multi-mode storage; we'd inherit a lot of surface we don't need.
- Packed vectors in SQLite keep v1 dependency-light; `sqlite-vec` remains the scale-up path once brute-force cosine stops being enough.

**The graph is just SQL tables.** A `wiki_pages` table, a `wiki_links (from_id, to_id, link_type)` table, optional `wiki_concepts (page_id, concept)`. Graph queries are recursive CTEs in SQLite. Petgraph in-memory for batch traversals. Avoids the entire "embedded graph DB" footgun cognee fell into.

**Crates** (research-backed picks):
- `rusqlite` for embedded SQLite access. `bundled-sqlcipher` if we want encryption later.
- `refinery` for SQL migrations.
- `tantivy` *not* used initially - sqlite FTS5 is sufficient at the corpus sizes we expect (hundreds to low-thousands of pages per project). Revisit only if FTS5 ranking proves inadequate.
- `petgraph` for in-memory graph algorithms during consolidation.

## 5. Embedding & LLM

**Embeddings:**
- Default: **local model via `ort` (ONNX Runtime) crate or `fastembed-rs`** running `bge-small-en-v1.5` (384 dim) or `bge-small-en-v1.5-q` quantized. Same model basic-memory uses.
- Persist `{provider, model, dim}` next to every vector. On mismatch, warn and ignore stale vectors until `ai-memory embed --force` or scheduled backfill re-embeds them (agentmemory #469 lesson, without blocking startup).
- Cache path: `<data_dir>/models/`, never `/tmp` (basic-memory #741).
- Trait-based: `trait Embedder { ... }` with implementations `LocalOrtEmbedder`, `OpenAIEmbedder`, `VoyageEmbedder`. User configures one.

**LLM for consolidation passes:**
- **Off by default**, behaves like agentmemory after #138's fix. Without a provider, the system still works: synthetic compression (rule-based), no LLM-generated summaries, no `memory_consolidate` page-rewrite.
- With a provider, LLM consolidation runs on PreCompact, on demand via `memory_consolidate`, and at session end only when `AI_MEMORY_CONSOLIDATE_ON_SESSION_END=true` (off by default — session end always writes a rule-based summary page + handoff regardless). Optional 6h maintenance timer.
- Provider trait `LlmProvider { complete(...); complete_structured(...) }`. Implementations: `AnthropicProvider`, `OpenAIProvider`, `GeminiProvider`, `OpenAICompatProvider` (the latter covers Ollama / vLLM / LM Studio and supersedes the earlier `OllamaProvider`).
- **Native HTTP per provider** - no LiteLLM-equivalent. The cognee tracker (#2412/#2430/#2537/#2608/#2749/#2782/#2840/#2842) showed silent-kwarg-drop in a generic gateway is the #1 source of provider bugs. Each provider's typed JSON, errors on unknown fields. Hand-coded but correct.
- **Structured output via JSON schema, not XML, not Instructor-style wrapping.** Use each provider's native JSON-mode where available; for Anthropic, request a tool-use response with a typed schema. Validate with `serde_json` + `schemars`-derived schemas.

## 6. Capture model - auto, never `write_note`

Three capture surfaces, in priority order:

1. **Lifecycle hooks/extensions** (Claude Code, Codex, Cursor, Gemini CLI, Antigravity CLI, OpenClaw, OpenCode, OMP). These are fast, reliable, structured. We ship hook scripts or generated TypeScript integrations the user installs once. Lessons from agentmemory:
  - Hooks must be **fire-and-forget** (#221). No `await fetch()` blocking session start.
  - Sub-second hard timeouts on the writer side (`tokio::time::timeout`).
  - All hooks → single HTTP/Unix-socket POST → server queues → returns 202
    immediately, or 429 when saturated.
  - Privacy strip at the hook boundary, not later (agentmemory `stripPrivateData`).

2. **Transcript tail** (universal fallback). Watch `~/.claude/projects/`, `~/.codex/`, `~/.config/opencode/sessions/`. Lossier but works for any agent. Required for the basic-memory #669/#687/#730 demand the tracker has been asking for.

3. **Manual MCP tool** (`memory_remember`) - only for ad-hoc explicit captures from the user ("remember this"). Not the primary path; not what the agent reaches for by default.

### Capture-policy boundary (#194)

The nearest `.ai-memory.toml` may use `[capture] ignore_paths` to exclude
recognized file-tool events before client spool or transport. This is a strict,
schema-specific lexical boundary, not a general content or DLP filter: private
patterns and candidates never leave the client, but shell/patch text, aliases,
and non-path-attributable bodies remain outside its scope. The authoritative
grammar, limits, supported integrations, and `--check-capture` affordance are
in [the marker-file reference](marker-file.md#capture-exclusions). It adds no
MCP tool and needs no DB migration; new-client/old-server is safe, while old
clients retain their previous capture behavior.

## 7. Memory model (temporal)

Adopt agentmemory's tier model **but** keep the surface narrow:

| Tier | What it is | Lifetime | Decay |
|---|---|---|---|
| **Working** | Current session: last N observations, last user prompt, current files | Until session end | Drop on session end (kept in DB for forensics, but excluded from default recall) |
| **Episodic** | Per-session summaries with concept tags, files-touched, decisions made | 30 days hot, 180 days cold, then evict if cold-score < threshold | `salience · exp(-λ · age_days) + σ · log(1 + access_count) · exp(-μ · days_since_access)`. Code of record: [`crates/ai-memory-store/src/decay.rs`](../crates/ai-memory-store/src/decay.rs). |
| **Semantic** | Distilled facts/preferences/architecture notes - the wiki pages themselves | Indefinite, supersedeable | Versioned in place: old `is_latest=false`, new `supersedes=old_id` |
| **Procedural** | Repeated patterns extracted from episodic clusters (`pattern` type with frequency ≥ 2) | Indefinite | Frequency-decay if not re-observed in N days |

**Implementation note:** the four tiers map to one `pages` table with a `tier` enum column + an `observations` table for raw working/episodic, not four separate tables. Keeps schema migrations sane.

## 8. Consolidation (the Karpathy bit)

Three scheduled MCP operations:

- **`memory_ingest`** (auto-called by hooks): one observation → write-fan-out to ~5–15 wiki pages. New page if no match; supersede + version if the page already exists. No-LLM fallback: append to a per-day digest page if no provider configured.
- **`memory_query`** (called by agent on demand): hierarchical - search `index.md` first, then page-level FTS+vector, then optional graph-walk expansion. RRF-fused. Agentmemory hit 95.2% R@5 with this pattern.
- **`memory_lint`** (scheduled hourly + on session-end): scans for contradictions, orphan pages, broken links, stale claims, low-confidence + zero-reinforcement entries. Pure LLM with strict JSON output.

Decay/forget runs as a separate `memory_forget_sweep` job: applies the retention formula; soft-deletes via `is_latest=false` + `superseded_at`; hard-deletes only after 180 days *and* zero accesses. Never silently destroys anything user-pinned.

Auto-improvement work stays separate from normal session consolidation. The
reviewer notes and staged design are in
[`docs/auto-improvement-loop.md`](auto-improvement-loop.md). The short version:
learning review is scheduled for newly completed sessions in every project when
an LLM provider is configured, and manual CLI/admin/MCP runs remain available for
catch-up or targeted reruns. Scheduling and approval are separate: `[auto_improve.scheduler]`
controls background review, while `[auto_improve] require_approval = true` keeps
scheduled and manual proposals pending for human approval instead of applying
them automatically. All writes remain scoped through the shared resolver/auth
paths and must not mutate the active agent context mid-turn.

## 9. Cross-agent handoff

A first-class typed protocol, shared state:

```rust
struct Handoff {
    from_agent: String,   // "claude-code", "codex"
    to_agent: Option<String>,
    project_id: ProjectId,
    cwd: PathBuf,
    summary: String,
    open_questions: Vec<String>,
    files_touched: Vec<PathBuf>,
    next_steps: Vec<String>,
    model: String,
    created_at: DateTime,
}
```

MCP tools `memory_handoff_begin` (writes a handoff row tagged `state=open`), `memory_handoff_accept` (acknowledges, returns the handoff content, marks `accepted_by`), and `memory_handoff_cancel` (marks an exact open handoff id expired when it was created by mistake). The user can stop Claude Code, start Codex, and Codex's session-start hook fetches the open handoff for the cwd. The cwd is matched by path-boundary (the prior art's check), not exact equality: a handoff left in `/repo` is delivered to a session in `/repo/api`, but never to `/repo-other`. A manual `memory_handoff_begin` handoff is stored with no cwd and so is project-wide, and is preferred over the auto SessionEnd handoff (then the most specific cwd, then the most recent) so an explicit "where we left off" baton is never shadowed by the heuristic one.

agentmemory has this informally (`/handoff` skill); we make it explicit from day one because every research report flagged cross-agent as the v0.1 weak spot.

## 10. MCP tool surface - narrow on purpose

basic-memory has ~25 tools, agentmemory has 53. Both have user confusion as a result. The current v1 surface is still deliberately narrow:

| Tool | Purpose | Annotation |
|---|---|---|
| `memory_query` | Search + retrieve, FTS5 + optional hybrid RRF | read-only |
| `memory_recent` | Most-recently-updated `is_latest=1` pages for the project | read-only |
| `memory_status` | Health, counts, last-consolidation-at | read-only |
| `memory_briefing` | Structured zero-LLM snapshot: 7d/30d windows, pending handoffs, recent pages, `_rules/` | read-only |
| `memory_explore` | LLM-composed prose digest over `memory_briefing`; degrades to JSON without a provider | read-only |
| `memory_handoff_begin` | Mark session boundary, write handoff | destructive |
| `memory_handoff_accept` | Fetch + ack the latest open handoff | destructive |
| `memory_handoff_cancel` | Mark an exact mistakenly-created open handoff expired | destructive |
| `memory_consolidate` | LLM-driven page rewrite (`multi_page=true` for atomic fan-out) | destructive |
| `memory_auto_improve` | Manual learning review for a completed session; the server also schedules review for new sessions, and manual-review opt-in keeps proposals pending | write |
| `memory_write_page` | Write durable wiki knowledge on explicit user request | destructive |
| `memory_read_page` | Read a full page body by exact path or top search hit | read-only |
| `memory_delete_page` | Delete a single exact-path page with admission hooks | destructive |
| `memory_forget_sweep` | Retention sweep (M8); soft-delete below cold threshold; `dry_run=true` previews | destructive |
| `memory_lint` | Rule-based + optional LLM contradiction findings → `wiki/_lint/<date>.md` | destructive |
| `memory_install_self_routing` | Returns the canonical slim CLAUDE.md / AGENTS.md routing block, managed Agent Skill payloads, target hints, and overwrite guidance | read-only |

Tool param aliases stay narrow: shipped aliases cover `query|q|search` and
`limit|n|top_k`; project and cwd parameters use canonical names unless the
code adds a concrete alias.

Managed ai-memory Agent Skills are prompt packaging for this tool-routing
guidance only. They are installed as ordinary `SKILL.md` files so agents can
progressively load detailed instructions, but ai-memory does not store durable
memory in them and does not include a runtime skill router.

## 11. Identity & project scoping (3-tuple from day one)

Lesson from basic-memory's v0.20 trauma: `(workspace, project, page_path)`. Even if v1 ships single-workspace, the schema and every API/tool param encodes the full 3-tuple. No retrofits.

Project resolution chain: explicit param → server's default → cwd-based heuristic (match repo root) → error.

**Install-time `project_strategy` default (#128).** `basename(cwd)` stays the v1 default, but an agent shell that `cd`s into a subdirectory and stays there silently forks the rest of the session into a phantom project named after the subdir. A `.ai-memory.toml` marker with `project_strategy = "repo-root"` fixes this (#16, #23, #111) but needs a marker in (or above) every repo; a runtime env-var fallback that the *user* sets was deliberately rejected in #16. `install-hooks --project-strategy repo-root` instead **bakes** the strategy into the generated hook command (and the OpenCode / OMP / OpenClaw plugins) at install time — the same status as the already-baked `AI_MEMORY_AUTH_TOKEN` / `AI_MEMORY_HOOK_URL` / `--data-dir`, not a user runtime override. This is a client/install-time-only change: the server already parses `project_strategy=repo-root`. A marker's own `project_strategy` / `project` still win, and the default stays `basename` (baking nothing) so existing installs are byte-identical.

## 12. Operability

- **Single binary**, statically-linked where possible. Distroless Docker image. **Absolute data path** by default (`dirs::data_local_dir().join("ai-memory")`); log it loudly on startup (agentmemory #303 lesson).
- **Atomic config**: one `Config::load()` → typed struct, every reader takes `&Config`. No `process.env` double-read paths (agentmemory #456/#469).
- **Write durability**: accepted hook work awaits the SQLite write and appends a
  `log.md` line before that background task finishes. Indexes still commit in
  the same transaction as the data; no detached indexing task runs after the
  write ack (basic-memory #763/#578/#839).
- **Migrations**: `sqlx::migrate!` runs on startup; never inline DDL (basic-memory #727).
- **Schema versioning**: one source of truth for the schema; derived clients/docs. No "update 7 files" checklists (agentmemory AGENTS.md smell).
- **Backup/move**: `ai-memory export <dir>` dumps wiki/ + sqlite snapshot. `ai-memory import <dir>` consumes. Default data dir is portable. Optional: `auto_git_commit = true` config flag → commits the wiki directory on every `memory_lint` run.
- **Self-healing**: startup checks (`memory_diagnose`): vector dim/provider drift, FTS index corruption, orphan pages, broken links, zombie sessions. `memory_heal` auto-fixes the safe subset.
- **Logging**: structured `tracing` with rotating files, capped at N MB. No feedback loops (agentmemory #519).

## 13. Original v1 exclusions (historical)

The initial scope excluded the following. Some, including multi-user auth, the
web UI, and managed Agent Skills, shipped after v1; this list is retained as the
historical decision boundary rather than a current support matrix.

- No multi-tenant auth/RBAC (single-user homelab).
- No web UI / dashboard (use `sqlite3` + `glow`/Obsidian).
- No Postgres backend (revisit if a real homelab user hits scale walls).
- No remote/cloud sync (use git remote on the wiki dir).
- No alternative embedded vector backends (sqlite-vec only).
- No alternative graph DB (SQL recursive CTEs only).
- No multimodal (text only).
- No general "skills" / slash-command bundle in v1 (agentmemory plugin format). The narrow exception is the managed ai-memory Agent Skills that package routing guidance for agents; hooks + MCP remain the product surface.
- No LongMemEval-style benchmark harness in v1 - add in v0.4.

## 14. Mistakes-to-avoid checklist (from issue research)

Top-line rules carved into the codebase:

1. One config-read path (agentmemory #456/#469).
2. Indexes in the same txn as the source-of-truth row (agentmemory #204/#309, basic-memory #763/#578).
3. JSON-schema structured outputs, no XML (agentmemory #492/#539; cognee #2840).
4. Hooks fire-and-forget (agentmemory #221, #143).
5. No background-task index-after-return; either sync or `index_status: pending` (basic-memory #763).
6. 3-tuple identity from day one (basic-memory #783/#834).
7. Vector index records `{provider, model, dim}`; ignore stale vectors and warn on mismatch (agentmemory #469).
8. Embedding cache path absolute, not `/tmp` (basic-memory #741).
9. Watcher heartbeat + reconciliation pass (basic-memory #580/#758/#798).
10. Live-process check before destructive ops (basic-memory #765).
11. Per-provider typed HTTP client; no LiteLLM equivalent (cognee #2840).
12. Idempotent ingest with deterministic id derivation (cognee #2510/#2557/#2633).
13. Single transactional boundary; no implicit graph/vector/relational sync (cognee Section B).
14. Filter propagation tests (cognee #2720 was a recall correctness bug).
15. Default data dir is an absolute canonical platform path (agentmemory #303).
16. No `lru_cache` on configs (cognee #2228/#2853).
17. Datasets/projects are query-time filters, not orchestration-mode-conditional (cognee #2867).
18. LLM has off by default; opt-in via env (agentmemory #138/#143).
19. `cargo deny` for transitive license audits (cognee #2807 - FastEmbed removed for license).
20. Pin upstream native deps; ship a lockfile (agentmemory #555/#540).

## 15. Managed workstreams use a portable ledger, not native format conversion

Managed cross-harness continuity is explicitly opt-in through `ai-memory run`.
Direct Claude Code, Codex, OpenCode, Pi, Crush, Kimi Code, and OMP launches
retain the existing hook and single-use handoff behavior. There is no
process-global mode or manual harness switch: the wrapper selects the current
repository/worktree workstream and each adapter applies that harness's native
create/resume syntax.

One logical workstream owns one native session per harness plus an append-only
portable event ledger. We rejected converting a Claude transcript into a fake
Codex rollout (and the inverse): native stores include private, versioned state,
provider-specific records, and integrity assumptions that ai-memory does not
own. Adapters therefore read native stores without modifying them, normalize
only visible messages/completed tools/compaction boundaries, and keep source
and delivery cursors separately. Hidden reasoning and provider-private records
are excluded explicitly.

A renewable single-writer lease resolves precedence and concurrency instead of
attempting bidirectional file synchronization. SessionStart receives a bounded
unseen delta; the full visible ledger stays searchable. Repository checkpoints
are evidence at run boundaries and never commit, stash, reset, or otherwise
mutate the checkout. The markdown wiki remains the durable knowledge surface;
the managed ledger is an operational continuity substrate.

Native-session adoption is restricted to bootstrapping an otherwise-empty
workstream. The interactive launcher may offer recent sessions recorded for the
same canonical checkout, but an explicit new workstream and noninteractive
invocations start fresh. After any harness links a session or contributes
portable history, server-side state disables adoption for every harness. This
prevents a first Codex launch after Claude work from attaching an unrelated old
Codex transcript; it creates a clean Codex session, injects the established
ledger, and resumes that linked Codex session on later returns.

Bare `ai-memory run` treats local timestamps as a bootstrap hint, not global
precedence. Once a workstream is established, the server selects the most
recently linked harness among the locally available candidates. This prevents a
newer obsolete transcript file from overriding the logical workstream. When no
checkout-local candidate exists, bare mode fails before creating server state.

We also rejected automatic private-store rewrites after a checkout directory
rename. Most harnesses persist absolute paths, there is no shared relocation
API, and the server cannot distinguish a moved checkout from another clone of
the same remote. Exact-path discovery and explicit native recovery are safer;
Crush is the exception because its project-local database moves with the tree.

This design follows the harness-format and session-portability experiments in
the companion `ai-babel` research project: semantic continuity is portable,
while exact native context is preserved by resuming that harness's own session.
