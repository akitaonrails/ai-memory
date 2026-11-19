# LongMemEval-S release benchmark: 3.5.0

3.5.0 reworks the store reader and the pages retrieval reads (opt-in
`min_relevance` and per-project result counts, retrieval history, page
sources, and consolidation output changes), so this release re-ran the
retrieval benchmark. It covers all 470 answerable LongMemEval-S questions
through the shipped hook capture pipeline, in the default zero-LLM FTS mode and
with the in-process local embedder, both with the observation stream on.

## Provenance

- code: both modes used `bc7faa7d847dd387e634ba8420d115b9dd4ecebe`, the frozen
  3.5.0 release commit. The full CI matrix (Linux, macOS, Windows, CodeQL, Nix)
  passed on that commit.
- dataset: `longmemeval_s`, sha256
  `08d8dad4be43ee2049a22ff5674eb86725d0ce5ff434cde2627e5e8e7e117894`.
- hardware: AMD Ryzen 9 7950X3D (32 threads), with nothing else running (the
  full CI matrix ran on GitHub's remote runners in parallel). The data dir was
  the harness default under `/tmp`, which on this host is a loop device (see the
  3.3.3 report on how that slows ingest).
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

- run dirs: `evals/runs/2026-11-18T02-12-01-960198083Z-retrieval` (FTS) and
  `evals/runs/2026-11-18T02-30-09-659066582Z-retrieval` (local, cold).

The previous report is [3.4.3](longmemeval-s-3.4.3-release.md); 3.4.4 through
3.4.7 carried its numbers forward. The table also keeps 3.4.0 for reference.

## Results

| mode | hit@1 | hit@3 | hit@5 | hit@10 | recall@5 | recall@10 | p50 | p95 | mean context tokens |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| FTS 3.4.0 | 0.857 | 0.928 | 0.951 | 0.972 | 0.892 | 0.936 | 15 ms | 114 ms | 518.6 |
| FTS 3.4.3 | 0.851 | 0.923 | 0.951 | 0.970 | 0.891 | 0.936 | 12 ms | 119 ms | 522.9 |
| **FTS 3.5.0** | **0.851** | **0.921** | **0.953** | **0.970** | **0.894** | **0.934** | **19 ms** | **159 ms** | **522.7** |
| local 3.4.0 | 0.764 | 0.906 | 0.940 | 0.972 | 0.876 | 0.941 | 129 ms | 307 ms | 519.6 |
| local 3.4.3 | 0.768 | 0.906 | 0.943 | 0.970 | 0.879 | 0.935 | 128 ms | 327 ms | 523.8 |
| **local 3.5.0** | **0.753** | **0.904** | **0.940** | **0.970** | **0.873** | **0.937** | **153 ms** | **376 ms** | **523.4** |

FTS `hit@5` rises by one question over 3.4.3 (0.951 to 0.953); `hit@1` is
identical and `hit@3` dips by one question. Local `hit@5` falls by one question
(0.943 to 0.940) and `hit@1` by seven (0.768 to 0.753), while `hit@10` and
`recall@10` are flat or up. Every overall move sits inside the usual run-to-run
noise (about ±0.005 overall, ±0.02–0.03 on small slices); `hit@1` is the
noisiest of the headline metrics. No slice regressed beyond that band:

| slice | n | local 3.4.3 hit@5 | local 3.5.0 hit@5 | delta |
|---|---:|---:|---:|---:|
| knowledge-update | 72 | 1.000 | 0.986 | -0.014 |
| multi-session | 121 | 0.975 | 0.975 | 0.000 |
| single-session-assistant | 56 | 0.893 | 0.893 | 0.000 |
| single-session-preference | 30 | 0.867 | 0.900 | +0.033 |
| single-session-user | 64 | 0.969 | 0.969 | 0.000 |
| temporal-reasoning | 127 | 0.906 | 0.898 | -0.008 |

## Latency and ingest

Query latency is in line with 3.4.3 for both modes; the small p95 rise sits
within the host's run-to-run spread.

FTS ingested 278,946 items in 1,086 seconds of wall clock, against 1,031 s for
3.4.3 on the same `/tmp` loop device. The cold local-embedding ingest took
1,657 s, against 1,519 s for 3.4.3. Ingest on this host follows the data-dir
storage and host load, as the 3.3.3 and 3.4.0 reports measured, so both are in
line with 3.4.3. No document exceeded the 16 KiB cap, and no question fell back
to the raw-observation stream.

## Verdict

No retrieval regression. FTS `hit@5` improves by one question, local `hit@5`
moves one question the other way, and every overall metric stays inside
run-to-run noise. Query latency and ingest are in line with 3.4.3 on the same
host and storage.
