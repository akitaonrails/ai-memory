# LongMemEval-S release benchmark: 3.3.2

The 3.3.2 release benchmark runs all 470 answerable LongMemEval-S questions
through the shipped hook capture pipeline, for the default zero-LLM FTS path
and for the in-process local embedder, both with the production-default
observation stream on. It ran because the patch range changes retrieval code:
full-text search now splits paths on `/` (#1921, #1922, migration V97).

## Provenance

- code: both modes used `b122aa0604`, the frozen 3.3.2 release candidate
  before this report.
- dataset: `longmemeval_s`, sha256
  `08d8dad4be43ee2049a22ff5674eb86725d0ce5ff434cde2627e5e8e7e117894`.
- hardware: AMD Ryzen 9 7950X3D (32 threads), with no local builds running.
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
    --embeddings local \
    --server-env AI_MEMORY_RETRIEVAL__OBSERVATION_STREAM=true \
    --server-env AI_MEMORY_EVAL_SKIP_WIKI_CHECKPOINTS=1
  ```

- run dirs: `evals/runs/2026-11-03T03-33-36-690613862Z-retrieval` (FTS)
  and `evals/runs/2026-11-03T03-34-34-121176009Z-retrieval` (local).

## Results

| mode | hit@1 | hit@3 | hit@5 | hit@10 | recall@5 | recall@10 | p50 | p95 | mean context tokens |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| FTS 3.3.0 | 0.857 | 0.930 | 0.951 | 0.972 | 0.893 | 0.937 | 11 ms | 73 ms | 534.8 |
| **FTS 3.3.2** | **0.857** | **0.928** | **0.951** | **0.972** | **0.892** | **0.936** | **10 ms** | **74 ms** | **518.6** |
| local 3.3.0 | 0.762 | 0.909 | 0.945 | 0.970 | 0.876 | 0.940 | 141 ms | 330 ms | 534.5 |
| **local 3.3.2** | **0.751** | **0.900** | **0.938** | **0.970** | **0.874** | **0.938** | **131 ms** | **307 ms** | **519.5** |

FTS keeps `hit@5` and `hit@10`. `hit@3` moved by one question. Mean returned
context fell by about 3%, which fits the path-splitting change: a hit on a
file path now matches its name parts instead of the whole path token.

Local `hit@5` moved by 0.007 (about three questions), just outside the
documented ±0.005 overall noise band. `hit@10` held. By slice:

| slice | n | 3.3.0 | 3.3.2 | delta |
|---|---:|---:|---:|---:|
| knowledge-update | 72 | 1.000 | 1.000 | 0.000 |
| multi-session | 121 | 0.975 | 0.975 | 0.000 |
| single-session-assistant | 56 | 0.893 | 0.875 | -0.018 |
| single-session-preference | 30 | 0.900 | 0.900 | 0.000 |
| single-session-user | 64 | 0.953 | 0.969 | +0.016 |
| temporal-reasoning | 127 | 0.913 | 0.890 | -0.024 |

## Is the local movement a regression?

The 3.3.0 run directory was not kept, so the per-question comparison uses the
3.2.4 run (`2026-10-25T15-38-37-620897357Z-retrieval`), which had the same
local `hit@5` of 0.945. Against it, FTS changed no question at `hit@5`. Local
lost five questions at `hit@5` (four of them temporal-reasoning) and gained
two.

Those seven questions were then re-run twice on the same 3.3.2 binary,
once at concurrency 6 and once at 1. Between those two runs, four of the
seven changed result (`57f827a0`, `gpt4_68e94288` and `gpt4_b5700ca0`
flipped; `5809eb10`, lost in the full run, hit in both re-runs). The same
binary answers these questions differently from run to run, which puts this
movement inside the local mode's run-to-run variance. #1854 investigates that
variance, and the evidence is recorded there.

## Latency and ingest

FTS query latency was 10/74 ms p50/p95 and local 131/307 ms. FTS ingested
278,946 items in 56.6 seconds, and local embeddings ingested the same items
in 1,583.1 seconds. Ingest wall time is load-sensitive and not a release
gate. Both runs returned zero raw-observation fallback hits.

## Verdict

FTS holds its primary metric and returns less context. The local `hit@5`
movement does not block the release under the benchmark policy; the numbers
are published as measured, and #1854 keeps tracking the local variance.
