# Prior-Art Implementation Findings

> Scope: compare the checked-in prior-art analyses for agentmemory,
> basic-memory, cognee, and MemPalace against the current ai-memory
> implementation. This is an analysis document only; no code changes
> are implied by this file.

## Executive Summary

ai-memory already captured the most important prior-art lessons:
automatic capture, a narrow MCP surface, markdown-in-git as the human
source of truth, SQLite as the derived index/state store, single-writer
durability, opt-in LLM consolidation, structured JSON outputs, typed
handoffs, versioned supersession, per-project UUID isolation, and an
agentmemory-style retention formula.

The remaining high-value improvements were not a rewrite. They were
mostly lifecycle and retrieval layers on top of the solid base: small
editable memory slots, scheduled maintenance, first-class graph/link
retrieval, bounded raw/verbatim fallback recall, diagnostics, and real
retrieval checks. The most important caution remains keeping the current
substrate simple. Do not copy agentmemory's sidecar surface, cognee's
multi-store orchestration, basic-memory's manual write-note workflow, or
MemPalace's verbatim-everything storage model.

## Implementation Status

The follow-up implementation landed the pragmatic subset of this roadmap:

| Area | Status |
|---|---|
| Slots | `_slots/` pages are pinned automatically and surfaced in briefing/explore snapshots. |
| Scheduled maintenance | Server-side schedule runs lint and forget sweep daily by default; embedding backfill is opt-in. |
| Vector contract | Status reports missing embeddings and stored triples; batch writes leave vector completeness to backfill. |
| Links / graph | Markdown and wikilinks are parsed into `links`; unresolved forward links resolve when targets appear; query uses graph-neighbor RRF. |
| Raw fallback | `observations_fts` provides bounded raw fallback when compiled wiki search misses. |
| Diagnostics | `status` includes FTS row counts, missing embeddings, embedding triples, and unresolved/stale link counts. |
| Retrieval evals | Recall harness now covers FTS/vector, graph expansion, and raw fallback. |
| Procedural learning | Consolidation can classify procedural memory; auto-improvement creates or patches bounded `procedures/` pages. |
| SessionEnd consolidation | The deterministic summary/handoff completes before optional provider work enters a durable, retryable queue outside hook latency. |

## Prior Art Summary

| Project | Main strengths | Main weaknesses | ai-memory stance |
|---|---|---|---|
| agentmemory | Automatic hook capture, four memory tiers, versioned supersession, retention scoring, hybrid BM25/vector/graph retrieval, slots, handoff patterns, snapshots, diagnostics. | iii-engine sidecar, JSON-in-KV storage, in-memory indexes, XML parsing, too many tools/endpoints, early default-on LLM costs, hook blocking/config drift bugs. | ai-memory follows the concepts more than any other project, but replaces the substrate with one Rust binary, SQLite, markdown, structured JSON, and a narrow tool surface. |
| basic-memory | Markdown files as source of truth, derived SQL index, human-editable notes, MCP hints/aliases, `memory://` style navigation, unresolved links, multi-project routing. | Manual `write_note` ceremony, append-only memory, fragile file watcher/sync, markdown grammar burden on the LLM, weak lifecycle/ranking. | ai-memory borrows markdown/source-of-truth and human editability, but rejects manual capture and adds lifecycle, decay, supersession, and hook capture. |
| cognee | Task-list pipeline shape, structured graph extraction, provenance stamping, triplet embeddings, multi-collection retrieval, feedback-weighted `improve()`. | Heavy Python dependency stack, LiteLLM/Instructor brittleness, multi-store graph/vector/relational sync bugs, SQLite deadlocks, retrieval filter propagation regressions. | ai-memory should borrow only the pipeline and retrieval ideas, not the multi-store architecture or generic LLM gateway. |
| MemPalace | Strong verbatim/raw recall story, deterministic IDs, resume-safe mining, transparent benchmark culture, temporal KG ideas. | Chroma/HNSW/FTS corruption under concurrent writers, deferred persistence, destructive repair paths, no decay causing bloat, embedding metadata drift. | ai-memory already avoids the durability failures through a single writer, one SQLite file, provider/model/dim metadata, and decay. The useful feature to steal is bounded raw fallback recall. |

## What ai-memory Already Implements Well

### Storage And Durability

