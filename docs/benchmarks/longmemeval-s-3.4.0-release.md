# LongMemEval-S: 3.4.0 release

Retrieval benchmark for the 3.4.0 minor release, compared with the
[3.3.3 release report](longmemeval-s-3.3.3-release.md). The range touched
retrieval inputs (the sanitizer, the capture policy, store reader refactors
and the eval harness), so the benchmark was re-run on the release commit.

## Setup

- code: `7bb659ce`, the `release: v3.4.0` commit on `main`.
- dataset: `longmemeval_s`, sha256
  `08d8dad4be43ee2049a22ff5674eb86725d0ce5ff434cde2627e5e8e7e117894`.
- hardware: AMD Ryzen 9 7950X3D (32 threads). The host was busy during the
  full runs: an unrelated process held about four cores throughout, and the
  load average ranged from about 25 to 55.
- data dir: `TMPDIR=/mnt/data/Projects/ai-memory/.work/bench-tmp` (NVMe), for
  every run in this report.
- commands: the same as 3.3.3, with release builds of `ai-memory` and
  `ai-memory-eval`. The local run adds `--no-embedding-cache`, so it embeds
  every document cold (#1973):

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

- run dirs: `evals/runs/2026-11-04T20-37-08-306176572Z-retrieval` (FTS) and
  `evals/runs/2026-11-04T20-53-04-817035456Z-retrieval` (local).

## Results

| mode | hit@1 | hit@3 | hit@5 | hit@10 | recall@5 | recall@10 | p50 | p95 | mean context tokens |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| FTS 3.3.3 | 0.857 | 0.928 | 0.951 | 0.972 | 0.892 | 0.936 | 11 ms | 107 ms | 518.6 |
| **FTS 3.4.0** | **0.857** | **0.928** | **0.951** | **0.972** | **0.892** | **0.936** | **15 ms** | **114 ms** | **518.6** |
| local 3.3.3 | 0.760 | 0.904 | 0.943 | 0.977 | 0.876 | 0.945 | 136 ms | 341 ms | 519.7 |
| **local 3.4.0** | **0.764** | **0.906** | **0.940** | **0.972** | **0.876** | **0.941** | **129 ms** | **307 ms** | **519.6** |

FTS overall matches 3.3.3 at every cutoff and returns the same context.
On a 326-question stratified sample (`--stratify 60`), the 3.3.3 and 3.4.0
servers produced identical per-slice accuracy and recall tables. Local
`hit@5` moved from 0.943 to 0.940, inside the ±0.005 overall noise band. By
slice:

| slice | n | local 3.3.3 hit@5 | local 3.4.0 hit@5 | delta |
|---|---:|---:|---:|---:|
| knowledge-update | 72 | 1.000 | 1.000 | 0.000 |
| multi-session | 121 | 0.975 | 0.975 | 0.000 |
| single-session-assistant | 56 | 0.893 | 0.893 | 0.000 |
| single-session-preference | 30 | 0.867 | 0.867 | 0.000 |
| single-session-user | 64 | 0.953 | 0.953 | 0.000 |
| temporal-reasoning | 127 | 0.913 | 0.906 | -0.007 |

The only slice that moved lost one question, well inside the ±0.02–0.03
small-slice noise. Both runs returned zero raw-observation fallback hits.

## Latency and ingest

The full FTS run took 937.3 seconds of wall clock to ingest 278,946 items,
against 218.7 seconds for 3.3.3 on NVMe. To separate code from host load,
the 3.3.3 release binary (checked against its published sha256) and 3.4.0
were run alternately on the same stratified sample, with the same harness
and data dir:

| order | server | load average at the end | ingest wall clock |
|---|---|---:|---:|
| 1 | 3.3.3 | 7.5 | 157.7 s |
| 2 | 3.4.0 | 28.1 | 209.2 s |
| 3 | 3.3.3 | 33.8 | 216.4 s |
| 4 | 3.4.0 | 4.7 | 177.2 s |

Wall clock follows the host load, not the version: each build has a faster
and a slower run, and the ranges overlap. The full run's slowdown is the busy
host, not a throughput regression. The cold local run ingested in 2,214.8
seconds; 3.3.3 used the embedding cache, so the two local ingest times are
not comparable.

Query latency was 15/114 ms p50/p95 for FTS and 129/307 ms for local, within
the spread of earlier releases on a loaded host.

## Verdict

No retrieval regression. FTS matches 3.3.3 overall and per slice, local
moves within its known variance (#1854), and ingest throughput is unchanged
once host load is accounted for.
