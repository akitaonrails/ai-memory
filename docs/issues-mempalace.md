# MemPalace - Issue & Architecture Synthesis

> Source: GitHub `MemPalace/mempalace`.
> Captured 2026-05-24. Repo: 52.7k stars, MIT, Python, created 2026-04-05,
> v3.3.5, 241 open issues, ~1600 PRs in ~7 weeks. The maintainers publish
> unusually candid history notes (see `docs/HISTORY.md`, where they publicly
> retract overclaims). Dominant themes: **ChromaDB/HNSW/FTS5 corruption under
> concurrent writes**, **silent persistence failures**, **repair tooling that
> destroys data**, **embedding-model drift**.

## What MemPalace is (one paragraph)

Local-first AI memory with the *opposite* philosophy to ai-memory: **store
verbatim, never compile**. Conversation history and files are chunked raw into
"drawers" (verbatim text), organised into "wings" (person/project) and "rooms"
(topic), indexed in ChromaDB (SQLite + HNSW vector index, 384-dim
all-MiniLM-L6-v2, zero API calls). Retrieval is hybrid vector + BM25 with a
"closet" keyword-boost layer. Capture is via explicit `mempalace mine`, a
transcript "convo miner", and Claude Code Stop/PreCompact hooks that spawn
detached `mempalace mine` subprocesses. Claims 96.6% R@5 on LongMemEval (raw,
no LLM). Its retrieval is strong; almost all of its pain is
**operational/storage**.

## Top recurring pain points (ranked)

### 1. Concurrent-write corruption of ChromaDB / HNSW / FTS5 - THE dominant theme

This single cluster is the majority of open bugs. Root cause: **multiple
uncoordinated writer paths to one ChromaDB store**. The Stop hook fires an
async `mempalace mine` on every session end, the PreCompact hook fires another,
the MCP server writes on tool calls, and manual `mempalace mine` can run at the
same time. ChromaDB's HNSW index and FTS5 shadow tables are not safe under
concurrent writers, and a writer killed mid-write (SIGTERM during compaction /
permission prompts / window close) leaves a malformed index.

- **#1596**: *"The Claude Code Stop hook fires `mempalace mine` asynchronously
  on every session end. When multiple Claude sessions close simultaneously …
  multiple mine processes start with no mutual-exclusion guard. These
  overlapping processes write to FTS5 shadow tables concurrently, and when one
  is killed mid-write … it leaves `embedding_fulltext_search` in a malformed
  state."* `PRAGMA integrity_check` returns `ok` (rows intact) but
  `quick_check` fails (FTS5 index malformed) → `repair` refuses to run.
- **#1599**: recurring FTS5 corruption under live writes, *after* the
  transient-vector-failure mitigation that was supposed to fix it.
- **#1581**: HNSW corruption from concurrent ChromaDB clients (MCP server +
  hook subprocesses + auto-ingest mine) **within a single Claude Code session**.
- **#1253**: PreCompact hook double-ingest race corrupts HNSW: *"no
  palace-wide write lock."*
- **#1564**: even the single-writer sweep can trigger stale HNSW quarantine.
- **#1514** (community patch, in Chinese): dual-client concurrent read/write:
  user shipped a `flock + threading.Lock` double-lock patch themselves.
- **#1343 / #974 / #965** (referenced): the original "wrap every ChromaDB
  write in `mine_palace_lock`" retrofit; corruption persists in edge cases.
- **#1497** (the tell): a user asks: *"should MemPalace recommend a
  single-writer/gateway pattern for multi-agent setups? … our reading is:
  `mcp-proxy` helps avoid each client spawning its own server, but it does not
  by itself guarantee that all palace mutations are serialized."* They sketch a
  gateway → write-queue → **single writer**, which is the architecture
  ai-memory has by construction.

### 2. Silent persistence failure - "filed N drawers" but nothing persisted

The write path returns success *before* the data is durably in ChromaDB. Below
`hnsw:sync_threshold`, the HNSW segment + `index_metadata.pickle` never flush,
so small mines vanish while the CLI reports success.

- **#1597**: *"`mempalace mine` reports `Drawers filed: 40` … but the chromadb
  does not persist them. `mempalace status` keeps showing the pre-upgrade
  total."* Confirmed not a cache issue (`mempalace_reconnect` agrees with the
  stale count). The v3.3.5 notes had specifically claimed *"integrity,
  recovery, and cross-process correctness."*