ai-memory's store matches the strongest lesson from the issue research:
keep durable storage and mutation ordering boring. `Store::open` uses
SQLite in WAL mode with foreign keys and migrations. `WriterHandle`
serializes all mutations through one writer thread. Page writes go
through `Wiki::write_page` or `Wiki::apply_batch`, which also update the
store rows and FTS triggers.

This directly avoids the major failure classes in cognee and MemPalace:
parallel SQLite deadlocks, uncoordinated writer processes, Chroma/HNSW
corruption, and write-success-before-durable-index bugs.

### Source Of Truth

The markdown wiki is organized as
`wiki/<workspace_id>/<project_id>/<page-path>`, so project renames do
not move files and two projects can use the same page path without
collision. That preserves basic-memory's best property, human-editable
plain files, while avoiding the permalink/workspace retrofit problems
documented in basic-memory's tracker.

### Capture And Handoff

The hook router accepts lifecycle events over HTTP, returns `202` fast
unless saturated (`429`), and does the writer work outside the agent's
critical path. `SessionEnd` creates a deterministic session page, ends the
session, opens a typed handoff, and commits the wiki. `PreCompact`
checkpoints state, using the LLM consolidator if configured and deterministic
synthesis otherwise.

The explicit `memory_handoff_begin` / `memory_handoff_accept` model is a
cleaner version of agentmemory's informal handoff behavior.

### Consolidation Model

ai-memory implements versioned page supersession (`is_latest`,
`supersedes`) and structured JSON-schema consolidation. Multi-page
consolidation can fan out into `sessions/`, `concepts/`, `decisions/`,
`gotchas/`, and `_rules/`. Rule routing is a useful extension beyond the
prior art: durable project rules are surfaced as `_rules/` pages and
lint suggests moving them into `CLAUDE.md` / `AGENTS.md` where agents see
them every turn.

### Retrieval And Decay

The current retrieval path is FTS5 by default, with graph-neighbor RRF
and optional RRF fusion against stored page embeddings. Raw observation
FTS is used as a bounded fallback when compiled wiki pages miss. The
retention sweep implements the agentmemory-style decay/reinforcement
formula for episodic pages and exempts semantic, procedural, pinned, and
slot pages. Query and recent-page reads bump access counters, which feed
the reinforcement term.

### Operability

The CLI is mostly a thin HTTP client to the server. State-changing admin
routes cover backup, bootstrap, reorg, lint, forget sweep, embed,
commit, purge, rename, and write-page. This preserves the single source
of truth: the running server owns state.

## Gaps And Improvement Opportunities

### P0: Add Bounded Editable Memory Slots

agentmemory's most useful missing idea is the slot system: small,
human-editable, durable memory blocks for project context, user
preferences, current focus, and pending items. ai-memory now implements
this as pinned `_slots/` markdown pages.

Recommended shape:

| Slot | Purpose | Injection policy |
|---|---|---|
| `_slots/project_context.md` | Stable project summary and active architecture constraints. | Small budget, eligible for session-start digest. |
| `_slots/user_preferences.md` | User preferences that are not project rules. | Small budget, never auto-expanded. |
| `_slots/current_focus.md` | What the next session should know before searching. | Expires or is overwritten frequently. |
| `_slots/pending_items.md` | Short actionable queue distinct from handoffs. | Prefer bullets, capped length. |

The important constraint is budget. Do not copy agentmemory's broad
always-injected context problem. Slots should be tiny, explicit, and
auditable. When a slot's write regime matters, use frontmatter:
`slot_kind: invariant` for stable context/preferences and
`slot_kind: state` for mutable current focus or pending items. Missing
`slot_kind` defaults to `state`.

### P0: Schedule Maintenance Outside Hooks

This recommendation is implemented. The server schedules maintenance with
clear intervals and summaries while preserving the manual commands:

| Job | Current behavior |
|---|---|
| forget sweep | Runs daily by default so retention does not depend on a manual request. |
| lint | Runs daily by default to surface stale pages, duplicate titles, rule suggestions, broken cross-project links, and optional LLM contradictions. |
| embedding backfill | Runs only when its interval is enabled and an embedder is configured. |
| optional consolidation queue | Enqueues SessionEnd provider work durably when `AI_MEMORY_CONSOLIDATE_ON_SESSION_END` is enabled; one bounded worker retries it outside hook processing. |

The deterministic SessionEnd page and handoff remain independent of provider
availability. Agentmemory's hook-blocking incidents remain the warning label
for keeping scheduled and queued provider work outside hook latency.

### P0: Clarify And Fix The Vector Indexing Contract

