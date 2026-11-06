# LongMemEval-S release benchmark: 3.4.3

3.4.3 changes how search tokenizes hyphenated words (#2019/#2013, migration
V100), so this release re-ran the retrieval benchmark. It covers all 470
answerable LongMemEval-S questions through the shipped hook capture pipeline,
in the default zero-LLM FTS mode and with the in-process local embedder, both
with the observation stream on.

## Provenance

- code: both modes used `53003f7cffd6fdd1d25503ed64cbaf9ba798444c`, the frozen
  3.4.3 release candidate. The full CI matrix (Linux, macOS, Windows) passed on
  that commit.
- dataset: `longmemeval_s`, sha256
  `08d8dad4be43ee2049a22ff5674eb86725d0ce5ff434cde2627e5e8e7e117894`.
- hardware: AMD Ryzen 9 7950X3D (32 threads), with nothing else running.
  The data dir was the harness default under `/tmp`, which on this host is a
  loop device (see the 3.3.3 report on how that slows ingest).
- commands, after building `ai-memory` and `ai-memory-eval` in release mode:

  ```bash
  ai-memory-eval retrieval \
    --server-bin target/release/ai-memory \
    --concurrency 6 --http-timeout-secs 900 \
    --server-env AI_MEMORY_RETRIEVAL__OBSERVATION_STREAM=true \
    --server-env AI_MEMORY_EVAL_SKIP_WIKI_CHECKPOINTS=1

  ai-memory-eval retrieval \
    --server-bin target/release/ai-memory \
    --concurrency 6 --http-timeout-secs 900 \
    --embeddings local --no-embedding-cache \
    --server-env AI_MEMORY_RETRIEVAL__OBSERVATION_STREAM=true \
    --server-env AI_MEMORY_EVAL_SKIP_WIKI_CHECKPOINTS=1
  ```

- run dirs: `evals/runs/2026-11-06T18-07-14-919512464Z-retrieval` (FTS) and
  `evals/runs/2026-11-06T18-24-27-023539709Z-retrieval` (local, cold).

The previous report is [3.4.0](longmemeval-s-3.4.0-release.md); 3.4.1 and
3.4.2 carried its numbers forward. The table also keeps 3.2.6 for reference.

## Results

| mode | hit@1 | hit@3 | hit@5 | hit@10 | recall@5 | recall@10 | p50 | p95 | mean context tokens |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| FTS 3.2.6 | 0.857 | 0.930 | 0.951 | 0.972 | 0.893 | 0.937 | 11 ms | 73 ms | 534.8 |
| FTS 3.4.0 | 0.857 | 0.928 | 0.951 | 0.972 | 0.892 | 0.936 | 15 ms | 114 ms | 518.6 |
| **FTS 3.4.3** | **0.851** | **0.923** | **0.951** | **0.970** | **0.891** | **0.936** | **12 ms** | **119 ms** | **522.9** |
| local 3.2.6 | 0.766 | 0.904 | 0.945 | 0.977 | 0.879 | 0.947 | 157 ms | 363 ms | 535.2 |
| local 3.4.0 | 0.764 | 0.906 | 0.940 | 0.972 | 0.876 | 0.941 | 129 ms | 307 ms | 519.6 |
| **local 3.4.3** | **0.768** | **0.906** | **0.943** | **0.970** | **0.879** | **0.935** | **128 ms** | **327 ms** | **523.8** |

FTS `hit@5` is identical to 3.4.0 in every slice. The `hit@1` and `hit@3` dips
are three questions each and sit inside the usual run-to-run noise (about
±0.005 overall). Local `hit@5` rises by one question overall:

| slice | n | local 3.4.0 | local 3.4.3 | delta |
|---|---:|---:|---:|---:|
| knowledge-update | 72 | 1.000 | 1.000 | 0.000 |
| multi-session | 121 | 0.975 | 0.975 | 0.000 |
| single-session-assistant | 56 | 0.893 | 0.893 | 0.000 |
| single-session-preference | 30 | 0.867 | 0.867 | 0.000 |
| single-session-user | 64 | 0.953 | 0.969 | +0.016 |
| temporal-reasoning | 127 | 0.906 | 0.906 | 0.000 |

## Latency and ingest

Query latency is in line with 3.4.0 for both modes.

FTS ingested 278,946 items in 1,031 seconds of wall clock, against 1,042 s for
3.3.3 on the same `/tmp` loop device and 937 s for 3.4.0 on a busy host. Ingest
on this host follows the data-dir storage and the host load, as the 3.3.3 and
3.4.0 reports measured, so this is in line with them; the second FTS spelling
that V100 indexes adds work but no visible step. The cold local-embedding
ingest took 1,519 s, faster than 3.4.0's 2,215 s.

## Verdict

No retrieval regression. FTS `hit@5` matches 3.4.0 in every slice, local
`hit@5` improves by one question, and query latency and ingest are in line with
3.4.0 on the same host and storage.
