# Benchmarks

Published retrieval-quality numbers for ai-memory, with full provenance
(commit, dataset sha256, hardware, mode). Every number here was produced
by the in-repo harness. `evals/README.md` explains how to reproduce them:

```bash
cargo build --release -p ai-memory-cli
cargo run --release -p ai-memory-eval -- retrieval --fetch
```

## Baselines

| date | dataset | mode | overall hit@5 | file |
|---|---|---|---|---|
| **2026-10-29** | LongMemEval-S (v1) | **zero-llm FTS + observation stream (3.3.0 release)** | **0.951** | [longmemeval-s-3.3.0-release.md](longmemeval-s-3.3.0-release.md) |
| **2026-10-29** | LongMemEval-S (v1) | **local embeddings + observation stream (3.3.0 release)** | **0.945** | [longmemeval-s-3.3.0-release.md](longmemeval-s-3.3.0-release.md) |
| **2026-10-26** | LongMemEval-S (v1) | **zero-llm FTS + observation stream (3.2.6 release)** | **0.951** | [longmemeval-s-3.2.6-release.md](longmemeval-s-3.2.6-release.md) |
| **2026-10-26** | LongMemEval-S (v1) | **local embeddings + observation stream (3.2.6 release)** | **0.945** | [longmemeval-s-3.2.6-release.md](longmemeval-s-3.2.6-release.md) |
| **2026-10-25** | LongMemEval-S (v1) | **zero-llm FTS + observation stream (3.2.4 release)** | **0.951** | [longmemeval-s-3.2.4-release.md](longmemeval-s-3.2.4-release.md) |
| **2026-10-25** | LongMemEval-S (v1) | **local embeddings + observation stream (3.2.4 release)** | **0.945** | [longmemeval-s-3.2.4-release.md](longmemeval-s-3.2.4-release.md) |
| **2026-10-22** | LongMemEval-S (v1) | **zero-llm FTS + observation stream (3.2.2 release)** | **0.951** | [longmemeval-s-3.2.2-release.md](longmemeval-s-3.2.2-release.md) |
| **2026-10-22** | LongMemEval-S (v1) | **local embeddings + observation stream (3.2.2 release)** | **0.945** | [longmemeval-s-3.2.2-release.md](longmemeval-s-3.2.2-release.md) |
| **2026-10-22** | LongMemEval-S (v1) | **zero-llm FTS + observation stream (3.2.1 release)** | **0.951** | [longmemeval-s-3.2.1-release.md](longmemeval-s-3.2.1-release.md) |
| **2026-10-22** | LongMemEval-S (v1) | **local embeddings + observation stream (3.2.1 release)** | **0.949** | [longmemeval-s-3.2.1-release.md](longmemeval-s-3.2.1-release.md) |
| **2026-10-22** | LongMemEval-S (v1) | **zero-llm FTS + observation stream (3.2.0 release)** | **0.951** | [longmemeval-s-3.2.0-release.md](longmemeval-s-3.2.0-release.md) |
| **2026-10-22** | LongMemEval-S (v1) | **local embeddings + observation stream (3.2.0 release)** | **0.938** | [longmemeval-s-3.2.0-release.md](longmemeval-s-3.2.0-release.md) |
| **2026-10-20** | LongMemEval-S (v1) | **zero-llm FTS + observation stream (3.1.2 release)** | **0.951** | [longmemeval-s-3.1.2-release.md](longmemeval-s-3.1.2-release.md) |
| **2026-10-20** | LongMemEval-S (v1) | **local embeddings + observation stream (3.1.2 release)** | **0.947** | [longmemeval-s-3.1.2-release.md](longmemeval-s-3.1.2-release.md) |
| **2026-10-16** | LongMemEval-S (v1) | **zero-llm FTS + observation stream (3.1 release)** | **0.951** | [longmemeval-s-3.1.0-release.md](longmemeval-s-3.1.0-release.md) |
| **2026-10-16** | LongMemEval-S (v1) | **local embeddings + observation stream (3.1 release)** | **0.940** | [longmemeval-s-3.1.0-release.md](longmemeval-s-3.1.0-release.md) |
| **2026-10-15** | LongMemEval-S (v1) | **zero-llm FTS + observation stream (3.0.1 release)** | **0.951** | [longmemeval-s-3.0.1-release.md](longmemeval-s-3.0.1-release.md) |
| **2026-10-15** | LongMemEval-S (v1) | **local embeddings + observation stream (3.0.1 release)** | **0.943** | [longmemeval-s-3.0.1-release.md](longmemeval-s-3.0.1-release.md) |
| **2026-10-13** | LongMemEval-S (v1) | **zero-llm FTS + observation stream (3.0.0 release; on by default)** | **0.951** | [longmemeval-s-3.0.0-release.md](longmemeval-s-3.0.0-release.md) |
| **2026-10-13** | LongMemEval-S (v1) | **local embeddings + observation stream (3.0.0 release; on by default)** | **0.945** | [longmemeval-s-3.0.0-release.md](longmemeval-s-3.0.0-release.md) |
| 2026-09-21 | LongMemEval-S (v1) | local embeddings (historical 2.4 RC) | 0.815 | [retrieval-ab-r2.md](retrieval-ab-r2.md) |
| 2026-09-21 | LongMemEval-S (v1) | zero-llm, stopword-filtered FTS (2.4 RC) | 0.666 | [retrieval-ab-r2.md](retrieval-ab-r2.md) |
| 2026-09-01 | LongMemEval-S (v1) | local embeddings (2.0 default, prior) | 0.823 | [longmemeval-s-2026-09-01-local.md](longmemeval-s-2026-09-01-local.md) |
| 2026-09-01 | LongMemEval-S (v1) | zero-llm, stopword-filtered FTS (prior) | 0.668 | [longmemeval-s-2026-09-01-fts.md](longmemeval-s-2026-09-01-fts.md) |
| 2026-09-01 | LongMemEval-S (v1) | zero-llm (pre-2.0 FTS) | 0.617 | [longmemeval-s-2026-09-01.md](longmemeval-s-2026-09-01.md) |