- **#1579**: mines smaller than `hnsw:sync_threshold` never persist
  `index_metadata.pickle`.
- **#1526**: silent empty index below batch threshold (one of two HNSW failure
  modes).
- **#1537**: `mempalace mine` reports `Done ✓` with full counts when the mine
  produced an FTS5-corrupt palace.
- **#1398**: MCP search returns `Error finding id` after bulk-add until WAL
  flushes; CLI works (read-after-write inconsistency).
- **#1489**: request to expose `hnsw:sync_threshold` because low-volume MCP
  workloads "never flush metadata."

### 3. Repair/migrate tooling that cannot recover - or actively destroys data

When corruption happens (theme 1) the recovery path is itself unsafe.

- **#1394**: *"`mempalace repair --mode legacy` destroys 99% of SQLite
  embedding rows on 3.3.4."*
- **#1545**: `repair --mode from-sqlite` "structurally broken at scale:
  auto-quarantine destroys rebuild progress mid-run."
- **#1586**: repair cannot recover combined HNSW-dimensionality + FTS5
  corruption (313k drawers).
- **#1589**: repair *rebuilds* an HNSW segment that ChromaDB 1.5.9 then cannot
  load; the corrupt segment SIGSEGVs in `chromadb_rust_bindings`.
- **#1595**: `mempalace-health.py` parses `header.bin` as uint32 →
  trillion-element HNSW corruption silently passes all checks.
- **#1266**: HNSW pickle corruption after long-running mine: "unsupported
  opcode" on next read; repair/migrate cannot recover.
- **#1492 / #1493**: `rebuild_index` deterministically writes
  `dimensionality=None` into ChromaDB's `index_metadata`.

### 4. Data loss by design - miners and swaps destroy verbatim history

- **#1593** (filed by the founder): "stop delete-on-remine - verbatim history
  should never be destroyed by MemPalace miners." Re-mining a source deletes the
  old drawers first, which contradicts the project's "verbatim, never lossy"
  pitch.
- **#1533**: MCP-server processes destroy newly-swapped palace files on first
  read after an atomic swap (no quarantine re-arm).
- **#1341**: no `SessionEnd` hook: short sessions lose memories on clean exit
  (only Stop/PreCompact exist, and they don't always fire).
- **#1535**: `mempalace mine --limit N` truncates files *before* the
  already-mined skip, so a run can finish with 0 new drawers.
- **#1329**: Stop hook → 1.9 **TB** palace bloat + ChromaDB Rust-bindings
  segfault (verbatim-everything with no decay → unbounded growth).

### 5. Embedding-model drift & pluggability gaps

- **#1561**: "persist and verify embedding model metadata in palace
  collections." There is currently **no stored `{provider, model, dim}` to
  refuse on mismatch**, which is the failure ai-memory's invariant #8 prevents.
- **#1559**: support external embedding APIs (LM Studio, Ollama,
  OpenAI-compatible).
- **#1563 / #1261**: embedding model is hardcoded to all-MiniLM-L6-v2; users
  want configuration-driven switching without source edits.
- **#1380**: hash-embedding fallback mode needs a real BM25 lexical path.

### 6. Native-binding / platform instability (the heavy-dependency tax)

ChromaDB drags in `onnxruntime`, `grpcio`, `numpy`, and a Rust bindings layer.

- **#1355**: `chromadb_rust_bindings` segfaults on macOS 26.4 ARM64, with **all
  `>=1.5.4,<2` versions affected**. That puts a hard floor on a core dependency.
- **#1247**: Windows ONNX `bad_alloc` + convo mine crash.
- **#1488**: MCP server fails on Cyrillic/non-ASCII on Windows (cp1252 stdin).
- **#1570**: openai-compat provider probe blocked (403) on Cloudflare-fronted
  endpoints because of the default `Python-urllib` User-Agent.

### 7. MCP correctness & scale

- **#1574**: MCP returns generic `-32000` instead of `-32602 (Invalid params)`
  on missing required params.
- **#1580**: `_expand_with_neighbors` stitches *unrelated* chunks across MCP
  drawers that share an empty `source_file` (silent cross-contamination of
  results).
- **#1379**: overview tools time out on large palaces.
- **#1471**: "HNSW capacity divergence" check runs per-request in the MCP
  server (perf).

