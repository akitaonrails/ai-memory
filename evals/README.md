# `evals/` — evaluation harnesses

One Rust binary (`ai-memory-eval`) with two subcommands:

- **`ab`** — runs the EXACT consolidation prompt ai-memory uses in
  production against two LLM providers side by side, and saves both
  outputs to disk for human comparison.
- **`retrieval`** — the LongMemEval retrieval benchmark, driven end to
  end through a real `ai-memory serve` subprocess. Published baselines
  live in `docs/benchmarks/`.

**This is not part of the shipped binary.** It's a workspace member
purely so it shares deps + builds with the rest. `cargo build` from
the repo root will compile it, but it's never bundled into the
docker image and never run by CI.

## When to reach for this

After switching providers, models, or major prompt edits. Concretely:

- Replacing OpenRouter/Kimi with local Ollama (the case that
  motivated this harness — see commit log).
- Trying a new Ollama model (`qwen3:32b` → `qwen3-coder:30b`).
- Tuning the `BATCH_SYSTEM_PROMPT` itself — does the rewrite
  preserve quality across providers?

The fixtures are deliberately small synthetic session logs that
exercise the prompt's hard cases (durable rule extraction,
multi-topic separation, "say nothing" sessions, decision/gotcha
distinction).

## What it does

For each `*.json` under `evals/fixtures/`:

1. Builds the request via
   [`ai_memory_consolidate::build_batch_request`] — same code path
   the live consolidator uses.
2. Sends it to a **baseline** and a **candidate** provider, *in
   parallel*.
3. Runs the result through
   [`ai_memory_llm::complete_structured`] — same JSON-schema
   validation the live system applies. Schema-parse failure is
   recorded (not fatal).
4. Persists to `evals/runs/<timestamp>/{baseline,candidate}/<fixture>.{json,md,meta.json}`.

The runner prints latency + parse status per fixture and a tail
summary. **Quality is for you to read** — open the markdown files
in `runs/<timestamp>/baseline/` and `…/candidate/` side by side and
judge faithfulness, scoping, hallucination, etc.

## Running it

### Ollama qwen3:32b vs OpenRouter Kimi (the canonical comparison)

```bash
# OPENROUTER_API_KEY in env; LLM_API_KEY can be any non-empty
# string for the candidate (Ollama doesn't validate).
export OPENROUTER_API_KEY="sk-or-v1-..."

cargo run -p ai-memory-eval -- ab \
    --baseline-provider openai-compat \
    --baseline-base-url https://openrouter.ai/api/v1 \
    --baseline-model moonshotai/kimi-k2.6 \
    --baseline-api-key-env OPENROUTER_API_KEY \
    --candidate-provider openai-compat \
    --candidate-base-url http://192.168.0.90:11434/v1 \
    --candidate-model qwen3:32b \
    --candidate-api-key ollama-local
```

### Two Ollama models against each other

```bash
cargo run -p ai-memory-eval -- ab \
    --baseline-provider openai-compat \
    --baseline-base-url http://192.168.0.90:11434/v1 \
    --baseline-model qwen3:32b \
    --baseline-api-key ollama-local \
    --candidate-provider openai-compat \
    --candidate-base-url http://192.168.0.90:11434/v1 \
    --candidate-model qwen3-coder:30b \
    --candidate-api-key ollama-local
```

### ChatGPT/Codex OAuth as one side

Run `ai-memory auth login openai-oauth` first, then point the eval harness at
the same token file:

```bash
cargo run -p ai-memory-eval -- ab \
    --baseline-provider openai-oauth \
    --baseline-token-file ~/.local/share/ai-memory/auth.json \
    --baseline-model gpt-5.5 \
    --candidate-provider openai-compat \
    --candidate-base-url http://192.168.0.90:11434/v1 \
    --candidate-model qwen3:32b \
    --candidate-api-key ollama-local
```

### GitHub Copilot as one side

Run `ai-memory auth login copilot` first, then point the eval harness at the
same auth file:

```bash
cargo run -p ai-memory-eval -- ab \
    --baseline-provider copilot \
    --baseline-token-file ~/.local/share/ai-memory/auth.json \
    --baseline-model gpt-5.5 \
    --candidate-provider openai-compat \
    --candidate-base-url http://192.168.0.90:11434/v1 \
    --candidate-model qwen3:32b \
    --candidate-api-key ollama-local
```