The 3.3.0 minor-release run found FTS identical to 3.2.6 at every accuracy
and recall cutoff and kept local `hit@5` exact. Local `hit@10` and
`recall@10` each moved by 0.007, slightly outside the documented overall
noise band, and are tracked in #1854. See the
[3.3.0 release report](longmemeval-s-3.3.0-release.md) for provenance, every
slice, and the explicit threshold assessment.

The 3.2.6 patch-release run found FTS identical to 3.2.4 at every accuracy
and recall cutoff. Local embeddings kept the same `hit@5` and `hit@10`
overall, while `recall@10` improved. See the
[3.2.6 release report](longmemeval-s-3.2.6-release.md) for provenance, slice
movement, and the load-sensitive latency comparison.

The 3.2.4 patch-release run found FTS identical to 3.2.2 at every accuracy
cutoff and local embeddings unchanged at `hit@5` overall and on every slice.
Local `hit@1` and `hit@10` improved. See the
[3.2.4 release report](longmemeval-s-3.2.4-release.md) for provenance and the
load-sensitive latency and ingest comparison.

The 3.2.2 patch-release run found FTS identical to 3.2.1 on every question.
Local embeddings moved from 0.949 to 0.945 `hit@5`, inside the pre-registered
overall noise band; the two changed questions remain within the top ten, and
`hit@10` improved from 0.968 to 0.974. See the
[3.2.2 release report](longmemeval-s-3.2.2-release.md) for provenance, every
slice, and the explicit threshold assessment.

The 3.2.1 patch-release run recovered 3.2.0's local embedding accuracy and
documented the remaining boundary-question variation. See the
[3.2.1 release report](longmemeval-s-3.2.1-release.md) for the capacity-fix
measurement and controlled latency follow-up.

The 2026-10-22 minor-release run found FTS identical to 3.1.2 on every
question. Local embeddings fell from 0.947 to 0.938 `hit@5` (five questions
lost, one gained), with higher query latency and 75% longer ingest. That is
outside the +/-0.005 noise band and is tracked as a possible regression for
the next release. See the [3.2.0 release report](longmemeval-s-3.2.0-release.md)
for every slice, ingest performance, the suspected commit and provenance.

The 2026-10-20 patch-release run found no retrieval regression against 3.1.
FTS is identical at every accuracy cutoff. Local embeddings improved from
0.940 to 0.947 `hit@5`, with stable latency and context size. See the
[3.1.2 release report](longmemeval-s-3.1.2-release.md) for every slice,
ingest performance, commands, and exact provenance.

The 2026-10-16 minor-release run found no material retrieval regression
against 3.0.1. FTS is identical at every overall cutoff. Local embeddings
moved from 0.943 to 0.940 `hit@5`, inside the pre-registered +/-0.005 noise
band; its latency and context size stayed stable. See the
[3.1 release report](longmemeval-s-3.1.0-release.md) for every slice, ingest
performance, commands, and the explicit post-measurement commit boundary.

The 2026-10-15 patch-release re-run found no retrieval regression against
3.0.0. FTS accuracy is identical (`hit@5` 0.951), while local embeddings
moved 0.945 -> 0.943, inside the +/-0.005 overall noise band. Query p50/p95
improved from 13/112 to 12/101 ms for FTS and from 99/232 to 98/228 ms for
local embeddings. See the [3.0.1 release report](longmemeval-s-3.0.1-release.md)
for slice results, ingest performance, commands, and exact provenance.

