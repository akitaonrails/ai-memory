# cognee - Research Report

> **Follow-up:** the September 2026 landscape refresh (how this project
> evolved, what the field looks like now, and what we concluded) is in
> [`research-2026-landscape.md`](research-2026-landscape.md).


> Source project: `topoteretes/cognee` (Python, knowledge-graph + vector + relational, MCP server).

## 1. Purpose & Scope

Cognee bills itself as "the brain behind your agents", a memory control plane (`README.md:40,67`). It ingests heterogeneous data (text, PDF, CSV, code, web pages), then continuously builds a hybrid knowledge graph + vector index + relational catalog so that agents can retrieve context by both meaning (embeddings) and structure (graph relationships). The current public SDK is intentionally minimal, with four verbs: `remember`, `recall`, `forget`, `improve` (README L127). Internally, those map to the older `add`/`cognify`/`search`/`prune` primitives. Cognee positions itself between a RAG retriever and a "company brain," with multi-tenant isolation, ontology grounding, and a Claude Code plugin for capturing agent traces.

## 2. The Cognify Pipeline (heart of the system)

The pipeline is a list of `Task` objects executed by `run_pipeline`. Canonical definition at `cognee/api/v1/cognify/cognify.py:316-344`:

```python
default_tasks = [
    Task(classify_documents),                                  # EXTRACT
    Task(extract_chunks_from_documents, ...),                  # EXTRACT
    Task(extract_graph_and_summarize, graph_model=..., ...),   # COGNIFY (LLM)
    Task(add_data_points, embed_triplets=embed_triplets, ...), # LOAD
    Task(extract_dlt_fk_edges),                                # LOAD (relational FK edges)
]
```

Step-by-step:

1. **Classify documents** (`cognee/tasks/documents/classify_documents.py`). Wraps each raw `Data` row in a typed `Document` subclass (`PdfDocument`, `TextDocument`, `DltRowDocument`, etc.) so downstream chunkers know how to read content.

2. **Chunk documents** (`cognee/tasks/documents/extract_chunks_from_documents.py` + `cognee/modules/chunking/TextChunker.py:1-60`). The default `TextChunker` uses `chunk_by_paragraph` to pack paragraphs up to `max_chunk_size` tokens (calculated as `min(embedding_max_completion_tokens, llm_max_completion_tokens // 2)`). Each chunk becomes a `DocumentChunk` DataPoint with `metadata={"index_fields": ["text"]}`; the storage layer later reads those `index_fields` to know what to embed.

3. **Extract graph + summarize** (`cognee/tasks/graph/extract_graph_and_summarize.py:21-37`). Two LLM tasks concurrently on every chunk via `asyncio.gather`:
  - `extract_graph_from_data` (`cognee/tasks/graph/extract_graph_from_data.py:128-222`) calls `extract_content_graph(chunk.text, graph_model, custom_prompt)` for each chunk, an `instructor`/`litellm`-backed structured call that returns a Pydantic `KnowledgeGraph` of `Node`/`Edge`. Edges with missing source/target are filtered (L181-188). Entity nodes are then validated against an ontology via `expand_with_nodes_and_edges(..., ontology_resolver, ...)` (L110-112). Provenance is stamped onto every DataPoint (`_stamp_provenance_deep`, L30-53). Existing edges are deduplicated via `retrieve_existing_edges`.
  - `summarize_text` produces a `TextSummary` for each chunk used by later "SUMMARIES" search.

4. **Persist nodes, edges & embeddings** (`cognee/tasks/storage/add_data_points.py:30-149`). `get_graph_from_model` recursively walks the Pydantic graph into `(nodes, edges)` tuples, then `deduplicate_nodes_and_edges` removes duplicates. The pipeline branches on `EngineCapability.HYBRID_WRITE`:
  - Hybrid backend (e.g. Postgres+pgvector): `graph_engine.add_nodes_with_vectors(nodes)` in one transaction.
  - Otherwise, parallel writes: `graph_engine.add_nodes(nodes)` + `index_data_points(...)` to the vector engine.
  - If `embed_triplets=True`, builds `(source -› relation -› target)` text strings and embeds them as additional `Triplet` DataPoints (L184-265). This makes graph-walk retrieval findable by similarity.

5. **DLT FK edges**: adds deterministic foreign-key edges for ingested SQL/DLT-backed datasets without LLM cost.

There's also a parallel **temporal** pipeline (`cognify.py:347-392`) that swaps the graph extractor for `extract_events_and_timestamps` → `extract_knowledge_graph_from_events` to build a time-aware graph.

