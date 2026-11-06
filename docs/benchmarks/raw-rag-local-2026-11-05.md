# Raw-RAG local-model smoke comparison

This receipt checks the new development-evaluator engine against the existing
ingestion modes. It is a small retrieval experiment, not a release gate,
a representative quality estimate, or a reproduction of the Raw-RAG paper's
answer-accuracy numbers.

## Provenance

- implementation: `783aa8638682575d1151e005f19f2533ae90e221`, based on
  `release/3.5` at `bf8b6d06b591b553bd0327a480eb458cf0913e4e`;
- hardware: Apple M5 Max, 18 logical CPUs, macOS ARM64;
- build: `cargo build --locked --release -p ai-memory-eval -p ai-memory-cli`;
- all configurations ran sequentially, with question concurrency 1;
- cutoffs: 1, 3, 5, 10, 15; no generative QA calls;
- document-embedding caches disabled; server modes used the evaluator's
  `AI_MEMORY_EVAL_SKIP_WIKI_CHECKPOINTS=1` instrument;
- models: all-MiniLM-L6-v2 and cross-encoder/ms-marco-MiniLM-L6-v2 on CPU.
  The evaluator verifies every file's SHA-256; model pins, selected windows,
  logits and stage timings are included in each Raw-RAG JSON report.

The protocol follows [arXiv:2610.30755v1 Appendix B](https://arxiv.org/html/2610.30755v1#A2)
with 256-token windows, overlap 32, dense top-128 and cross-encoder top-15.
It uses WordPiece and local MiniLM models, not the paper's tokenizer/models;
windows stay within each supplied session. The expanded condition displays
complete intersecting turns, retaining repetitions. The windows-only control
kept exactly the same selected windows and logits on both datasets.

## Datasets and selection

- `coding_v1`: all four bundled coding questions, sha256
  `af3382663a3e4e969c8bcfa744b3a794de86cb5932393a88bea16f014193d467`.
- `longmemeval_s`: `--stratify 1`, the first scored question of each of six
  categories in the pinned dataset, sha256
  `08d8dad4be43ee2049a22ff5674eb86725d0ce5ff434cde2627e5e8e7e117894`.
  This is a deterministic diagnostic sample, not random or representative.

The [per-question CSV](raw-rag-local-2026-11-05.csv) contains every measured
row, including the selected question ids and categories. No question or
configuration was dropped or retuned after seeing a score.

## Measurements

Cutoffs score distinct source sessions; multiple windows from one session
do not inflate recall. Context includes every displayed title and passage,
including repeated expanded turns, using the common chars/4 estimate rather
than a provider tokenizer. Query latency excludes ingestion/indexing, model
loading and downloads. Raw-RAG is in-process inference; server times include
the MCP round trip, so these timings describe different mechanisms.
Ingest/index times are also mechanism-specific and are reported separately.

### Coding fixture (4 questions)

| configuration | hit@1 | hit@5 | recall-all@5 | context mean | query p95 ms | ingest/index mean ms |
|---|---:|---:|---:|---:|---:|---:|
| raw-expanded | 1.000 | 1.000 | 1.000 | 1099.2 | 619 | 267.8 |
| raw-windows | 1.000 | 1.000 | 1.000 | 1024.2 | 598 | 270.0 |
| hooks-none | 1.000 | 1.000 | 1.000 | 213.2 | 5 | 336.8 |
| hooks-local | 0.500 | 1.000 | 1.000 | 445.0 | 14 | 1073.2 |
| whole-session-local | 0.500 | 1.000 | 1.000 | 577.8 | 14 | 510.8 |
| captured-session-local | 0.750 | 1.000 | 1.000 | 563.5 | 13 | 505.0 |
| whole-session-none | 1.000 | 1.000 | 1.000 | 163.0 | 4 | 359.0 |
| captured-session-none | 0.750 | 1.000 | 1.000 | 164.2 | 2 | 302.0 |

### LongMemEval-S diagnostic sample (6 questions)

| configuration | hit@1 | hit@5 | recall-all@5 | context mean | query p95 ms | ingest/index mean ms |
|---|---:|---:|---:|---:|---:|---:|
| raw-expanded | 1.000 | 1.000 | 1.000 | 11926.0 | 8395 | 31927.7 |
| raw-windows | 1.000 | 1.000 | 1.000 | 3869.8 | 12164 | 42629.0 |
| hooks-none | 1.000 | 1.000 | 1.000 | 807.2 | 13 | 3092.7 |
| hooks-local | 0.833 | 1.000 | 1.000 | 809.8 | 17 | 8037.3 |
| whole-session-local | 0.833 | 1.000 | 1.000 | 795.8 | 19 | 7335.7 |
| captured-session-local | 0.833 | 1.000 | 1.000 | 784.0 | 20 | 7538.0 |

## Interpretation

The coding fixture already reaches 4/4 top-one hits through the shipped
hook/FTS path. Raw-RAG also reaches 4/4, with substantially more reader
context and a cross-encoder pass. This run does not establish a need to
replace production retrieval.

The configurations differ in several ways: windowing, ranking, reranking,
capture and display budgets. A change cannot be attributed to windowing
alone. The expansion control isolates display cost because its selections
and retrieval scores stayed identical. Neither session recall nor these
fixtures establish semantic evidence sufficiency, causal-path retention or
answer accuracy. Those require the optional QA path and broader evaluation.

## Reproduce

After explicitly downloading the pinned models as described in
[`evals/README.md`](../../evals/README.md#retrieval---engine-raw-rag--source-preserving-local-baseline),
run from the repository root. The following commands cover the six-question
LongMemEval sample; for the complete coding fixture use `--dataset-variant
coding-v1` and omit `--stratify 1`. The receipt also ran `whole-session` and
`captured-session` with `--embeddings none` on coding-v1.

```bash
target/release/ai-memory-eval retrieval --engine raw-rag \
  --dataset-variant s --stratify 1 --ks 1,3,5,10,15 --concurrency 1
target/release/ai-memory-eval retrieval --engine raw-rag \
  --dataset-variant s --stratify 1 --ks 1,3,5,10,15 --concurrency 1 \
  --raw-rag-no-turn-expansion

for mode in hooks whole-session captured-session; do
  target/release/ai-memory-eval retrieval --dataset-variant s --stratify 1 \
    --ingest "$mode" --embeddings local --concurrency 1 --ks 1,3,5,10,15 \
    --no-embedding-cache --http-timeout-secs 900 \
    --server-env AI_MEMORY_EVAL_SKIP_WIKI_CHECKPOINTS=1
done
target/release/ai-memory-eval retrieval --dataset-variant s --stratify 1 \
  --ingest hooks --embeddings none --concurrency 1 --ks 1,3,5,10,15 \
  --no-embedding-cache --http-timeout-secs 900 \
  --server-env AI_MEMORY_EVAL_SKIP_WIKI_CHECKPOINTS=1
```

Use the same cutoffs and question selection with `retrieval-compare` to
inspect movements question by question. No full-set LongMemEval run,
generative answer-quality evaluation, native Linux/Windows execution or
statistical confidence interval is claimed by this receipt.
