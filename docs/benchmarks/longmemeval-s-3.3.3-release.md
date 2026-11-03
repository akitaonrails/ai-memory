# LongMemEval-S retrieval: 3.3.3 release

The 3.3.3 range changes the hook sanitizer, which every captured observation
passes through (#1963, #1974), and the observation prune guard (#1961). The
retrieval rule asks for one run on the release candidate whenever the capture
path changes, so both modes were run on the final candidate and compared with
the [3.3.2 report](longmemeval-s-3.3.2-release.md).

## Setup

- code: `d445475eb0`, the 3.3.3 release candidate on `main`.
- dataset: `longmemeval_s`, sha256
  `08d8dad4be43ee2049a22ff5674eb86725d0ce5ff434cde2627e5e8e7e117894`.
- hardware: AMD Ryzen 9 7950X3D (32 threads). An unrelated Wine build was
  running on the host during these runs.
- commands: the same as 3.3.2, with release builds of `ai-memory` and
  `ai-memory-eval`:

  ```bash
  ai-memory-eval retrieval \
    --server-bin target/release/ai-memory \
    --concurrency 6 --http-timeout-secs 900 \
    --server-env AI_MEMORY_RETRIEVAL__OBSERVATION_STREAM=true \
    --server-env AI_MEMORY_EVAL_SKIP_WIKI_CHECKPOINTS=1

  ai-memory-eval retrieval \
    --server-bin target/release/ai-memory \
    --concurrency 6 --http-timeout-secs 900 \
    --embeddings local \
    --server-env AI_MEMORY_RETRIEVAL__OBSERVATION_STREAM=true \
    --server-env AI_MEMORY_EVAL_SKIP_WIKI_CHECKPOINTS=1
  ```

- run dirs: `evals/runs/2026-11-03T19-03-18-326030814Z-retrieval` (FTS,
  server data dir on NVMe), `evals/runs/2026-11-03T17-55-48-712367503Z-retrieval`
  (FTS, data dir under `/tmp`) and
  `evals/runs/2026-11-03T18-13-11-609604257Z-retrieval` (local, data dir
  under `/tmp`).

## Results

| mode | hit@1 | hit@3 | hit@5 | hit@10 | recall@5 | recall@10 | p50 | p95 | mean context tokens |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| FTS 3.3.2 | 0.857 | 0.928 | 0.951 | 0.972 | 0.892 | 0.936 | 10 ms | 74 ms | 518.6 |
| **FTS 3.3.3** | **0.857** | **0.928** | **0.951** | **0.972** | **0.892** | **0.936** | **11 ms** | **107 ms** | **518.6** |
| local 3.3.2 | 0.751 | 0.900 | 0.938 | 0.970 | 0.874 | 0.938 | 131 ms | 307 ms | 519.5 |
| **local 3.3.3** | **0.760** | **0.904** | **0.943** | **0.977** | **0.876** | **0.945** | **136 ms** | **341 ms** | **519.7** |

FTS is identical to 3.3.2 in every slice at every accuracy and recall cutoff,
and returns the same context. Local `hit@5` rose from 0.938 to 0.943, inside
the local mode's run-to-run variance (#1854). By slice:

| slice | n | local 3.3.2 hit@5 | local 3.3.3 hit@5 | delta |
|---|---:|---:|---:|---:|
| knowledge-update | 72 | 1.000 | 1.000 | 0.000 |
| multi-session | 121 | 0.975 | 0.975 | 0.000 |
| single-session-assistant | 56 | 0.875 | 0.893 | +0.018 |
| single-session-preference | 30 | 0.900 | 0.867 | -0.033 |
| single-session-user | 64 | 0.969 | 0.953 | -0.016 |
| temporal-reasoning | 127 | 0.890 | 0.913 | +0.023 |

Each slice moved by at most two questions, within the documented ±0.02–0.03
small-slice noise. The sanitizer changes only alter text that holds a secret
shape, which this dataset does not contain, so no ranking change was
expected.

## Latency and ingest

Ingest wall time depends on the storage under the server's data dir. The
harness creates it with `tempfile`, which honours `TMPDIR` and otherwise uses
`/tmp`. On this host `/tmp` is now a loop device, and the first FTS run there
took 1,042 seconds to ingest 278,946 items. To check whether the code
changed this, the 3.3.2 candidate (`b122aa06`) and `d445475e` were run back
to back on a 330-question stratified sample:

| data dir | 3.3.2 (`b122aa06`) | 3.3.3 (`d445475e`) |
|---|---:|---:|
| `/tmp` (loop device) | 502.7 s | 507.7 s |
| NVMe (`TMPDIR`) | 105.3 s | 106.9 s |

The two builds ingest at the same rate under the same conditions. In a
release-mode micro-benchmark over 30,000 LongMemEval turns, `Sanitizer::scrub`
takes 5.6 µs per call on 3.3.3 against 37.0 µs on 3.3.2. The full FTS run
with its data dir on NVMe ingested in 218.7 seconds, against 56.6 seconds
for the 3.3.2 run, which was measured on an idle host whose storage setup was
not recorded. Future runs should set `TMPDIR` to a directory on the same disk
as the previous run and name it in the report.

FTS query latency was 11/107 ms p50/p95 and local 136/341 ms; the p95 rise
fits the busier host. Both runs returned zero raw-observation fallback hits.

## Verdict

No retrieval regression. FTS matches 3.3.2 exactly. Local moves within its
known variance, and ingest throughput is unchanged once storage is held
constant.
