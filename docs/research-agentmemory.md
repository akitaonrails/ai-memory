# agentmemory - Research Report

> **Follow-up:** the September 2026 landscape refresh (how this project
> evolved, what the field looks like now, and what we concluded) is in
> [`research-2026-landscape.md`](research-2026-landscape.md).


> Source project: `rohitg00/agentmemory` (TypeScript, MCP server built on `iii-engine`).
> This repo builds on that earlier TypeScript project, keeping its *ideas* and
> replacing its *substrate*.

## 1. Purpose & Scope

agentmemory is **persistent memory infrastructure for AI coding agents**. An agent silently captures what you do during a coding session (tool calls, prompts, decisions, errors), compresses those raw observations into searchable memory, and re-injects relevant context into the *next* session so the user never has to re-explain architecture, preferences, or past bugs. README pitches a tagline ("Your coding agent remembers everything. No more re-explaining.") with claimed retrieval R@5 of 95.2% on LongMemEval-S vs. 86.2% BM25-only fallback, and ~1,900 tokens/session vs. ~22K for raw CLAUDE.md.

The author explicitly frames it as the *implementation* of Karpathy's "LLM Wiki" pattern, extended with confidence scoring, lifecycle, knowledge graph, and hybrid search; the project page points to a viral gist with 1200+ stars that articulated the design.

Caveat about `DESIGN.md`: that file in agentmemory is a Lamborghini-inspired *visual* design system for the marketing site, not architecture. The real architecture docs are in `AGENTS.md` and `README.md`.

`ROADMAP.md` confirms the trajectory: Q2 2026 "Depth" (multimodal), Q3 "Breadth" (more agents, OpenSSF), Q4 "Trust" (SSO/RBAC), Q1 2027 v1.0 freeze. Candidate item for Q1 2027: *"Reference implementation in a second language (Rust or Go)"*, which is directly relevant to this project.

## 2. Architecture

- **Stack**: TypeScript (ESM, Node ≥ 20), packaged as `@agentmemory/agentmemory`. Build via `tsdown`.
- **Not a standalone server**. Everything runs on top of **iii-engine**, a separately-installed Rust binary that runs on `ws://localhost:49134` and provides Worker/Function/Trigger primitives (`AGENTS.md:5`). The Node process registers functions; the engine routes them. This is the project's central architectural bet.
- **Storage**: a single **file-based SQLite KV store**, owned by iii-engine's StateModule, not by the Node process. From `iii-config.yaml:11-16`:
  ```yaml
  - name: iii-state
    config:
      adapter: { name: kv, config: { store_method: file_based, file_path: ./data/state_store.db } }
  ```
  The Node code only sees a tiny shim (`src/state/kv.ts:6-46`) wrapping five RPCs (`state::get/set/list/update/delete`). There is no direct SQLite, Postgres, Qdrant, or graph DB; *all* memory types are stored as JSON values under namespaced "scopes" (e.g. `mem:memories`, `mem:semantic`, `mem:graph:nodes`).
- **Indices** live in-process: BM25 (`src/state/search-index.ts`), an in-RAM vector index with cosine similarity (`src/state/vector-index.ts`), and persisted snapshots written back into KV via `IndexPersistence` (`src/state/index-persistence.ts`). Hybrid search uses RRF-style fusion of BM25 + vector + graph (`src/state/hybrid-search.ts`).
- **Surfaces**: REST API on `:3111` (124 endpoints, `src/triggers/api.ts`), MCP server on stdio via `npx @agentmemory/mcp` (`src/mcp/server.ts`, ~62KB, with 53 tools in `src/mcp/tools-registry.ts`), live WebSocket stream on `:3112`, and a real-time viewer HTML on `:3113`.
- **KV scope catalogue**: `src/state/schema.ts:3-50` lists ~40 scopes (sessions, observations per-session, memories, summaries, semantic, procedural, graph:nodes/edges, insights, lessons, crystals, sketches, sentinels, actions, leases, routines, signals, checkpoints, mesh, slots, retention, accessLog, audit, imageRefs, etc.). That is a wide catalogue, but it all lives in one SQLite file.

## 3. Memory Model

The system has many memory *types*, organized roughly into a four-tier consolidation hierarchy declared explicitly in `types.ts:429`:
```ts
export type ConsolidationTier = "working" | "episodic" | "semantic" | "procedural";
```

