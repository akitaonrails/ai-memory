# agentmemory - Issue & PR Pain-Point Synthesis

> Source: GitHub `rohitg00/agentmemory`, captured 2026-05-21.
> Repo health: 15.7k stars, very active. ~50 merged PRs in last week.
> Architecture: TypeScript MCP server over a native Rust `iii-engine` KV.

## Top recurring pain points (ranked)

### 1. Install / ops - the single largest bucket
- **`iii-engine` is a separate native binary** with its own version, config file, and storage layout. Pinning is fragile: `iii-sdk@0.11.6` broke routing because `package.json` used `^0.11.2` (#555, fixed by PR #567 pinning exact). Migrating to `iii-database`/iii 0.11.7 is blocking the SQLite migration (#309 comment). The whole stack is held back by an upstream you don't control.
- **Distroless engine + Docker named volumes** = silent permission denied. UID 65532 can't write to root-owned `/data`; engine logs the error but the wrapper buffers in RAM and looks fine until restart wipes everything (#301, still OPEN despite an earlier 0.9.7 "fix").
- **Engine writes `data/` to caller's `cwd`**, so launching from different directories produces different state stores. On Windows users believed memories vanished; they were stranded in `E:\文档\New project\data\state_store.db` while the dashboard read `C:\Users\Lenovo\data\` (#303, still OPEN even after PR #314 added `--data-dir`).
- **Hooks break on Windows when username has spaces** because `hooks.json` doesn't quote `${CLAUDE_PLUGIN_ROOT}` (#477).
- **Runaway log feedback loop**: `iii::workers::observability` warns about "subscriber lagged", and that warn is captured by the same subscriber: 137 GB `daemon.log.new`, system at 98% (#519, still OPEN; `RUST_LOG` not honoured).

### 2. Data integrity / silent loss
- **State persistence buffered behind a 5s `IndexPersistence` debounce**. When `state::set` times out at 30s, the uncaught `IIIInvocationError` crashes the Node process, losing every in-memory BM25/vector update since the last debounce flush (#204).
- **BM25 index `mem%3Aindex%3Abm25.bin` stays at ~96 bytes** because every `state::set` times out at 180s on 10k-observation corpora; each restart pays a 5-minute rebuild (#309, OPEN).
- **Sessions never end on Ctrl-C / SSH-drop / laptop sleep**, so the consolidation + graph-extraction pipeline never fires, then the eviction sweep deletes the entire session (#308, "graph would-have-extracted content is permanently lost").
- **`AGENTMEMORY_DROP_STALE_INDEX=true` in `.env` did nothing**: the dimension guard read `process.env` directly while everything else reads `getMergedEnv()`. **Two config-read paths in the same codebase** (#456). Combined with #469 (vector index 2048-dim on disk vs 384-dim from provider), stranded users with no working recovery path.
- **Sessions never created, but observations were**: separate KV scopes; OpenClaw plugin only wrote observations, so `GET /sessions` returned `[]` (#522, OPEN). Masked by `postJson({fallback_on_error:true})` swallowing the 4xx.
- **`session.summary` and `session.firstPrompt` set to the same truncated title** (#276, OPEN, labelled CRITICAL).

### 3. LLM compression / token-cost
- **`AGENTMEMORY_AUTO_COMPRESS=true` was the default in v0.8.7**. User olcor1: *"My allocation is busted within 20 minutes."* The PostToolUse hook called the LLM on every tool call (#138, brown-paper-bag fix in v0.8.8 flipping default to false). Maintainer: *"this is a real bug in the tool's design, not your setup."*
- **`SessionStart` hook was injecting ~1-2 K tokens into every new session**. Maintainer initially blamed Claude Pro caps, then retracted and gated injection behind `AGENTMEMORY_INJECT_CONTEXT=false` default in v0.8.10 (#143). His own retraction: *"I pattern-matched without verifying against the docs."*
- **`mem::compress` silently failed on ~47% of Claude Code tool calls** because `post-tool-use.mjs` read `data.tool_output` but Claude Code sends `tool_response` (#539, fixed PR #561).
- **Graph extraction parser drops self-closing `<entity .../>` tags** (#492). Same family: #338.

### 4. Retrieval quality / API contract
- **MCP `memory_recall` was aliased to `smart_search` and dropped the `format` param**, so full content was unreachable via MCP no matter what the caller asked for (#440 + #507, fixed PR #516). Six weeks of users getting only compact-mode hits.
- **Viewer/status show 0 memories on real corpora** because `/agentmemory/memories?latest=true` and `/agentmemory/export` materialize the entire list, time out on >8k memories (#544, OPEN).
- **Event-loop starvation from pure-JS dot products**: VectorIndex.search hangs the loop on 100k vectors; sqlite-vec swap took search from 200-250 ms to 20-40 ms (#195).

### 5. Agent integration / MCP surface
- **Claude Code requested protocol version 2025-03-26 but the shim pinned 2024-11-05**; Claude Code discarded the tools list as a result (#510, OPEN). Same root in #553 (OpenCode 8/51 tools) and #400.
- **Codex worktrees treated as separate projects**, fragmenting lessons/sessions per ephemeral worktree path (#515, OPEN).
- **Hooks blocked startup**: `await fetch(... 5000 ms timeout)` on session-start; 10 parallel `claude -p` jobs OOM-killed the engine (#221, fixed PR #222 making hooks fire-and-forget).

## Design choices that caused the most issues

1. **Embedding the search indexes in the Node process while persisting through a remote KV with a 30s timeout.** Drives #204, #309, the rebuild-on-boot cost, and 5-second window of data loss on every crash.
2. **LLM compression on every observation, on by default** (#138, #143, #539).
3. **Two config-read paths (`process.env` vs `getMergedEnv()`)** caused #456/#469.
4. **XML as the compression/extraction wire format**, hand-parsed: drops self-closing tags (#492), accepts only specific casings, fails `CompressOutputSchema` on schema drift (#539).
5. **Hooks that `await` REST round-trips during agent startup** (#221).
6. **Relative paths in the bundled `iii-config.yaml`** (#303), distroless engine without a chown init container (#301), no log rotation (#519).
7. **`fallback_on_error: true` everywhere with swallowed errors** (#522, #539).
8. **Unpinned upstream native dep** (`iii-sdk: ^0.11.2`) → #555. No lockfile shipped → #540.

## Maintainer fixes reveal regrets

- **Auto-compress flipped from default-on to default-off** in v0.8.8 the same day #138 was filed: *"the closest thing to an admission that the headline feature was the headline misfeature"*. *"This is exactly the kind of brown-paper-bag issue."*
- **Context injection moved behind `AGENTMEMORY_INJECT_CONTEXT=false`** in v0.8.10 (#143); the maintainer publicly retracted a wrong first diagnosis. Two defaults reversed in two minor versions.
- **`VECTOR_BACKEND=sqlite-vec` introduced behind a flag**, not flipped on, explicitly because *"some Windows / Alpine Docker users will hit install issues we can't preempt"* (#195).
- **Multiple GH-packages mirror experiments reverted within hours** - PRs #545 → #547 → #548. Lots of try/revert.
- **PR #500: rebuildIndex made non-blocking on boot**, PR #504: batch-embed in rebuildIndex (25 h → 3 h on large corpora). Admits the original boot path made the daemon unusable for hours.
- **OpenCode plugin (#236) shipped as separate subsystem** because the Claude Code hook abstraction didn't fit other agents.

## Still-open architectural debts

- **#309 in-memory BM25/graph → SQLite/FTS5**: blocked on iii v0.11.7. Biggest debt in the repo.
- **#519 daemon.log feedback loop**: the offending warn is inside the closed-source `iii` binary.
- **#303 cwd-relative state store on Windows**: still leaks the wrong dir despite #314.
- **#301 distroless docker volume permissions**: still OPEN.
- **#510 / #553 / #400 MCP protocol-version negotiation**: three reports, same root cause.

## Seven "do not repeat" lessons for the Rust rewrite

1. **Keep search indexes and durable storage in the same transaction boundary.** Don't buffer index writes in memory behind a debounce that loses 5s of data on crash (#204, #309). In Rust, use a single `sqlx` transaction per observation; SQLite FTS5 + `sqlite-vec` in one file solves all three search types and removes the entire "rebuild on boot" pathology.

2. **LLM compression must be opt-in, with a visible token-cost banner.** Default to a zero-LLM synthetic compression (extract title/files/narrative from raw tool I/O) (#138, #143).

3. **One config-read path.** Have `Config::load()` resolve env + file + CLI once into a typed struct; every reader takes `&Config` (#456 + #469).

4. **Never use XML as a wire format for LLM extraction.** Use JSON-mode / structured outputs (#492, #539).

5. **Hooks must be fire-and-forget by contract.** No `.await` on an HTTP round-trip during agent startup (#143, #221). Budget the response time hard (`tokio::time::timeout` with sub-second ceilings).

6. **Persist provider metadata next to the index.** A vector index file must record `{provider, model, dim}` and refuse to load on mismatch with a *single* clear error and an in-process re-embed migration path (#469).

7. **Don't depend on an unpinned native sidecar.** Statically link the engine (`tantivy` for BM25, `sqlite-vec` via `rusqlite`, `petgraph` for the concept graph) (#301, #519, #555). Half the install issues in the tracker exist because `iii-engine` is a separate binary the wrapper can't fix.

**Bonus:** Default the data directory to a canonicalized absolute platform path (`dirs::data_local_dir().join("ai-memory")`) and log it loudly on startup. That single change would have prevented #303 entirely.

### Driver issues
#138 (auto-compress default), #143 (context-injection default), #195 (CPU-bound JS), #204 (uncaught SDK timeout), #221 (blocking hooks), #274 (lesson discard), #276 (corrupt session fields), #301 (distroless volume), #303 (cwd state), #308 (sessions never end), #309 (in-memory BM25), #338/#492 (XML parser), #440/#507 (MCP recall aliased), #456/#469 (dim mismatch + two env paths), #477 (Windows quoting), #510/#553/#400 (MCP protocol version), #515 (Codex worktrees), #519 (log feedback loop), #522 (silent error swallow), #539 (tool_response vs tool_output), #540 (no lockfile), #544 (unbounded list endpoints), #555 (iii-sdk semver).