### Reading the output

```
evals/runs/2026-05-22T18-30-00Z/
├── baseline/
│   ├── 01-rust-bug-fix.json        ← raw structured output
│   ├── 01-rust-bug-fix.md          ← flat markdown rendering, easy to read
│   └── 01-rust-bug-fix.meta.json   ← {elapsed_ms, parsed_ok, update_count}
└── candidate/
    └── …
```

Eyeball the `.md` files in pairs. The runner also prints a
`diff -ru baseline candidate` command you can run.

## Adding fixtures

Each fixture is a JSON file:

```json
{
  "name": "human-readable",
  "description": "what this case is meant to surface",
  "observations": [
    {"kind": "session-start", "title": "...", "body": "..."},
    {"kind": "user-prompt",   "title": "user prompt", "body": "..."},
    {"kind": "pre-tool-use",  "title": "Edit", "body": "..."},
    ...
  ]
}
```

`kind` values: `session-start`, `user-prompt`, `pre-tool-use`,
`post-tool-use`, `pre-compact`, `notification`, `stop`,
`session-end`, `other` (see `ObservationKind` in `ai-memory-core`).

The `description` field isn't read by the runner — it's a comment
for the next human who opens the file.

## What this harness does NOT do

- **Score quality automatically.** Pure side-by-side. If you want
  metrics, the next layer up would be keyword recall (`must_mention`
  per fixture) or an LLM-as-judge pass — both deliberately out of
  scope here.
- **Test the embedding pipeline.** This only exercises the
  consolidation LLM. Embedding A/B would be a parallel harness
  (probe queries + expected target pages, measure recall@5/MRR).
- **Persist via the real wiki layer.** No SQLite, no markdown
  writes, no git. Pure prompt → response.

## Cleanup

`evals/runs/` is in `.gitignore`. Drop it whenever it gets large.

## `retrieval` — LongMemEval benchmark

