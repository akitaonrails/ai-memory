# Vector Backend Policy

ai-memory currently stores embeddings as packed vectors in SQLite and
does brute-force cosine over latest pages at query time. `sqlite-vec` is
deferred intentionally, not rejected.

## Why Not `sqlite-vec` Yet?

The expected corpus is usually hundreds to low-thousands of latest wiki
pages per project. At that size, brute-force cosine is simple,
inspectable, and fast enough, especially because vector retrieval is only
one signal in the query path. `memory_query` already combines FTS5,
graph-neighbor expansion, optional vector RRF, and raw observation
fallback. Each stream contributes through the same bounded candidate window,
then a page-authority adjustment runs after fusion, so semantic similarity
cannot by itself declare a session page more canonical than a maintained
decision.

Adding `sqlite-vec` would add operational surface before it has proven
value:

- Extension loading must work consistently on every SQLite connection.
- Docker/static-link packaging must stay reliable across target hosts.
- Startup diagnostics need to distinguish extension-load failures from
  embedding provider/model/dim drift.
- Derived vector tables need safe rebuild/backfill paths.
- Prior-art projects hit real bugs around native vector dependencies,
  extension loading, and silent embedding skips.

For now, regular SQLite rows keep the vector contract simple: markdown is
source of truth, `page_embeddings` is durable derived data, and status can
report missing embeddings or provider/model/dim heterogeneity without a
second index format.

## When To Add `sqlite-vec`

Add `sqlite-vec` when measurements show brute-force vector scoring is the
bottleneck and vectors materially improve retrieval quality.

Concrete trigger criteria:

- Latest embedded pages per project regularly exceed roughly `5k-10k`.
- `memory_query` p95 exceeds roughly `150-250ms` locally because of
  vector scoring, after FTS/graph paths are already optimized.
- Profiling shows query-time dot products consume meaningful CPU,
  especially under concurrent MCP calls.
- Real retrieval evals show vector results improve recall over FTS5 +
  graph by a meaningful margin, for example `+5-10% recall@5`.
- A migration can create and backfill the vec table from existing
  `page_embeddings` without resetting user data.
- The derived vector index can be dropped and rebuilt from markdown +
  `page_embeddings`; repair must never delete source wiki files.
- Docker packaging and startup checks fail clearly when the extension is
  unavailable.

## Non-Criteria

Do not add `sqlite-vec` just because vector databases are conventional,
or because it feels architecturally cleaner. Without measured latency or
recall pressure, the extra dependency and repair surface are not worth it.

## Intended Shape When It Lands

`sqlite-vec` should remain a derived index behind the existing embedding
contract. It should not become the source of truth and should not change
the MCP tool surface. The migration path should be additive:

1. Keep `page_embeddings` as durable metadata and vector storage.
2. Add a vec virtual table populated from `page_embeddings`.
3. Teach diagnostics to report vec-table row counts and stale rows.
4. Add a safe rebuild command/path for the derived vec table.
5. Switch vector candidate generation from brute-force scan to
   `sqlite-vec`, while preserving FTS5 + graph + vector RRF semantics.
   The existing post-fusion authority adjustment remains unchanged.