## 3. Storage Backends - Triple-Stack, Pluggable

Cognee always uses three stores in parallel, abstracted by a `UnifiedStoreEngine` (`cognee/infrastructure/databases/unified/unified_store_engine.py:11-66`):

| Layer | Default | Other supported |
|---|---|---|
| **Graph** | `ladybug` (Kuzu fork, file-based) | `neo4j`, `postgres` (AGE-style), `kuzu`, `ladybug-remote`, `neptune`, `neptune_analytics` (hybrid) |
| **Vector** | `lancedb` (file-based, subprocess-isolated) | `pgvector`, `chromadb`, `neptune_analytics` |
| **Relational** | `sqlite` (aiosqlite) | `postgres` via SQLAlchemy async |

Selection is environment-driven: `GRAPH_DATABASE_PROVIDER`, `VECTOR_DB_PROVIDER`, `DB_PROVIDER`, with normalized credentials. There is also a `USE_UNIFIED_PROVIDER=pghybrid` short-circuit that makes a single Postgres instance back graph+vector+relational simultaneously.

Two `EngineCapability` flags (`HYBRID_WRITE`, `HYBRID_SEARCH`) let the pipeline branch on whether nodes+vectors can land in one transaction (saves a round trip) or need two writes.

The default local-only stack is therefore SQLite + LanceDB + Ladybug/Kuzu, all file-based, with no servers required. For homelab parity, that's the lean setup.

## 4. Search / Retrieval

Recall is multi-strategy and auto-routed. `SearchType` enum (`cognee/modules/search/types/SearchType.py`) lists 16 strategies: `GRAPH_COMPLETION` (default), `GRAPH_COMPLETION_COT`, `GRAPH_COMPLETION_CONTEXT_EXTENSION`, `GRAPH_COMPLETION_DECOMPOSITION`, `GRAPH_SUMMARY_COMPLETION`, `RAG_COMPLETION`, `TRIPLET_COMPLETION`, `CHUNKS`, `CHUNKS_LEXICAL`, `SUMMARIES`, `CYPHER`, `NATURAL_LANGUAGE`, `TEMPORAL`, `FEELING_LUCKY`, `CODING_RULES`, `AGENTIC_COMPLETION`.

The `recall()` API (`cognee/api/v1/recall/recall.py:314-513`) picks one via `route_query(query_text)`, a rule-based classifier in `query_router.py` (regex patterns for "when/before/after" → `TEMPORAL`, keyword fragments → `CHUNKS_LEXICAL`, multi-hop wording → `GRAPH_COMPLETION_COT`, etc.). Default fallback is `GRAPH_COMPLETION`.

The main retriever is `GraphCompletionRetriever` (`cognee/modules/retrieval/graph_completion_retriever.py`). It uses `brute_force_triplet_search` (`cognee/modules/retrieval/utils/brute_force_triplet_search.py:216-355`), which:

1. Embeds the query, runs vector search across multiple collections in parallel: `Entity_name`, `TextSummary_text`, `EntityType_name`, `DocumentChunk_text`, `EdgeType_relationship_name` (L281-290).
2. Projects results into a `CogneeGraph` memory fragment, scoring triplets by combined node+edge similarity with `triplet_distance_penalty` and `feedback_influence` (L318-333).
3. Optionally expands a `neighborhood_depth` hop-out from top seed nodes.
4. Resolves the top-K edges to natural-language sentences (`resolve_edges_to_text`).
5. Feeds them into an LLM completion with `graph_context_for_question.txt` as the user prompt.

Recall also supports a session-cache first pass (keyword match against recent QA entries in a relational session table) and falls through to the graph when the session has no hit (`recall.py:382-397, 447-457`). That's the "hybrid working-memory + long-term memory" pattern.

## 5. Memory Lifecycle - Cognee Does Reorganize

Cognee has explicit memory enrichment beyond one-shot ingest. The `improve()` API (`cognee/api/v1/improve/improve.py:36-232`) runs up to five stages:

1. **Feedback weights**: session entries with thumbs-up/down ratings adjust `feedback_weight` on the specific graph nodes/edges that were used to answer (`apply_feedback_weights_pipeline`, L284-299). Tracked via `used_graph_element_ids` recorded at retrieval time. Higher-rated answers boost their source nodes; lower-rated ones decrease them.
2. **Persist session Q&A**: cognifies session transcripts into permanent graph tagged `node_set="user_sessions_from_cache"`.
3. **Triplet enrichment / memify**: `cognee/memify_pipelines/create_triplet_embeddings.py` builds and embeds new triplet datapoints.
4. **Global context index**: `global_context_index_pipeline` builds bucket+root summaries over all text summaries.
5. **Sync graph→session**: incrementally copies recently-added graph edges into session caches as JSON-lines so live agents pick up new knowledge without a re-query.

There's also a `consolidate_entity_descriptions.py` pipeline (`cognee/memify_pipelines/consolidate_entity_descriptions.py`) that walks Entity nodes, fetches neighbors+edges, and rewrites their `description` field via an LLM (`NodeDescription` Pydantic). And `apply_frequency_weights.py` ages knowledge by usage frequency. Deduplication happens at write time in `add_data_points.py` and at extraction time in `retrieve_existing_edges`.

So ingest is not one-shot. There is a feedback loop, summary consolidation, and an edge-aging concept. There is no automatic decay/TTL, but `feedback_weight` and `frequency_weight` give you the levers.

## 6. MCP Integration

Yes, in `cognee-mcp/` as a sibling project with its own pyproject. The server (`cognee-mcp/src/server.py`) uses the official `mcp` SDK over stdio/SSE/HTTP. The publicly exposed tools are deliberately minimal (L1076-1222):

- `remember(data, dataset_name?, session_id?, custom_prompt?)`: with `session_id`, a fast session-cache write; without, runs the full `add + cognify` pipeline.
- `recall(query, search_type?, datasets?, session_id?, top_k=10)`: auto-routes the search type.
- `forget(dataset?, everything=False)`: deletes across all three stores.

There are *internal* tools registered but not exposed in API mode: `cognify`, `search`, `list_data`, `delete`, `prune`, `improve`, plus a UI bundle (`visualize_graph_ui`, `upload_file_ui`, `open_cognee_workspace`, `cognify_file`) that opens an embedded HTML workspace from `src/app_bundles/visualize-graph.html`.

One design choice worth noting: per-MCP-client auto-named datasets (`cursor_vscode_memory`, `claude_code_memory`) so different agents don't pollute each other's memory unless they opt in by passing `dataset_name="main_dataset"`. Toggle with `COGNEE_MCP_AGENT_SCOPED=false`.

MCP integration is manual in the sense that the agent invokes `remember`/`recall` explicitly. The Claude Code plugin (separate repo `cognee-integrations`) automates it via Claude Code lifecycle hooks (`SessionStart`, `PostToolUse`, `UserPromptSubmit`, `PreCompact`, `SessionEnd`); see README L204.

## 7. Operational Concerns

**Deployment**: A top-level `Dockerfile` builds the FastAPI server; `cognee-mcp/Dockerfile` builds the MCP server. `docker-compose.yml` ships the API on port 8000 with resource limits of 4 CPUs and 8 GB RAM, which is a lot. Deployment targets include Cognee Cloud, Modal, Railway, Fly.io, Render, Daytona (`distributed/deploy/`).

**LLM cost per ingest**: heavy. For *every* chunk, cognify runs:
- 1 structured-extraction call (entity/relationship → Pydantic KnowledgeGraph)
- 1 summarization call
- N embedding calls (one per Entity/EntityType/DocumentChunk/TextSummary/EdgeType row, plus optional Triplet rows)

A 100-chunk document easily generates 200+ LLM calls plus hundreds of embeddings. `chunks_per_batch` defaults to 100; `LLM_RATE_LIMIT_*` env vars exist but are off by default.

**What needs API keys**: `LLM_API_KEY` is mandatory unless you point at a local Ollama/HuggingFace model via the `ollama`/`huggingface` extras. Default provider is OpenAI (`openai>=1.80.1` is a core dep). Defaults to its own embedding model via `litellm`.

**Local-only stack**: SQLite + LanceDB + Ladybug means no external services for the storage layer, but the LLM is the cost driver. Replacing OpenAI with Ollama makes it CPU/GPU-heavy but free.

## 8. Strongest Ideas to Adopt

1. **Task-list pipeline composition.** `cognify.py:316-344` reads like a data-flow recipe. Each `Task` is a thin wrapper with a `batch_size` config and a context object. This is the cleanest pattern to port to Rust: represent the pipeline as a typed `Vec<Box<dyn Task>>` and let tasks declare their batching behavior.