The 2026-10-13 rows are the 3.0.0 release benchmark: all 470 questions on the
release-candidate code, comparing the stream off and on with the same build
([release report](longmemeval-s-3.0.0-release.md)). Zero-LLM FTS `hit@5` went
0.662 → 0.951 (hit@1 0.511 → 0.857, hit@10 0.687 → 0.972; 139 questions
gained, 3 lost) and local embeddings 0.800 → 0.945 (hit@1 0.543 → 0.766; 72
gained, 4 lost). Context tokens for the top five hits rose 3.8% (FTS) and
17.0% (local). Against the last published release numbers (2.4/2.6: 0.666 FTS,
0.815 local), that is +0.285 and +0.130, and every slice improved. The
stream-off local baseline (0.800 against 0.815) is tracked in
[#1263](https://github.com/akitaonrails/ai-memory/issues/1263). These numbers come from
the harness corrected on 2026-10-13, which replays each question's sessions
under their own session ids; the first published stream figures (0.666 →
0.868) came from a harness that reused them across questions and left a few
sessions without a page. Details and commands are in the
decomposition report below.

The 2026-09-21 (2.4 RC) re-run shows no default-ranking regression: local
`hit@5` 0.823 → 0.815 and FTS `hit@5` 0.668 → 0.666 are identical within
run-to-run noise. 2.4's aging/retrieval features are opt-in and off by default,
so default retrieval is unchanged. The full triple and provenance are in
[retrieval-ab-r2.md](retrieval-ab-r2.md).

The 2.0 retrieval work moved overall hit@5 from 0.617 → 0.823
(+20.6 points; hit@1 0.449 → 0.536, recall@5 0.472 → 0.680) in two
measured steps: dropping stopwords from bare-query FTS OR-joins
(+5.1 hit@5, +8.5 hit@1), then the in-process local embedder with
correct masked-mean pooling. The pooling fix alone was worth ~6.6
points over a padded-attention implementation, which the
calibration tests caught. Every intermediate number was measured on this
harness before the next change landed.

## Decomposition: pipeline vs component

[longmemeval-s-decomposition-2026-10-12.md](longmemeval-s-decomposition-2026-10-12.md)
splits the pipeline number into capture cost, ranking quality and harness
artifacts. With each raw session indexed whole (component mode, ranking
only), zero-LLM FTS scores 0.972 `hit@5` on all 470 questions, against 0.662
through the shipped capture path. The gap was not the 2 KB excerpt cap: pages
holding exactly the text capture keeps score 0.968. It was the zero-LLM
session page, which indexed only 80-character turn titles and no assistant
text. The harness's per-turn date prefix used about half of each title, which
is an artifact of the harness, not of production capture. The observation
stream recovered nearly all of that FTS gap (0.662 → 0.951).

## A/B comparisons (R2)

The [2026-10-15 consolidation literal-fidelity
run](consolidation-literal-fidelity-2026-10-15.md) compares the default
projection with the opt-in `verbatim_residue` request on the same model over
three runs. The residue reduced generated-session projection loss from 8/15
facts to 3/15 and recovered a stable 1/5 newly visible facts on the pages,
without changing kept-fact recall, parse rate or invention counts. It is a
prompt-input benchmark and is separate from the retrieval baselines above.

The harness can also A/B two configs over the same question set and report
an accuracy + latency + context-tokens triple with a baseline→candidate
delta (see [retrieval-ab-r2.md](retrieval-ab-r2.md)). That file publishes the
full-dataset A/B (FTS baseline → local candidate, 470 scored) from the
2026-09-21 (2.4 RC) run: local embeddings add +0.149 hit@5 / +0.254
recall@10 over zero-LLM FTS for ~90 ms p50 latency, and the
baseline-vs-baseline determinism check is clean (all accuracy/token deltas `0.000`).

## Reading the numbers

- **Evidence coverage.** `hit@k` credits any evidence session, `recall@k`
  measures the fraction found, and `recall-all@k` requires every evidence
  session in the top k distinct retrieved sessions. Each metric is averaged
  per question; abstention questions are excluded. Historical reports that
  omit `recall-all@k` do not establish complete-evidence recall.
- **mode: local embeddings** is the 2.0 default: the in-process
  all-MiniLM embedder (no API key, no egress) fused with FTS5 + entity +
  graph. **mode: zero-llm** is the deterministic floor (`embedding_provider
  = "none"`, or any host where the model cannot load): FTS5 +
  entity/graph only.
- **Comparability.** Our `hit@5` is the statistic other systems publish as
  R@5 (any evidence session in the top 5). doobidoo/mcp-memory-service
  publishes 0.804 R@5. agentmemory publishes 95.2% `recall_any@5` (April
  2026), measured by indexing each whole raw session directly (BM25 plus
  vectors, reranker disabled), which is a component measurement. Our
  pipeline numbers also pay for capture, so they are not like-for-like with
  a component number, and even our component numbers use a
  different index and document construction. Our grep engine reproduces
  agentmemory's grep adapter on all 470 questions, so the question set and
  metric match, but no ranking between the two systems is claimed. The decomposition report has the
  details. agentmemory's 2026-10-09 baseline adds 84.68% strict
  recall-all@5 and 452/470 any-hit, which its authors say supports no
  competitive claim. MemPalace publishes 96.6% R@5 raw and 98.4% on 450
  held-out questions, also retrieval-only and from a different harness.
  Mem0, Zep, Hindsight and EverOS publish end-to-end QA accuracy, where an
  LLM answers from retrieved context and an LLM judges the answer; those are
  a different metric from `hit@5`. [comparison.md](../comparison.md) lists
  each setup.
- **Release policy.** A release that touches retrieval re-runs this
  benchmark once, on the final release candidate, and compares every slice
  with the previous release (AGENTS.md, project maintenance rules). A drop
  does not block the release: the numbers are published and an issue opens
  for the next release to decide whether it is a regression. To see which
  questions moved, run `ai-memory-eval retrieval-compare` on the two runs'
  saved reports ([evals/README.md](../../evals/README.md)).
- **Pre-registration.** Before the release-candidate run, write down in the
  release's tracking issue what the run is meant to show: the question, the
  slices that matter for the change, and the expected direction for each.
  Judge the result against run-to-run noise (about ±0.005 overall,
  ±0.02–0.03 on small slices), not a percentage threshold. Writing the
  expectation first keeps a run from being read in whichever way suits it;
  a result that misses it still follows the release policy. The idea came
  from #1255.

## Running it fast

The full release benchmark is two runs (zero-LLM FTS and local embeddings)
over all 470 scored questions. Profiling the ingest path in October 2026
found most wall clock went to wiki git checkpointing, which retrieval
never reads: every SessionEnd runs `commit_all`, each commit walks a wiki
tree and object database that keep growing across the run, and the
per-event `log.md` append queues behind the same commit mutex. On a
100-question prefix those two phases were 55% and 38% of server-side
ingest time. The two changes below follow from that, and neither touches
a published number.

### Skip wiki checkpoints (release runs)

`AI_MEMORY_EVAL_SKIP_WIKI_CHECKPOINTS=1`, passed to the eval server via
the harness's general `--server-env` escape, makes the server skip the
SessionEnd wiki auto-commits. Retrieval never reads git history (the
markdown files and the SQLite index are the source of truth), so
accuracy, query latency and context tokens are unchanged. Verified on the
16- and 100-question prefixes: every hit@k and recall@k matched a default
run to the last digit, and the 100-question FTS wall clock fell from
155.9 s to 12.1 s. The variable appears in the report's config line
(`env:AI_MEMORY_EVAL_SKIP_WIKI_CHECKPOINTS=1`), so a run that used it
says so. Accuracy, latency and context-token numbers stay comparable with
earlier releases; only ingest wall clock differs, because runs before
October 2026 paid for the commits. The variable is an eval-only escape, not
a supported product configuration: no product path sets it, and its only
reader is the server's startup check.

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

### Run the two modes back to back, not at the same time

Two harness processes are fully independent (separate server, temp data
dir and run dir), and concurrent runs score identically to serial ones:
a concurrent and a serial 100-question pair showed zero ranking
differences. But with checkpoints skipped the FTS run takes minutes,
while the local run saturates the CPU with embedding work. Sharing the
machine slowed the local leg by 20–45% in repeated trials on the
32-thread benchmark box, so run FTS first, then local. Concurrent
execution only pays on hardware with real core headroom.

### Iteration runs (never the release comparison)

`--sample N` (first N questions, one slice), `--stratify N` (first N per
question type, every slice) and `--question-id a,b,c` (chosen questions)
exist for iteration and smoke runs; `evals/README.md` documents each. A
working loop is `--stratify 5` (30 scored questions, every slice) for
direction, then the failing questions by id. Release comparisons stay
full-470 per the release policy above; a subset number never lands in
`docs/benchmarks/`.

### What to expect

On the benchmark hardware (Ryzen 9 7950X3D, 32 threads), with checkpoints
skipped: the FTS run finishes in a couple of minutes (~5,400 items/s
sustained on a 282-question prefix); the local-embeddings run is bounded
by the embedder at roughly 19 pages/s and lands near half an hour. Expect
about ±15% of run-to-run spread on wall clock when the machine has other
work on it; accuracy noise is the ±0.005 band above, not wall clock.

## Local builder measurements

[Draft query-answer context admission](query-answer-context-draft.md) records
synthetic prompt sizes and local debug timings for #1684. It does not measure
retrieval quality, answer accuracy or provider token usage.
