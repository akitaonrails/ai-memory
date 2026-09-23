# Retrieval A/B (R2) — accuracy + latency + context-tokens triple

The `retrieval` harness can compare two server/query configurations over
the **same** LongMemEval-S question set in one run, reporting a triple per
config and a baseline→candidate delta:

- **accuracy** — `hit@k` / `recall@k` (as the single-config baselines do);
- **latency** — `memory_query` MCP round-trip p50 / p95, milliseconds;
- **context tokens** — the context an agent would ingest from a result,
  estimated as **chars / 4** over every returned hit's `title` + `snippet`
  (a documented, provider-agnostic heuristic — not a real tokenizer).

Provenance (commit, dataset sha256, hardware, per-config knob summary) is
carried exactly as the single-config reports carry it. See
`evals/README.md` for the full flag reference.

The harness can additionally run **end-to-end QA-accuracy** (`--qa`, #771/#772):
each question's retrieved context is handed to an LLM that answers it, and an
LLM-as-judge grades the answer against the gold label. This reports
answered/graded accuracy, answer latency (p50/p95), and answer-token cost
alongside the retrieval triple. It requires a provider key and is off by
default.

## How to run

```bash
cargo build --release -p ai-memory-cli
# baseline (FTS-only) vs candidate (local embeddings), full 500 questions:
cargo run --release -p ai-memory-eval -- retrieval --fetch \
    --candidate-embeddings local
```

A run without a candidate config is unchanged and still writes the
single-config report. A baseline-vs-baseline run (`--candidate`, no
differing knob) is the determinism check: accuracy and context-token
deltas are exactly zero (latency wobbles at wall-clock noise).

## Published A/B numbers

Full-dataset run, **2.4 release candidate** (470 scored; 30 abstention
questions excluded).

- commit: `79fea4009e74cf83561c6e2170dccef80360c079` (release/2.4)
- dataset: `longmemeval_s` (sha256 `08d8dad4be43ee20…`)
- hardware: AMD Ryzen 9 7950X3D 16-Core (32 threads)
- baseline: `embeddings=none reranker=off` (zero-LLM, FTS + entity + graph)
- candidate: `embeddings=local reranker=off` (in-process all-MiniLM-L6-v2)

**Overall (470 questions):**

| metric | baseline (FTS, zero-LLM) | candidate (local) | delta |
|---|---|---|---|
| hit@1 | 0.532 | 0.536 | +0.004 |
| hit@3 | 0.645 | 0.730 | +0.085 |
| hit@5 | 0.666 | 0.815 | +0.149 |
| hit@10 | 0.694 | 0.891 | +0.198 |
| recall@5 | 0.536 | 0.677 | +0.142 |
| recall@10 | 0.564 | 0.817 | +0.254 |
| latency p50 ms | 5 | 94 | +89 |
| latency p95 ms | 40 | 162 | +122 |
| ctx tok mean | 350.3 | 415.7 | +65.4 |
| ctx tok median | 316.5 | 367.5 | +51.0 |

Local embeddings buy a large recall gain (hit@10 +0.198, recall@10 +0.254)
for ~90 ms added p50 query latency and ~65 more context tokens — the expected
tradeoff, now measured on the full set rather than a smoke.

Per-slice `hit@5` (candidate, local): knowledge-update 0.875, multi-session
0.868, temporal-reasoning 0.780, single-session-assistant 0.821,
single-session-preference 0.767, single-session-user 0.734.

**Determinism check (baseline vs baseline, `--candidate`):** every accuracy
and context-token delta is exactly `0.000` (latency wobbles ±5 ms at wall-clock
noise) — the harness is sound on the 2.4 tree.

**No default-ranking regression on 2.4.** The 2.4 aging/retrieval features are
opt-in / off by default, so default retrieval is unchanged from 2.3.x: overall
FTS `hit@5` 0.668 → 0.666 and local `hit@5` 0.823 → 0.815 versus the
2026-09-01 snapshot (commit `0ac0dcf`) — identical within run-to-run noise.

**Cross-run variance (read the numbers accordingly).** The harness is
deterministic *within* a run (the `--candidate` determinism check is exactly
`0.000`), but *across* independent runs it varies: two full local-embeddings
runs on this same 2.4 commit gave overall `hit@5` 0.815 and 0.821, and per-slice
wobble up to ~0.02–0.03 on the small-n slices in **both directions** (e.g.
knowledge-update 0.875 then 0.903; single-session-user 0.734 then 0.750). Treat
overall `hit@5` as ≈ 0.82 ± 0.005 and don't over-read a single small-n slice
from one run. Both 2.4 runs are statistically identical to the 2.3.x 0.823
baseline — a genuine per-slice regression is a drop that persists across a
confirmation re-run, not a one-run dip.

Reproduce:

```bash
cargo run --release -p ai-memory-eval -- retrieval --candidate-embeddings local
# determinism (deltas must be 0):
cargo run --release -p ai-memory-eval -- retrieval --candidate
```

### Illustrative sample-20 run (triple + QA-accuracy, NOT a baseline)

Recorded to show the triple + QA columns on a real run. **`--sample 20`, 20
LongMemEval-S questions — illustrative, NOT the full-500 baseline.** The
published baseline stays 0.823 hit@5 (local) / 0.668 zero-LLM in
[README.md](README.md); do not cite the numbers below as a baseline.

Retrieval (zero-LLM):

| metric | value |
|---|---|
| hit@1 | 0.750 |
| hit@5 | 0.800 |
| hit@10 | 0.800 |
| recall@5 | 0.800 |
| latency p50 / p95 ms | 2 / 3 |
| ctx tok mean / median | 330.2 / 386.5 |

QA-accuracy (Gemini `gemini-2.5-flash` answered + graded):

| metric | value |
|---|---|
| QA accuracy | 9/20 = 0.450 |
| answer latency p50 / p95 ms | 705 / 862 |
| answer tokens mean | 453.9 |

Reproduce (QA needs `GEMINI_API_KEY`):

```bash
cargo run --release -p ai-memory-eval -- retrieval \
    --sample 20 --qa --qa-provider gemini --qa-model gemini-2.5-flash
```