Measures the real retrieval stack against
[LongMemEval](https://huggingface.co/datasets/xiaowu0162/longmemeval)
(v1, S variant, MIT license, 500 questions over ~50-session chat
haystacks). Nothing is mocked:

1. a real `ai-memory serve` subprocess starts on a fresh temp data dir
   (zero-LLM: no consolidation LLM, no embedder, no reranker — fully
   deterministic and offline);
2. each question's haystack replays through `POST /hook/batch` at the
   production hook cadence (`session-start`, `user-prompt-submit` per
   user turn, `stop` with the opt-in assistant excerpt per assistant
   turn, `session-end`), one project per question;
3. the question runs through MCP `tools/call memory_query` with
   explicit workspace/project scoping;
4. results are scored session-level: `hit@k` (any evidence session in
   the top k — the "Recall@k" most systems publish) and `recall@k`
   (fraction of evidence sessions found). The 30 abstention questions
   are excluded and reported separately.

The dataset (278 MB) is NOT committed; it downloads to
`evals/datasets/` (gitignored) with a pinned sha256 that fails loudly
on upstream drift.

```bash
cargo build --release -p ai-memory-cli   # the server under test
cargo run --release -p ai-memory-eval -- retrieval --fetch   # full 500
cargo run -p ai-memory-eval -- retrieval --sample 10         # smoke
```

Output: a per-slice table on stdout plus
`evals/runs/<timestamp>-retrieval/report.{json,md}` with full
provenance (commit, dataset sha, hardware, per-question scores).
When publishing a baseline, copy the markdown into `docs/benchmarks/`.

### The triple: accuracy + latency + context-tokens

Every run — single-config or A/B — reports three things per slice, not
just accuracy:

- **accuracy** — `hit@k` / `recall@k` (unchanged);
- **latency** — `memory_query` MCP round-trip **p50 / p95** in ms;
- **context tokens** — the context an agent would ingest from the result,
  estimated as **chars / 4** over each returned hit's `title` + `snippet`
  (a documented heuristic, not a real tokenizer), reported **mean / median**.

### `retrieval` A/B mode (R2)

Pass a **candidate** config and the same question set runs through two
fresh servers (baseline + candidate) and gains a baseline→candidate delta
table (markdown + JSON). A/B mode turns on with `--candidate` or any
`--candidate-*` knob; without one, the single-config path is unchanged.

Baseline knobs: `--embeddings none|local` (also = vectors off/on),
`--reranker` (sets `AI_MEMORY_RERANKER=llm`; needs a provider via
`--server-env`), `--server-env KEY=VALUE` (any server env, repeatable),
`--query-arg KEY=JSON` (extra `memory_query` args, repeatable — so future
knobs like `pin_first` / `include_superseded` can be A/B'd;
`query`/`workspace`/`project`/`limit` are reserved). Each has a
`--candidate-*` twin (`--candidate-embeddings`, `--candidate-reranker`,
`--candidate-server-env`, `--candidate-query-arg`); a candidate knob left
unset mirrors the baseline.

```bash
# Determinism check — baseline == candidate ⇒ accuracy/context delta = 0:
cargo run --release -p ai-memory-eval -- retrieval --candidate --sample 2

# Vectors off vs on, full run:
cargo run --release -p ai-memory-eval -- retrieval --fetch \
    --candidate-embeddings local

# A future memory_query knob, baseline off vs candidate on:
cargo run --release -p ai-memory-eval -- retrieval --fetch \
    --candidate-query-arg include_expired=true
```

Output: `evals/runs/<timestamp>-retrieval/report.{json,md}` — the JSON is
`{ baseline, candidate, delta }`; the markdown carries both per-slice
triples and an `overall` delta table. Publish full-dataset A/B numbers in
`docs/benchmarks/retrieval-ab-r2.md`.

### `retrieval` QA-accuracy mode (R2b, opt-in, live LLM)

R2a scores *retrieval*. `--qa` adds an opt-in **end-to-end answer-quality**
metric so the `answer`/`reasoning` MCP features can be proven later. It is
**OFF by default and makes REAL LLM API calls**, gated behind a key. Per
scored question, with QA on, the harness:

1. gets a candidate answer — the server's own `answer` field when a future
   `answer=true` feature populates it, otherwise a **synthesis** LLM call
   over the retrieved snippets (the exact context an agent would receive);
2. **grades** that candidate against the dataset's gold answer with an
   LLM-as-judge (`{correct, reason}`), LongMemEval-style;
3. reports **QA accuracy** (fraction correct) per slice — plus the
   answer-step latency (p50/p95) and answer-token estimate — alongside the
   R2a triple, in both the single-config and A/B reports. Provider+model
   names are recorded; **keys never are**.

Flags: `--qa`, `--qa-provider <name>`, `--qa-model <model>` (required to
enable), `--qa-base-url`, `--qa-api-key`/`--qa-api-key-env` (defaults to the
provider's canonical env var, e.g. `GEMINI_API_KEY`), `--qa-token-file`
(oauth/codex/copilot). The grader defaults to the answerer and is overridable
per field: `--qa-grader-provider`/`-model`/`-base-url`/`-api-key`/
`-api-key-env`/`-token-file`.

**Gating:** if `--qa` is set but the provider/key cannot be resolved, the run
prints a SKIP line and continues with QA off (retrieval metrics unaffected),
exiting success — so a keyless CI stays green. The default non-QA path stays
zero-LLM and deterministic.

```bash
# Keep keys in the process env only for the run; never echo a key.
set -a; source ~/.config/zsh/secrets; set +a   # or export GEMINI_API_KEY=...
cargo run -p ai-memory-eval -- retrieval --sample 3 \
    --qa --qa-provider gemini --qa-model gemini-2.5-flash
```

Honest-numbers notes:

- capture is production-shaped: excerpts are bounded at the 2 KB
  privacy boundary, so evidence buried deep inside one long turn is
  genuinely out of reach — that is the system being measured, not a
  harness artifact;
- each session's original date is prepended to its turn text, since a
  replayed history must carry its own timestamps (the official
  harness exposes the same information to retrievers);
- competitors' published numbers (e.g. agentmemory 0.967 R@5,
  mcp-memory-service 0.804 R@5) are answer-evidence Recall@5 on this
  same v1 dataset, comparable to our `hit@5`.
