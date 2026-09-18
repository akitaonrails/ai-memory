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

> Full-dataset (500-question) A/B baselines go here, one section per
> comparison, pasted verbatim from `evals/runs/<stamp>-retrieval/report.md`
> with its provenance header. **Not yet populated** — running the full
> matrix is a deliberate, separately-scheduled pass (the dataset is 278 MB
> and a full local-embeddings leg is minutes, not seconds). Do not paste
> `--sample` numbers here as if they were baselines.

### Illustrative smoke (NOT a baseline)

Recorded only to show the report shape. `--sample 4`, four
`single-session-user` questions, so the accuracy figures are noise;
latency and context-tokens show the expected direction (local embeddings
add query latency):

| metric | baseline (none) | candidate (local) | delta |
|---|---|---|---|
| hit@5 | 0.750 | 0.500 | -0.250 |
| latency p50 ms | 2 | 31 | +29 |
| latency p95 ms | 2 | 35 | +33 |
| ctx tok mean | 408.5 | 393.2 | -15.2 |
| ctx tok median | 494.5 | 389.0 | -105.5 |

Hardware: AMD Ryzen 9 7950X3D (32 threads). Replace this section with a
full-dataset run before citing any A/B number.