There was a concrete mismatch in the implementation notes: `Wiki`'s
comment implied `write_page` / `apply_batch` both computed embeddings
when an embedder was attached, but the code only embedded in
`write_page`. The comment now states the contract explicitly:
`apply_batch` keeps SQL/file fan-out atomic and vector completeness is
owned by admin or scheduled backfill.

Also, `write_page` logs and continues if the embedder fails. That is a
reasonable availability tradeoff, but docs and invariants should be
precise: FTS5/page persistence is transactional; vector completeness is
best-effort unless a backfill/status mechanism says otherwise.

Options evaluated:

| Option | Tradeoff |
|---|---|
| Embed synchronously in `apply_batch` too | Stronger completeness, slower consolidation, external provider failures can delay writes. |
| Add `embedding_status` and scheduled backfill | Better availability and observability, slightly more schema/state. |
| Document vector indexing as optional best-effort | Minimal change, but less trustworthy without visible status. |

The implemented choice is status plus scheduled backfill. It preserves the
durable FTS path, exposes vector gaps, and keeps backfill opt-in.

### P1: Make Graph/Link Retrieval First-Class

The schema has a `links` table with nullable `to_page_id`. Markdown
parsing now extracts wikilinks and markdown links into that table, and
`memory_query` uses graph-neighbor expansion alongside FTS5 and optional
page embeddings.
This leaves two prior-art strengths untapped:

| Source | Idea to adopt |
|---|---|
| basic-memory | unresolved forward links and graph context building. |
| agentmemory | graph as a third retrieval stream in RRF. |
| cognee | triplet-like searchable relationship text, later. |

Recommended sequence:

1. Parse `[[wiki links]]` and ordinary markdown links from page bodies.
2. Store unresolved links with `to_page_id = NULL` and resolve them when
   the target appears.
3. Add a graph-neighbor expansion mode for pages already retrieved by
   FTS/vector.
4. Fold graph hits into RRF as the third signal.

Avoid a separate graph database. SQLite tables and recursive CTEs are
enough for the expected corpus size.

### P1: Add Bounded Raw/Verbatim Fallback Recall

MemPalace's strongest useful idea is not its storage stack. It is the
evidence that raw text recall can recover details that compiled summaries
drop. ai-memory reserves `raw/`; the implemented fallback searches the
durable `observations` table through `observations_fts`.

Recommended shape:

| Layer | Role |
|---|---|
| compiled wiki | Default retrieval, high signal, human-readable. |
| raw observation fallback | Only searched when compiled wiki misses or the user asks for raw/session detail. |
| retention budget | Prevent MemPalace-style bloat. |
| privacy boundary | Use the same sanitizer before raw text becomes durable/searchable. |

This gives ai-memory the best of both philosophies: compile first, but
keep a bounded escape hatch for exact details.

### P1: Add Diagnostics And Safe Heal Paths

The prior-art trackers show that users forgive complexity less than they
forgive uncertainty. ai-memory has a simple architecture, but it should
still expose health checks.

Useful diagnostics:

| Check | Why |
|---|---|
| wiki file exists for latest page row | Detect file/db drift. |
| FTS row count matches latest pages | Detect derived-index corruption. |
| embedding triples are homogeneous | Already checked at startup, also useful on demand. |
| pages missing embeddings | Supports vector-status/backfill contract. |
| unresolved/broken links | Once link parsing exists. |
| orphan sessions/observations/handoffs | Confirms FK/cascade expectations. |
| watcher degraded streak | Make reconciliation failures visible in status. |

Safe heal should rebuild derived indexes from markdown and backfill
embeddings. It should not delete source files without a graveyard step.

### P1: Add Real Retrieval Evaluation

The current recall@5 test is useful as a harness, but it is synthetic.
Do not make LongMemEval-style claims until they are backed by a real
held-out dataset.

Recommended benchmark matrix:

| Variant | Purpose |
|---|---|
| FTS5 only | Baseline, zero-LLM/zero-embedding mode. |
| vector only | Measures semantic signal independent of keywords. |
| FTS5 + vector RRF | Current hybrid path. |
| FTS5 + vector + graph RRF | Future graph path. |
| compiled wiki + raw fallback | Future MemPalace-inspired path. |

Borrow MemPalace's transparency, not its headline-chasing. Public claims
should have tests or should not be claims.

### P2: Consider Feedback/Reinforcement Beyond Access Counts

