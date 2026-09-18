# Benchmarks

Published retrieval-quality numbers for ai-memory, with full provenance
(commit, dataset sha256, hardware, mode). Every number here was produced
by the in-repo harness — see `evals/README.md` for how to reproduce:

```bash
cargo build --release -p ai-memory-cli
cargo run --release -p ai-memory-eval -- retrieval --fetch
```

## Baselines

| date | dataset | mode | overall hit@5 | file |
|---|---|---|---|---|
| 2026-09-01 | LongMemEval-S (v1) | **local embeddings (2.0 default)** | **0.823** | [longmemeval-s-2026-09-01-local.md](longmemeval-s-2026-09-01-local.md) |
| 2026-09-01 | LongMemEval-S (v1) | zero-llm, stopword-filtered FTS | 0.668 | [longmemeval-s-2026-09-01-fts.md](longmemeval-s-2026-09-01-fts.md) |
| 2026-09-01 | LongMemEval-S (v1) | zero-llm (pre-2.0 FTS) | 0.617 | [longmemeval-s-2026-09-01.md](longmemeval-s-2026-09-01.md) |

The 2.0 retrieval work moved overall hit@5 from **0.617 → 0.823**
(+20.6 points; hit@1 0.449 → 0.536, recall@5 0.472 → 0.680) in two
measured steps: dropping stopwords from bare-query FTS OR-joins
(+5.1 hit@5, +8.5 hit@1), then the in-process local embedder with
correct masked-mean pooling — the pooling fix alone was worth ~6.6
points over a padded-attention implementation, caught by the
calibration tests. Every intermediate number was measured on this
harness before the next change landed. For context, published
embedding-based numbers on this dataset: agentmemory 0.967 R@5
(hybrid + reranking), doobidoo/mcp-memory-service 0.804 R@5.

## A/B comparisons (R2)

The harness can also A/B two configs over the same question set and report
an **accuracy + latency + context-tokens** triple with a baseline→candidate
delta — see [retrieval-ab-r2.md](retrieval-ab-r2.md). Full-dataset A/B
baselines are published there once run.

## Reading the numbers

- **mode: local embeddings** is the 2.0 default: the in-process
  all-MiniLM embedder (no API key, no egress) fused with FTS5 + entity +
  graph. **mode: zero-llm** is the deterministic floor (`embedding_provider
  = "none"`, or any host where the model cannot load): FTS5 +
  entity/graph only.
- **Comparability.** Published numbers from other systems on this dataset
  (agentmemory 0.967 R@5, doobidoo/mcp-memory-service 0.804 R@5) are
  embedding-based retrieval over raw chat logs. Our `hit@5` is the
  comparable statistic, but our pipeline additionally pays for
  production-shaped capture: excerpts are bounded at the 2 KB privacy
  boundary, so evidence deep inside one long turn is genuinely out of
  reach of the index. That cost is real and deliberate — the benchmark
  measures the shipped system, not an idealised retriever.
- **Regression gate.** Roadmap items 2-6 re-run this benchmark; a change
  that lowers a slice materially is a regression to fix, not a note to
  publish.