### 8. Capture noise (markdown/transcript parsing)

- **#1333**: `<local-command-*>` tags and ANSI escapes survive
  `strip_noise()`, polluting drawers (same class of problem as basic-memory
  #738 and our own Claude-Code-tag stripping).

## Design choices that caused the most issues

| Design choice | Cited in | Symptom |
|---|---|---|
| ChromaDB (SQLite + binary HNSW segments + FTS5) as both store **and** index | #1596, #1599, #1581, #1589, #1355 | Index corruption + native segfaults; integrity_check ok but quick_check fails |
| Multiple uncoordinated writer paths (Stop hook + PreCompact hook + MCP + manual mine), lock retrofitted later | #1596, #1253, #1497, #1514, #1343 | Concurrent-write corruption; community ships own dual-lock |
| Persistence deferred until `hnsw:sync_threshold` batch; write returns before durable | #1597, #1579, #1526, #1489, #1398 | "Filed N drawers" with nothing persisted; read-after-write misses |
| Repair/migrate without graveyard-before-destroy | #1394, #1545, #1586, #1266 | Recovery tooling destroys the data it was meant to save |
| Verbatim-everything with no decay/forget | #1329, #1593 | 1.9 TB palace bloat; delete-on-remine to manage size → history loss |
| Hardcoded local embedding model, no `{model, dim}` stamped on the collection | #1561, #1563, #1261 | No refuse-on-mismatch; silent drift on model change |
| Heavy native ML stack (onnxruntime + chromadb_rust_bindings) | #1355, #1247, #1488 | Platform-specific crashes, hard version floors |
| Hooks spawn detached `mempalace mine` subprocesses | #1596, #1253, #1329 | N concurrent writers per machine; killed-mid-write corruption |

## How ai-memory compares (invariant-by-invariant)

**MemPalace's biggest pain cluster is the set of failures ai-memory's
cross-cutting invariants exist to prevent.** Someone else's 241 open issues
support the architecture choices we made.

| MemPalace pain | ai-memory invariant that prevents it |
|---|---|
| #1497/#1514/#1343: debating/retrofitting a single-writer gateway; concurrent writers corrupt HNSW | **#2 Single-writer SQLite actor.** All writes through one mpsc channel, by construction, since M1. No retrofit. |
| #1597/#1579/#1537: "filed N" but not persisted; write returns before durable | **#3 Indexes commit in the same transaction as the data.** No background-task-indexing-after-return; tool responses block until durable. |
| #1561/#1563: no stored embedding model identity; drift on model change | **#8 `{provider, model, dim}` stored next to every embedding; refuse on mismatch.** Schema column since M1, enforced at startup. |
| #1394/#1545/#1586: repair tooling destroys data it should save | **#17 Wiki migrations: no destructive deletes without a graveyard step** (`_graveyard/<migration>/…`) + **#10 atomic tmp+rename+fsync**. |
| #1593/#1533: miners/swaps destroy verbatim history | **#10 Atomic file writes** + git-versioned wiki (M5): every page change is a commit, recoverable via `git`. |
| #1355/#1247/#1488: native ONNX + Rust-bindings segfaults, platform floors | **Bundled `rusqlite` (static), pure-Rust, zero-LLM default path (#13).** Vectors (sqlite-vec) deferred to v0.2 precisely to avoid this class of dependency risk. |
| #1596/#1253: hooks spawn N concurrent writer subprocesses | **#5 Hooks are fire-and-forget POST to one server** with one writer actor, never a process per event. |
| #1329: verbatim-everything → 1.9 TB bloat | **Karpathy compile-don't-hoard** + **M8 decay/forget-sweep** + log rotation. We compress instead of accumulating raw data forever. |
| #1574: wrong JSON-RPC error codes | rmcp typed `McpError` variants. (Worth a spot-check that we map param errors correctly; see "gotchas".) |

## What ai-memory could learn / steal from MemPalace

These ideas are worth a brainstorm. None is adopted yet.

1. **A verbatim recall tier as a safety net under the compiled wiki.** This is
   the most important idea. MemPalace shows that *raw text + local semantic search*
   scores 96.6% R@5 on LongMemEval with zero LLM. ai-memory compiles
   observations into wiki pages, which is higher-signal but **lossy**:
   consolidation can drop a detail the user later needs. We already keep `raw/`
   immutable session logs. When vectors land (v0.2), we could expose semantic
   search over the raw observation log as a *fallback recall path* when the
   compiled wiki misses ("compiled-first, verbatim-fallback"), combining both
   philosophies.
2. **LongMemEval as a first-class, honest eval (our M10).** MemPalace has the
   harness wired and *publishes held-out R@5 with retractions* in
   `docs/HISTORY.md`. Their `tests/test_readme_claims.py`, which fails the
   build if a public claim isn't backed by code, is a pattern worth copying. Adopt the methodology (held-out split, no headline-chasing) when we
   build M10.
3. **Deterministic IDs for idempotent, resume-safe ingest.** MemPalace's sweeper
   uses `session_id + message_uuid` deterministic drawer IDs + a cursor so a
   crashed mine resumes without dupes. Our hook ingestion could adopt the same
   for replay-safety.
4. **Lightweight temporal knowledge-graph triples** (subject/predicate/object +
   `valid_from`/`valid_to` with explicit invalidation). We have a `links`
   table; MemPalace's KG (`knowledge_graph.py`) and their #1416 "promote drawer
   claims into KG triples at end of day" rhymes with our consolidation. Could be
   a future tier for "facts that change over time" (who owns X now vs. before).
5. **Index-health monitoring + a repair path, done safely.** When sqlite-vec
   lands in v0.2, monitor index health (MemPalace's link-to-data ratio guard is
   a good model) AND make repair graveyard-first. Their #1394/#1595 show what
   goes wrong: a repair that destroys rows, and a health check that parses a field
   wrong and passes trillion-element corruption.
6. **Transparent benchmark/claims culture.** A `HISTORY.md` that records
   corrections builds more trust than a clean marketing page, and we should do
   the same.

## Gotchas to AVOID that bit MemPalace (cautions for our v0.2 vector work)

- **Never report "indexed/saved" before durable commit**, even for vectors.
  MemPalace #1597 shows the full failure. Invariant #3 already covers this; keep
  it strict when sqlite-vec writes land.
- **Stamp `{provider, model, dim}` on the collection and refuse on mismatch.**
  MemPalace #1561 shows the cost of not doing it. We have it; don't regress.
- **Any re-embed / rebuild-index path must graveyard before delete.** MemPalace
  #1394 destroyed 99% of rows in a "repair." Invariant #17 already mandates
  this; apply it to vector rebuilds too.
- **Keep hooks one-writer fire-and-forget.** Do not, under load, spawn a worker
  per event (MemPalace #1596). Our current design avoids this; keep it.
- **Watch raw-log growth.** Verbatim helps recall but #1329 hit 1.9 TB.
  Our compile + decay + log rotation limits growth, but if we add a verbatim
  recall tier (idea #1), give it a retention budget from the start.
- **Spot-check MCP param-error codes.** MemPalace #1574 returns the wrong
  JSON-RPC code on missing params. Confirm rmcp maps our missing/invalid params
  to `-32602`, not a generic internal error. (Verify; do not assume.)

## Non-technical gotcha: supply-chain / impostor domains

`docs/HISTORY.md` and the README's top banner document a real incident: once
MemPalace got popular, **impostor domains (`mempalace.tech`, etc.) appeared
distributing malware** under the project name. Their mitigation was a loud
"ONLY official sources are GitHub + PyPI + mempalaceofficial.com" warning.
Lesson for ai-memory: when/if it gains traction, expect typosquatting of the
name, the install one-liner (`curl … | sh`), and the Docker image. Cheap
pre-emptive moves: claim the obvious domains, document the *only* official
install sources (GitHub repo, the `akitaonrails/ai-memory` Docker Hub image,
the raw wrapper URL on `main`), and pin/sign releases so users can verify. We
don't have this "popular-project" problem yet, but preparing for it costs
nothing.

## Bottom line

MemPalace is a popular, well-tested retrieval engine with a sound verbatim
philosophy and honest maintainers, and it is **struggling with the
storage/concurrency/durability failures ai-memory designed around from the
first milestone**. The most useful thing to take from it is a feature idea: a
*verbatim semantic-recall fallback* beneath the compiled wiki, so we get
MemPalace's lossless recall without inheriting its ChromaDB corruption surface.
Its 241 open issues also show that our invariants #2, #3, #8, #10, #17 address
real failures and are not over-engineering.