- **Raw observation** (`RawObservation`, `types.ts:29-42`): captured by hooks on every tool call.
- **Compressed observation** (`types.ts:44-62`): structured XML output from an LLM (`mem::compress`) with `type`, `title`, `facts`, `narrative`, `concepts`, `files`, `importance` 1–10. This LLM compression is OFF by default (`src/index.ts:245-253`, issue #138). The default path uses **synthetic** compression (`src/functions/compress-synthetic.ts`), with zero LLM calls, to keep token bills down. Set `AGENTMEMORY_AUTO_COMPRESS=true` to opt in.
- **Memory** (`types.ts:81-101`): consolidated long-term entry with `type` (pattern/preference/architecture/bug/workflow/fact), `strength`, `version`, `supersedes`, `isLatest`. Versioned in place: the old memory gets `isLatest=false`, and the new one keeps `parentId`/`supersedes` chain (`src/functions/consolidate.ts:159-191`).
- **SemanticMemory** (`types.ts:435-446`): individual *facts* with confidence + access counts.
- **ProceduralMemory** (`types.ts:448-462`): named procedures with steps + trigger condition.
- **Lesson / Insight / Crystal**: a higher tier of distillation. Crystals (`crystallize.ts`) summarize chains of completed Actions into narrative + key outcomes + lessons; Lessons feed Reflect which produces Insights from concept clusters.
- **MemorySlot** (`types.ts:222-232`, `functions/slots.ts:13-83`): pinned editable text blocks (persona, user_preferences, project_context, guidance, pending_items, etc.), a Karpathy-wiki-style human-editable section. Always injected into context via `src/functions/context.ts:43-61`.

**Automatic operations (no manual writes needed)**:
- `mem::observe` runs from `PostToolUse` hook on every tool call: privacy-stripped, dedup'd, optionally LLM-compressed, indexed, streamed live (`functions/observe.ts:42-280`).
- `setInterval` timers in `src/index.ts:491-531`: auto-forget every 1h, lesson decay every 24h, insight decay every 24h, consolidation pipeline every 2h.
- `mem::auto-forget` (`functions/auto-forget.ts`): deletes TTL-expired memories, soft-deletes contradiction pairs (Jaccard similarity > 0.9), purges 180-day-old observations with importance ≤ 2.
- `mem::retention-score` (`functions/retention.ts:80-94`): retention = `salience * exp(-λ·Δt) + reinforcementBoost(accessLog, σ)`. Below the "cold" threshold (0.15), entries become evictable. The reinforcement boost (`computeReinforcementBoost`) sums `1/daysSinceAccess`, a classic spaced-repetition scheme.

## 4. Reorganization / Consolidation (the Karpathy bit)

The consolidation pipeline runs on a 2h cron (`src/index.ts:523-531`) when `CONSOLIDATION_ENABLED=true`. `functions/consolidation-pipeline.ts:50-269` orchestrates four tiers:

1. **Semantic tier**: takes the 20 most recent `SessionSummary` items, asks the LLM to extract `<fact confidence="x">…</fact>` items. Existing facts (matched case-insensitively) get `accessCount++` and `confidence = max(old, new)`; new ones become `SemanticMemory` rows (`consolidation-pipeline.ts:91-122`).
2. **Reflect tier**: `mem::reflect` (`functions/reflect.ts`) walks the knowledge graph, builds concept clusters via BFS-by-degree (`buildGraphClusters`, falls back to Jaccard clustering at line 106 if no graph), feeds each cluster's facts + lessons + crystals to the LLM with the `REFLECT_SYSTEM` prompt, expects `<insight>` XML back. Existing insights (fingerprinted on content) get `reinforcements++` and `confidence += 0.1*(1-confidence)` (reflect.ts:26-35); new ones are stored with `decayRate=0.05/week`.
3. **Procedural tier**: finds Memory rows of type `pattern` with `frequency >= 2`, extracts named `<procedure>` blocks with steps.
4. **Decay tier**: applies geometric decay `strength *= 0.9^decayPeriods` after a configurable inactivity window (`applyDecay` at consolidation-pipeline.ts:21-43).

Separately, `mem::consolidate` (`functions/consolidate.ts:65-225`) groups observations by concept, picks the top-N most important per concept, and for each cluster either *creates* a Memory or *evolves* an existing one. Evolution = mark old `isLatest=false`, write a new row pointing at it via `supersedes` and `parentId` (lines 161-189). The old memory isn't deleted; it remains versioned-but-shadowed, with `isLatest` filtering at read time.

`mem::insight-decay-sweep` (`functions/reflect.ts:425-476`) runs weekly: `newConfidence = confidence - decayRate * weeksSince`. Below 0.1 with zero reinforcements, soft-deleted.

This matches the Karpathy wiki pattern: **memories get rewritten in place via versioned supersession instead of only appended, and unused entries fade.**

## 5. Agent Integration

Three surfaces, depending on what the host supports:

- **Hooks (Claude Code, Codex)**: 12 hook scripts in `src/hooks/`, standalone Node scripts that read JSON from stdin and POST to `/agentmemory/observe` over HTTP with a 3s timeout. `plugin/hooks/hooks.json` registers all 12 (SessionStart, UserPromptSubmit, PreToolUse, PostToolUse, PostToolUseFailure, PreCompact, SubagentStart/Stop, Notification, TaskCompleted, Stop, SessionEnd). Codex gets a 6-hook subset (`README.md:419`).
- **MCP tools**: 53 tools in `src/mcp/tools-registry.ts`: `memory_recall`, `memory_smart_search`, `memory_save`, `memory_sessions`, `memory_consolidate`, `memory_action_create`, `memory_lease`, `memory_signal_send`, `memory_crystallize`, etc. Only 8 are visible by default (`AGENTMEMORY_TOOLS=all` exposes the rest, AGENTS.md:114).
- **Skills** (slash commands): `plugin/skills/{recall,remember,handoff,recap,forget,session-history,commit-context,commit-history}/SKILL.md`; each is a markdown file with frontmatter the host parses.
- **System-prompt injection**: pre-tool-use and session-start hooks can write up to 4000 chars of memory context to stdout, which Claude Code prepends to the next turn. **Off by default** (`AGENTMEMORY_INJECT_CONTEXT=false`) because of token-burn complaints (#143, see `src/hooks/pre-tool-use.ts:9-22`).

## 6. Cross-Agent Handoff

Two mechanisms:

- **`/handoff` skill** (`plugin/skills/handoff/SKILL.md`): finds the most recent session whose `cwd` matches your current directory (with proper path-boundary check), surfaces any unanswered question first, then fetches a recall of top concepts. This works *because all agents write to the same `:3111` server*: a Claude Code session and a Codex session in the same project share KV. Cross-agent recall is implicit.
- **Signals** (`functions/signals.ts`): a typed inter-agent message bus with `from`/`to`/`threadId`/`type: handoff|request|response|alert|info` and TTLs. `mem::signal-send` + `mem::signal-read` MCP tools.
- **Mesh sync** (`functions/mesh.ts`) does LWW-merge replication between agentmemory *servers* on different machines, but cross-agent on a single box is just the shared SQLite.

Roadmap explicitly flags "Cross-agent shared memory namespace" as Q3 2026 candidate and "Agent-to-agent memory handoff protocol" for Q4 2026, so the current support is informal.

## 7. Self-Healing / Operations

- **Backup**: `mem::snapshot-create` (`functions/snapshot.ts:44-150`) dumps sessions + memories + graphNodes + observations + accessLogs to a `state.json` and `git commit`s it into a configurable directory. Configurable interval cron. Each snapshot has a SnapshotMeta with commitHash. There's a `mem::snapshot-diff` too. Using git as the backup is a good idea.
- **Migration**: `mem::migrate` (`functions/migrate.ts`) reads legacy `better-sqlite3` DBs and replays them through the KV layer, with a path-allowlist guard (only under `~/.agentmemory/`).
- **Schema versioning**: an explicit `ExportData.version` union with 50+ string literals (`types.ts:296`) and `supportedVersions` in `export-import.ts`. Updating it requires editing 6 files (AGENTS.md:28-34).
- **Vector index corruption guard**: at boot, `src/index.ts:367-410` *refuses to start* if persisted vectors have mixed dimensions, with a structured remediation message. This is careful defensive code.
- **Diagnostics**: `mem::diagnose` (`functions/diagnostics.ts`) runs 8 category checks (orphan leases, blocked actions with all deps done, dead sentinels...) and `mem::heal` auto-fixes the fixable ones.
- **Audit**: every state-changing operation records to `KV.audit` with a typed operation union (`types.ts:493-539`, ~45 op types). `AGENTS.md:39-41` makes adding new ones mandatory.
- **Resilience**: top-level `unhandledRejection` swallow with throttled logging (`src/index.ts:118-128`) to survive iii-engine timeout spikes (#204). Circuit-breaker + fallback-chain providers (`src/providers/`).

## 8. What's Good / What's Missing - Honest Take

**What's clever and worth reusing:**

- **Two-tier compression (synthetic default, LLM opt-in)**: the #138 fix is the right call. Default zero-LLM keeps token bills down while preserving BM25/vector search. Lesson: don't make the LLM call mandatory.
- **Versioned in-place memory evolution** (`isLatest` + `supersedes` chain): the Karpathy wiki-rewrite pattern. Cheaper than maintaining a separate "graveyard", and `parentId` keeps history queryable.
- **Retention-as-formula** rather than rules: `salience * exp(-λΔt) + Σ(σ/daysSinceAccess)`. It is tunable and can be computed in batches. Worth borrowing wholesale.
- **Triple-stream RRF retrieval** (BM25 + vector + graph): graph-walk-as-third-signal is unusual and the benchmark numbers (95.2% R@5) suggest it pays off.
- **Git-as-snapshot-backup**: dump state.json, `git commit`. The result is diffable and restorable without DB-specific tooling.
- **Slots**: pinned, human-editable, always-injected wiki-style sections. The user can hand-edit `project_context` and it survives forever, which gives an escape hatch from purely emergent memory.
- **Hook scripts as standalone Node files** (no SDK import, just HTTP): fast startup, fault-tolerant. The 800ms-1500ms hard timeouts (`src/hooks/session-start.ts:27-28`) are explicit; issue #221 records unbounded hook fan-out OOM-killing iii-engine.
- **Privacy filter at observe boundary** (`stripPrivateData` before any persistence), as defense in depth.

**What feels overengineered:**

- **~40 KV scopes, 53 MCP tools, 124 REST endpoints, 50+ iii functions** is a *lot* of surface for v0.9. Many feel speculative (sentinels, sketches, frontier, leases, routines, checkpoints, facets, crystals, mesh, branch-aware, flow-compress, vision-search). The AGENTS.md "you must update ALL of the following" 7-step checklists are a warning sign: the system is so wide that ordinary changes ripple far.
- **iii-engine dependency**: every install requires a separate Rust binary, pinned to a specific version (0.11.2; 0.11.6 broke them), with no canonical Windows installer (`README.md:549`). For a Rust rewrite this is a *strong argument to be self-contained*: embed SQLite directly, drop the engine.
- **All-JSON storage under one big SQLite file**: every "memories" or "graph nodes" list operation is `state::list` → return *all* JSON, parse in Node, filter in memory. `auto-forget.ts:67` literally caps at 1000 latest memories because the O(N²) Jaccard loop. Won't scale past ~10K memories without proper indexing.
- **XML-in-LLM-output everywhere**: `<memory>...</memory>`, `<temporal_graph>`, `<insight>`. Fragile regex parsing (`parseCompressionXml`, `parseTemporalGraphXml`). A schema-validated JSON / structured-outputs path would be more robust.
- **DESIGN.md is for the website**, not the architecture. Architecture is scattered across AGENTS.md, README.md, and inline comments. No real `ARCHITECTURE.md`.

**What's missing that a Rust competitor should improve:**

1. **First-class embedded store.** Use `rusqlite` (or `sqlx` + SQLite/Postgres) with *proper indices and FTS5* instead of JSON-blob-in-KV. The retention/auto-forget logic deserves real WHERE clauses, not in-memory filter passes. Look at `sqlite-vec` or `lancedb` for vectors so you're not maintaining a Map<String, Vec<f32>>.
2. **Self-contained binary.** No separate engine. A single `agentmemory` binary that *is* the MCP server, REST server, and storage. The iii-engine indirection costs operability for ~nothing the user sees.
3. **Native MCP transport.** Use a proper Rust MCP SDK; you don't need an HTTP shim between hooks and storage; hooks can speak MCP-over-stdio or a Unix socket. Cuts the 3s HTTP timeout and the auth bearer dance.
4. **First-class structured outputs.** Don't parse `<memory><type>...</type></memory>` regex-strings. Use JSON-schema constrained generation; `serde_json` deserialization with strict validation.
5. **Smaller tool surface.** Pick the 8–12 tools that demonstrably matter (recall, save, sessions, smart_search, handoff, consolidate, forget, governance_delete) and ship those well. The 53-tool surface invites confusion; the README admits 8 are visible by default for a reason.
6. **Real graph store for temporal edges.** The temporal-graph design (`tvalid`, `tvalidEnd`, `supersededBy`, edge-history) is *good*, but implementing it on JSON blobs in KV means every "what does Alice prefer as of 2024-06-15" query is a full table scan. Consider `petgraph` in memory backed by a real SQL graph table, or a proper graph DB if you're going big.
7. **Cross-agent handoff as a designed protocol**, shared SQLite. The Q4 2026 roadmap candidate is exactly this; ship it from day one. Define a `Handoff` type with from-agent, to-agent, context, open-questions, files-touched, model-used; expose `mcp::handoff/begin` and `mcp::handoff/accept`.
8. **Reproducible eval harness.** They ran LongMemEval-S and got 95.2% R@5; bake the harness into CI from the start so regressions are caught.
9. **Single source of truth for schema.** The AGENTS.md "update all 7 files" checklists indicate the schema is duplicated across types, tools-registry, REST, MCP, tests, README, plugin. In Rust, one `proto`-or-derive-driven definition can generate all of those.
10. **A clear `ARCHITECTURE.md`.** Don't repeat their mistake of scattering architecture across AGENTS, README, and inline comments.

In short, **agentmemory's *concepts* are unusually thoughtful**: versioned supersession, retention formulas, slot pinning, hybrid retrieval, opt-in LLM compression, session-cwd-based handoff. The *implementation* is constrained by the iii-engine bet and the all-JSON-in-KV storage choice. A Rust rewrite can keep the ideas and drop the substrate.