2. **Triplet embeddings as first-class citizens.** Embedding `(source -› relation -› target)` text directly (`add_data_points.py:184-265`) lets a graph become searchable by semantic similarity, by walking. It bridges "vector RAG" and "graph RAG."

3. **Capability-flag-driven storage facade.** `UnifiedStoreEngine.has_capability(HYBRID_WRITE)` lets the same pipeline target separate `(Kuzu, LanceDB)` or fused `(Postgres+pgvector)` without conditionals scattered everywhere.

4. **`improve()` lifecycle with feedback weights.** The `feedback_weight` on graph elements + `used_graph_element_ids` from the retrieval trace gives you graded knowledge without retraining anything.

5. **Multi-collection vector search at recall time.** Querying `Entity_name`, `TextSummary_text`, `DocumentChunk_text`, and edge collections in parallel and merging by triplet score, rather than picking one index, is the core of retrieval.

6. **Provenance stamping** (`_stamp_provenance_deep`): every DataPoint carries `source_pipeline` + `source_task`, making the graph debuggable.

## 9. Realistic Downsides for a Lean Homelab

- **Dependency weight**: 40+ core dependencies (`openai`, `litellm`, `instructor`, `sqlalchemy`, `aiosqlite`, `lancedb`, `pylance`, `ladybug`, `networkx`, `pypdf`, `fastapi`, `fastapi-users`, `rdflib`, `langdetect`, `datamodel-code-generator`, `tiktoken`, `tenacity`, `aiolimiter`, `diskcache`, `fakeredis`, plus optional extras (neo4j, chromadb, postgres, anthropic, ollama, huggingface, scraping, dlt, monitoring)). Cold install is huge: `poetry.lock` is 1.4 MB.
- **Python async everywhere + LRU-cached engine handles** (`closing_lru_cache`): debugging stale-adapter issues required adding `_GraphEngineHandle` (see the 50-line docstring at `get_graph_engine.py:56-105`). That's complexity you inherit.
- **Per-cognify LLM cost** is high. There's no obvious caching of "I've already extracted entities from this chunk hash", so re-ingestion repeats work. `incremental_loading=True` helps but only at the dataset level.
- **Three databases to back up**, three to migrate. The "wipe `.cognee_system/` when you flip `ENABLE_BACKEND_ACCESS_CONTROL`" caveat (cognee-mcp/README L518-525) shows the multi-store coordination is fragile.
- **8 GB RAM limit** in docker-compose is the recommended floor. LanceDB + Kuzu + embedding model loaded simultaneously is heavy on a low-end NAS.
- **Bus factor on `ladybug`**: a 0.16.0 pinned Kuzu fork by the same org. If they stop maintaining it, you're on a single-vendor graph DB.
- **Two-tier MCP API**: public 3 tools + 10+ "internal" tools means the API surface is in motion; not stable.

For a Rust homelab MCP server, the lean equivalents would be SQLite (relational) + a single embedded vector store (e.g. `lancedb-rs` or embedded `sqlite-vec`) + an embedded graph DB (CozoDB, SurrealDB, or just SQL tables with proper indices). Then port the task pipeline + triplet embedding + capability flag pattern and skip the FastAPI/users/migrations/UI bundle stack entirely.

### Key file references

- Pipeline definition: `cognee/api/v1/cognify/cognify.py:316-344`
- Graph extraction (LLM call site): `cognee/tasks/graph/extract_graph_from_data.py:128-222`
- Triple-store write path: `cognee/tasks/storage/add_data_points.py:62-149`
- Triplet embeddings: `cognee/tasks/storage/add_data_points.py:184-265`
- Vector backend factory: `cognee/infrastructure/databases/vector/create_vector_engine.py:150-318`
- Graph backend factory: `cognee/infrastructure/databases/graph/get_graph_engine.py:241-457`
- Unified facade: `cognee/infrastructure/databases/unified/unified_store_engine.py:11-66`
- Retrieval (graph+vector hybrid): `cognee/modules/retrieval/utils/brute_force_triplet_search.py:216-355`
- Graph completion retriever: `cognee/modules/retrieval/graph_completion_retriever.py`
- Recall + auto-router: `cognee/api/v1/recall/recall.py:314-513`, `cognee/api/v1/recall/query_router.py`
- Improve / lifecycle: `cognee/api/v1/improve/improve.py:36-411`
- Memify pipelines: `cognee/memify_pipelines/`
- MCP server: `cognee-mcp/src/server.py:1076-1222`
- Deps and extras: `pyproject.toml:22-160`
