# cognee - Issue & PR Pain-Point Synthesis

> Source: GitHub `topoteretes/cognee`. Captured 2026-05-21.
> Tracker character: ~40% feature requests (high comment counts), ~40% bugs
> closed fast by next release, ~20% truly hard still-open bugs at architectural seams.

## Top recurring pain points (ranked)

### A. LLM-adapter brittleness (highest volume, highest churn)

Every provider has had a wire-level bug in 2026.
- Anthropic adapter dropped `max_tokens`, every call HTTP-422 (#2749, #2782; **two consecutive releases shipped broken Anthropic**).
- Ollama / LlamaCpp adapters missing `@observe` decorator (#2820).
- vLLM hangs because system message sent after user message (#2537).
- vLLM "custom" provider doesn't forward `LLM_ENDPOINT` to LiteLLM (#2412, #2430, #2842).
- LiteLLM `model_cost` lookup overrides user's `LLM_MAX_COMPLETION_TOKENS` (#2608, fixed by #2613, #2582).
- HTTP/2 stream stalls cause 60s timeouts on sequential Anthropic calls (#2607).
- Preflight LLM connection test hangs forever for non-OpenAI providers (#2752, #2123, #2380).
- Local OpenAI-compatible LM Studio / Ollama hangs on macOS (#2119, #1743, #1742).
- **OPEN, unsolved**: "Severe Performance Degradation Due to Thinking Tokens + Instructor Incompatibility" (#2840). User patch shows the root cause: when `response_model=str`, instructor wraps `str` in a JSON/tool schema that llama.cpp doesn't honor; the LLM returns plain text, instructor fails to parse it, then **tenacity retries sleep 8-128s per attempt**. LiteLLM silently drops non-standard top-level kwargs like `chat_template_kwargs` and `reasoning_effort`, requiring an `extra_body` shim.

**Root design choice causing this**: LiteLLM + Instructor as the universal LLM gateway. Both churn fast; both silently drop kwargs they don't recognize; both have OpenAI-specific assumptions baked into the structured-output path.

### B. Multi-store coordination & data integrity bugs

The "graph + vector + relational" tripartite store is the cognee architectural commitment, and it is the source of consistent, high-severity bugs.
- `EntityAlreadyExistsError`: `Entity("institution")` and `EntityType("institution")` collide on UUID (#2510). Second `cognify` blew up.
- `add_data_points` parallel DB ops cause SQLite `database is locked` deadlock; **still reproducible in 1.0.2**: `multiple PipelineRunErrored ... elapsed 561s before crash` (#2717, OPEN).
- `cognify` exceeds asyncpg bind-argument limit during `upsert_edges` for large batches (~4356 edges) (#2829, fixed via batching in #2798, #2586).
- `add_data_points` crashes with `asyncpg.CharacterNotInRepertoireError` on null bytes (0x00) (#2612, OPEN).
- `index_data_points`: shallow copy of metadata dict, so only the first `index_field` is embedded (#2529, OPEN).
- `get_graph_from_model + copy_model` drops user-defined DataPoint id (#2633, OPEN).
- Edge deduplication in `retrieve_existing_edges` is non-functional (#2557).
- `delete_dataset` fails in non-"public" Postgres schema (#2291).
- **Shared data: deleting from one dataset wipes the same data from other datasets that share it** (#2732, OPEN).
- N+1 query pattern in `/api/v1/cognify` (#2532, OPEN).
- KuzuAdapter writes visible in-memory but not persisted to disk on 0.5.1 (#1981).
- Per-collection distance normalization in `brute_force_triplet_search` produces incorrect ranking (#2030, fix #2451 removed normalization, **then #2720 surfaced downstream**).

### C. Recall quality regressions - the bug a memory server cannot afford

- **#2720 (OPEN)**: "Graph-completion retrieval returns identical subgraph regardless of query". User-built reproducer shows direct LanceDB queries return different top-K for different queries, but cognee's `/api/v1/search` returns ~identical answers because LanceDB IDs are not propagating into the graph projection. User attributes to fallout from #2451: downstream thresholds in `brute_force_triplet_search.py` still expect pre-#2451 [0,1] scale and silently fall back to an unfiltered graph.
- `SearchType.CHUNKS` silently ignores `node_name` filtering (#2815).
- `CHUNKS/SUMMARIES/GRAPH_COMPLETION` ignores `datasets=` filter when `ENABLE_BACKEND_ACCESS_CONTROL=false` (#2867; the maintainer's answer is essentially "datasets only work with access control on").
- Cognee-mcp GRAPH_COMPLETION discarded all but first dataset (#2617).
- `TemporalRetriever` is event-only, blocks ontology-wide temporal filtering (#2429, closed as superseded by an internal Q2 redesign that has not shipped).
- GRAPH_COMPLETION does not search custom DataPoint vector collections (#2495).

### D. Dependency-installation hell

- macOS arm64 + Python 3.14: `kuzu` wheel does not exist; quick-start fails (#2753).
- `ModuleNotFoundError: No module named 'kuzu' in Docker since v1.0.4` (#2775): kuzu removed but image not rebuilt.
- `fastembed` removed from the Docker image entirely because one of its transitive deps was not Apache/MIT (#2807).
- LiteLLMEmbeddingEngine truncated `BAAI/bge-m3` to `bge-m3` (#1915).
- `embedding_dimensions` defaulted to 3072 regardless of model, so every non-3072-dim embedder broke (#2751, fix #2757).
- LanceDB lance-file writer schema drift / "contained null values" RuntimeError bypassed auto-migration (#2702, #2768).
- Pydantic v1/v2 friction: upper bound conflicts with openai-agents (#2019); `.json()` deprecated (#2042); generic validation issues (#1198).
- Mistral client import error (#2481).
- `lru_cache` hash invalidation bugs for Vector and Graph configs (#2357), and **`lru_cache` was eventually disabled outright** in PR #2853 ("refactor: reduce lru cache").

### E. MCP server bugs vs FastAPI

The MCP wrapper consistently lags the core API.
- MCP `cognify` with valid local file path returns success but creates no Data item (#2250, OPEN).
- cognee-mcp Quick Start fails on macOS arm64 + Py 3.14 (#2753).
- cognee-mcp `cognify(data=str)` silently dropped all writes after first due to hardcoded `data.txt` filename (#2747).
- MCP recall in default config fails: `'NoneType' object has no attribute 'id'` because the wrapper does not pass user to cognee.recall (#2855).
- cognee-cli `--api-url` doesn't support remember/recall/improve/forget (#2809, OPEN).
- Frontend Docker build broken (#2832), Turbopack import case mismatch (#2605), `cognee-cli -ui` v0.5.5 missing 3 npm deps (#2413), UI compile errors (#2709).

### F. Auth / multi-tenancy regressions

- Token refresh mechanism literally not implemented; maintainer admits *"we had to reimplement it for our cloud deployment. At this point, we can't allocate resources"* (#2065).
- Request-scoped LLM config impossible because `get_llm_config()` and `get_embedding_config()` use `@lru_cache` singletons (#2228).
- Auth disable needs *two* flags: `ENABLE_BACKEND_ACCESS_CONTROL=false` AND `REQUIRE_AUTHENTICATION=False` (#2808, fix #2836).
- `cognee.search` ignored ACL when resolving dataset by name for non-owners (#2845); `cognee.add` silently created a new owner-scoped dataset when a non-owner reused a name (#2846); `cognify` silently skipped data added by non-owners (#2847).
- Agent display name leaked user ID (#2811).
- `GRAPH_DATASET_TO_DATABASE_HANDLER` (user typo of `GRAPH_DATASET_DATABASE_HANDLER`) was silently ignored and defaulted to kuzu (#2697). No validation on env-var names.

## Design choices that caused the most issues

1. **Singleton config via `@lru_cache`.** Breaks multi-tenancy (#2228), invalidation bugs (#2357), eventually reverted (#2853).
2. **LiteLLM + Instructor as the universal LLM/structured-output layer.** Source of #2412, #2430, #2537, #2608, #2613, #2749, #2782, #2820, #2840, #2842.
3. **SQLite as the default relational backend with greenlet parallelism.** #2717: `OperationalError: database is locked` under parallel cognify, OPEN. Maintainer hedges: *"sqlite is not there for production use-cases"*.
4. **Kuzu as the default embedded graph DB.** Kuzu was archived upstream (#2098), maintainers chose to replace with **Ladybug** (PR #2755), a *fork* of Kuzu. Immediate post-replacement bugs: #2768, #2775, WAL corruption (PR #2838). **The forked-DB risk has already produced bugs.**
5. **LanceDB as the default vector store** with schema migration assumed to auto-handle drift. Reality: #2702 null-values bypasses migration, #2720 retrieval pipeline drops filter on the path from vector hits to graph subgraph.
6. **Tripartite store with implicit sync (graph + vector + relational + optional ontology).** Source of the entire integrity-bug class in section B. Orchestration is handled inside cognee, not by any transactional layer.
7. **`@lru_cache` + ContextVar mixed model** for tenant isolation. #2228 explains: db config uses ContextVar pattern, but LLM and embedding configs do not.
8. **Per-collection distance normalization in `brute_force_triplet_search`** (#2030). Fix #2451 removed normalization, breaking downstream threshold assumptions, surfacing as #2720.
9. **Backend access control is the orchestration plane.** When `ENABLE_BACKEND_ACCESS_CONTROL=false`, *all* dataset-scoped retrieval silently degrades. (#2867, #2845, #2846, #2847, #2808.)
10. **Default-LLM cost coupling**: `embedding_dimensions` defaults to 3072 (text-embedding-3-large), and litellm's `model_cost` table silently overrode user `max_completion_tokens`. Two bugs from "assume OpenAI defaults" (#2751, #2608).

## What the maintainers' fixes reveal

- **LRU cache for configs has been quietly retreated from** (#2853, #2851).
- **Subprocess mode + Redis** was added explicitly to escape the SQLite-greenlet trap (#2803, #2812).
- **Ladybug is a Kuzu fork** they own. Already shipped "fix: resolve issue with WAL file corruption for ladybug" (#2838).
- **Auto-migrate LanceDB on schema drift** had to be added (#2703) after lance-file writer crashed workers.
- **Anthropic adapter broken for a full version cycle**: #2749, then re-broken in 1.0.5 as #2782.
- **Defaults flipped**: `fastembed` removed from core (#2807), result-cache logging disabled by default (#2851), embedding dimensions auto-derived not defaulted (#2757), auth gated by single switch (#2836).
- **Feature deprecated, not fixed**: `TemporalRetriever` (#2429).

## Open issues maintainers haven't solved

- **#2717 SQLite deadlock under parallel cognify**: reproducible across versions.
- **#2720 LanceDB filter not propagating to graph projection**: a *correctness* bug in core retrieval path. No assignee.
- **#2840 Thinking-token + Instructor incompatibility.**
- **#2532 N+1 query in `/api/v1/cognify`.**
- **#2612 null-byte crash in asyncpg.**
- **#2529 shallow-copy bug in `index_data_points`.**
- **#2228 request-scoped LLM/embedding config.** Architectural.
- **#2065 token refresh.** Punted to community.

**What's hard about these**: they all sit at architectural seams (config plane, retrieval pipeline, async orchestration), so none is a one-PR fix.

## Specific dependency culprits (Rust must bet differently)

| Lib | Bug | Issue |
|---|---|---|
| litellm | drops `extra_body` kwargs silently; `model_cost` overrides user setting | #2608, #2613, #2840 |
| instructor | wraps `response_model=str` in JSON schema local LLMs don't honor | #2840 |
| tenacity | 8-128s backoff multiplies instructor's parse failures | #2840 |
| asyncpg | bind-arg limit; `CharacterNotInRepertoireError` on `\0` | #2829, #2612 |
| lancedb / lance-file | null-values RuntimeError bypasses migration | #2702 |
| pyarrow (under lance) | upstream of #2720 / #2702 schema drift | #2702 |
| kuzu | upstream archived, wheel gap on Py 3.14 / arm64 | #2098, #2753 |
| ladybug (fork of kuzu) | version-mapping crashes on every fresh DB; WAL corruption | #2768, PR#2838 |
| sqlite/sqlalchemy/greenlet | database-is-locked under parallel cognify | #2717 |
| anthropic SDK | `max_tokens` required, two consecutive releases broken | #2749, #2782 |
| fastembed | transitive dep not Apache/MIT; removed from core | #2807 |
| pydantic | v1/v2 friction, deprecated `.json()`, upper bound conflicts | #1198, #2019, #2042 |
| mistralai | client import error | #2481 |
| openai-agents | pin conflict with pydantic | #2019 |
| HF tokenizers | every word triggered HF request in chunking | #729 |
| Turbopack / npm | frontend builds repeatedly broken | #2605, #2413, #2709, #2832 |

## Do-not-repeat lessons for the Rust rewrite

1. **Do not put the LLM call behind a generic Python-style gateway that silently drops kwargs.** Each provider gets a typed Rust client that errors on unknown fields rather than dropping them. (#2840, #2608, #2782.)

2. **Don't use SQLite for write-parallel pipeline state.** Use Postgres or, for embedded, a single-writer actor in front of an LMDB/Sled/SQLite-with-WAL serialized via a queue. (#2717.)

3. **Don't pin a forked embedded graph DB as the default.** Either commit to a battle-tested external store (Postgres + AGE, Neo4j) or build the graph primitives directly on the relational store. The Kuzu→Ladybug pivot cost real users (#2098, #2768, #2775, #2753, PR#2838).

4. **Treat retrieval filter propagation as a first-class invariant with property tests.** Show test cases like `assert different_queries_yield_different_subgraphs`. A memory product cannot survive the exact bug in #2720.

5. **Configuration must be per-request from day one.** No global singletons, no `lru_cache` over config. Use a request-scoped context type passed explicitly. (#2228, #2357, PR#2853.)

6. **Never default `embedding_dimensions` to a constant.** Derive from `(provider, model)` at startup; refuse to start if collection-dim and model-dim mismatch. (#2751, #2757.)

7. **Idempotent ingestion with explicit id derivation.** Node IDs must be a function of `(category, name, dataset)` `name`. Property tests on cross-run determinism. (#2510, #2557, #2633.)

8. **Re-ingestion / pipeline-run status must be a state machine, not a flag.** "PipelineRunAlreadyCompleted" prevented re-ingest of deleted files (#2097).

9. **Dataset isolation must work whether or not "access control" is on.** Dataset is a hard query-time filter on every retriever. (#2867, #2845, #2846, #2847.)

10. **Batch every cross-store mutation.** asyncpg bind-arg limit, SQLite locks, lance writer flushes: all rooted in unbounded fan-out (#2829, #2717, #2702).

11. **Single transactional boundary or a documented eventual-consistency contract.** Cognee's silent skips between graph/vector/relational are the deepest class of bug, so pick one of the two.

12. **Audit logs and result caches with retention from day one.** Cognee's relational DB grew unbounded: 42k cached results in 9 days (#2548). They eventually disabled by default; do it before the first user hits it.

**Calibration**: the single most repeated lesson, weighted by both severity and recurrence: **the LLM/structured-output layer (LiteLLM + Instructor) is fragile, and the multi-store sync (graph + vector + relational) is the deepest source of correctness bugs.** A Rust rewrite that gets either of those wrong inherits cognee's tracker.