ai-memory already tracks access count and last access time. cognee's
useful idea is finer-grained feedback: which memory items were used in a
successful answer or handoff. This can wait until retrieval paths are
more mature.

Possible future signals:

| Signal | Use |
|---|---|
| page included in handoff | Boost because it supported continuity. |
| page included in explore digest | Boost if repeatedly surfaced. |
| user marks finding stale/wrong | Lower confidence or route to lint. |
| query hit ignored repeatedly | Decay ranking without deleting. |

### P2: Temporal Triples Later

MemPalace's temporal triples are useful for facts that change over time,
but ai-memory should not add them before basic link extraction and graph
retrieval exist. A future lightweight table could model
`subject/predicate/object/valid_from/valid_to/source_page_id`, but adding
that now would be speculative.

## Ideas Not To Copy

| Temptation | Why not |
|---|---|
| agentmemory's iii-engine sidecar | Install and ops fragility were the largest pain cluster. |
| JSON blobs as primary state with in-memory indexes | Scaling, rebuild, and durability issues. |
| XML or regex-parsed LLM output | Fragile extraction; ai-memory's JSON-schema path is better. |
| 50+ MCP tools and 100+ REST endpoints | Burns context and confuses agents. |
| Broad automatic context injection | Token costs and stale context outweigh convenience. |
| basic-memory's manual `write_note` workflow | Ambient capture is ai-memory's core advantage. |
| LLM-authored markdown grammar as storage API | Forces models to generate syntax instead of meaning. |
| cognee's LiteLLM/Instructor gateway | Provider drift and silent kwarg drops are documented failure modes. |
| cognee's three-store sync | Correctness bugs concentrate at graph/vector/relational seams. |
| MemPalace's Chroma/HNSW surface | Concurrent-writer corruption and native-binding failures. |
| Verbatim-everything with no decay | Leads to bloat and later destructive cleanup pressure. |
| Destructive repair tooling | Any repair/rebuild must be graveyard-first or derived-index-only. |

## Documentation Status Audit

These items previously drifted from the code. Their current status is:

| Area | Current status |
|---|---|
| MCP tool count/list | Fixed: architecture and design docs list the current 16-tool surface. |
| Lint scope | Fixed: lint covers stale episodic pages, duplicate titles, rule suggestions, broken cross-project links, and optional LLM contradictions. |
| sqlite-vec status | Fixed: the vector policy documents packed vectors in SQLite with brute-force cosine as the current backend. |
| raw archive status | Fixed: docs distinguish reserved `raw/` files from implemented observation-FTS fallback. |
| scheduled maintenance | Fixed: docs describe default scheduled lint/sweep, opt-in embedding backfill, and the independent hollow-project cleanup. |
| procedural tier | Fixed: consolidation can classify procedural memory, auto-improvement can create or patch bounded `procedures/` pages, and retention preserves semantic/procedural tiers. |
| SessionEnd consolidation | Fixed: SessionEnd always writes a deterministic page and handoff; opt-in LLM work uses a durable retry queue, while PreCompact/manual consolidation remain available. |
| batch embeddings | Fixed: code comments and status diagnostics now describe batch vector completeness as backfill-owned. |

## Implemented Roadmap Slice

1. **Tighten docs and vector status first.** Missing embeddings are now
   visible in status diagnostics.
2. **Add scheduled jobs.** Periodic lint and forget sweep run by default;
   embedding backfill is available but opt-in.
3. **Add slots.** `_slots/` pages are pinned and surfaced through briefing.
4. **Add link extraction and graph RRF.** The existing SQLite `links`
   table powers graph-neighbor expansion.
5. **Add bounded raw fallback recall.** Observation FTS handles exact-detail
   fallback when compiled pages miss.
6. **Make retrieval claims test-backed.** The recall harness covers the new
   graph and raw fallback paths.
7. **Add procedural learning.** Consolidation and auto-improvement can produce
   bounded durable procedure pages without weakening retention safeguards.
8. **Queue optional SessionEnd consolidation.** Provider work is durable,
   retryable, and independent of the deterministic summary/handoff path.

## Bottom Line

ai-memory followed agentmemory most closely at the idea level and made
the right substrate choices to avoid agentmemory's worst operational
costs. The implemented follow-up kept that discipline: slots, scheduled
decay/lint, graph-aware retrieval, raw fallback, and diagnostics landed
without adding a sidecar, broad MCP surface, or multi-store sync layer.
