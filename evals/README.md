# `evals/` — evaluation harnesses

One Rust binary (`ai-memory-eval`) with four subcommands:

- **`ab`** — runs the EXACT consolidation prompt ai-memory uses in
  production against two LLM providers side by side, saves both
  outputs to disk for human comparison, and scores literal fidelity
  (see [Literal fidelity](#literal-fidelity)).
- **`projection`** — the zero-LLM half of that scoring: which fixture
  facts the observation projection lets into the prompt at all. No
  provider, no network.
- **`retrieval`** — LongMemEval and bundled coding-session retrieval checks,
  driven through a real `ai-memory serve` subprocess. Published baselines
  live in `docs/benchmarks/`.
- **`abstention`** — offline evidence-sufficiency signals and threshold sweeps
  over versioned answerable, hard-negative, conflicting, partial, and
  missing-channel fixtures, with optional structured LLM answerability.

**This is not part of the shipped binary.** It's a workspace member
purely so it shares deps + builds with the rest. `cargo build` from
the repo root will compile it, but it's never bundled into the
docker image. CI builds it and runs its unit tests
(`cargo nextest run --workspace --all-targets`); the `ab`, `projection`,
`retrieval`, and `abstention` commands themselves never run there.

## `abstention` — evidence sufficiency

The default run is deterministic, offline, and keyless:

```bash
cargo run -p ai-memory-eval -- abstention
```

It reads `evals/fixtures/abstention/v1.json`, validates the version, unique
ids, evidence paths, finite score/belief ranges, and required support for
answerable cases, then writes `report.json` and `report.md` under a timestamped
`evals/runs/*-abstention/` directory. Each case records raw normalized lexical
coverage, top-score margin/distribution, matching-term density, snippet size,
and belief aggregation only where the fixture actually supplies belief.

The report sweeps lexical-coverage thresholds from 0.0 through 1.0 and records
coverage, false-supported rate (FSR), and false-abstention rate (FAR), including
undefined denominators. It does not designate a calibrated threshold: the
clean-room corpus is a regression probe, not a representative sample.
`answerable` maps to supported and `unanswerable` to unsupported;
`conflicting` and `partial` remain separate slices and are excluded from both
binary error-rate denominators.

The fixture complements rather than relabels LongMemEval. To add the pinned
LongMemEval-S `*_abs` questions as unanswerable cases, first fetch that dataset
through the retrieval command, then opt in without any network access:

```bash
cargo run -p ai-memory-eval -- retrieval --fetch --sample 1
cargo run -p ai-memory-eval -- abstention --longmemeval-abs
```

Those cases use the evaluator's existing whole-session grep baseline, not the
production server: each question receives at most the top 10 term-overlap
sessions, with normalized overlap scores and each rendered session clipped to
4,000 characters. This keeps optional QA inputs bounded and makes the signal's
provenance explicit; it does not claim to measure production retrieval.

Optional `--qa` uses the exact `--qa-*` provider/auth construction shared with
the retrieval evaluator. Synthesis returns a structured `answerability` enum
(`answerable`, `insufficient`, `conflicting`, or `partial`); the evaluator never
detects abstention from answer wording. Missing credentials produce a reported
skip and leave the offline signal report intact. The retrieval QA path also
parses production's `answer: { text, citations }` object; empty validated
citations are an abstention, while nonempty citations are answerable for this
evaluation metadata. This is still identity evidence, not a claim-level
support proof.

When adding cases, keep them clean-room and synthetic. `target_evidence` must
name supplied paths, answerable cases require a `gold_answer`, and channel-loss
cases declare `channels.fts` / `channels.vector` explicitly. Do not tune a
threshold and the fixture against each other, and do not fold conflicting or
partial evidence into binary ground truth without changing the documented
mapping and report schema.

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
5. Scores each output against the fixture's `must_mention` and
   `must_not_mention` lists and writes `report.json`.

The runner prints latency, parse status and the fidelity numbers per
fixture and run, and a tail summary. Structure and judgement are
still **for you to read**: open the markdown files in
`runs/<timestamp>/baseline/` and `…/candidate/` side by side.

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
│   └── 01-rust-bug-fix.meta.json   ← {elapsed_ms, parsed_ok, update_count, fidelity}
├── candidate/
│   └── …
├── requests/01-rust-bug-fix.txt    ← the exact system prompt + user message sent
├── generated-session.json          ← with --generated-seed
└── report.json                     ← provenance + every run of both sides
```

With `--runs N` the per-case files carry a `.rN` suffix. Eyeball the
`.md` files in pairs. The runner also prints a
`diff -ru baseline candidate` command you can run.

## Literal fidelity

Consolidation can lose a fact at two stages, and the fix differs:

- **projection**: the observation projection omits whole observations
  and cuts bodies at 3,000 characters to fit the budget. A fact it
  drops never reaches the model, so no prompt wording can bring it
  back.
- **model**: the fact was in the prompt and the page does not carry it.

Each fixture may list `must_mention` (literal strings the pages should
carry) and `must_not_mention` (strings no page may carry: injected
instructions, credential-shaped values). Matching is exact,
case-sensitive substring, so a paraphrase counts as a loss; that is
what "verbatim" means in the prompts. The scores, per fixture and run:

| Metric | Meaning |
|---|---|
| `projection_loss` | share of `must_mention` absent from the prompt (zero-LLM) |
| `llm_loss` | share of the prompt-reaching `must_mention` absent from the pages |
| `error_line_observation_coverage` | share of observations carrying a mined error line whose error line reaches the request |
| `file_error_line_share` | share of request-reaching error lines that came from file-shaped tool output |
| `recall_prompt` / `recall_page` | keep-token recall (`mine_keep_tokens`) of the prompt and the pages; a broad secondary view |
| `hallucinated_tokens` | keep-tokens on a page found in neither the request nor the batch's own paths |
| `invented_wikilinks` | `[[links]]` to pages outside the batch; the prompt lists no existing pages, so each one is invented (#1207) |
| `must_not_mention_hits` | forbidden strings found on a page |

The `must_mention` lists are the primary measure because they do not
depend on any token miner. A change that adds miner tokens to the
prompt raises `recall_prompt` by construction, so it cannot be judged
by that number.

`--runs N` (up to 20) repeats every side. The spread between a side's
own runs, printed as `mean (min-max)`, is the noise band, and a delta
inside it is noise. Run the same model on both sides to measure
the band on its own.

`--generated-seed 1215` adds the seeded long session from
`src/generate.rs`: 300 observations in production capture shapes with
fifteen planted facts. Seven sit where the default budget keeps them;
eight sit where the projection drops them (early in the session, in
output with no high-signal term, or past the excerpt cap of a pasted
prompt). Where a fact sits is a choice, so this session's
`projection_loss` describes the fixture, not real sessions; what it
supports is a before/after comparison. `report.json` records the seed
and the sha256 of the generated JSON next to the numbers; a test pins the
sha256 of seed 1215, so a generator change that alters it has to update the
pin and say so beside any republished number.

Two differences from a live server: fixtures and the generated session reach
the projection without passing through the capture sanitizer (they are
synthetic, and the injection fixture keeps its planted strings on purpose),
and they are projected at the harness's 300,000-character default budget,
not the server's 240,000-character one (100,000 tokens × 3 characters ×
the 0.8 safety margin). Compare runs against each other, not against a
server's prompts.

Zero-LLM, no provider:

```bash
cargo run -p ai-memory-eval -- projection --generated-seed 1215
# compare the default projection with the opt-in residue, without a provider:
cargo run -p ai-memory-eval -- projection --generated-seed 1215 --residue
# also write every request and the generated session to a directory:
cargo run -p ai-memory-eval -- projection --generated-seed 1215 --dump evals/runs/projection
```

Point `--dump` and the fixtures at synthetic sessions only. A request
holds every observation verbatim, and real sessions do not belong in
files you may share.

For the model half, `ab --residue` keeps the baseline request at the
byte-compatible default and enables the residue only for the candidate. Use
the same provider and model on both sides and `--runs 3`; the within-side
spread is the noise band. The JSON report records the candidate projection
separately and sets `verbatim_residue_ab` so published results cannot be
mistaken for a provider comparison.

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
  ],
  "must_mention": ["error[E0382]: borrow of moved value"],
  "must_not_mention": ["IGNORE ALL PREVIOUS RULES"]
}
```

Both lists are optional.

`kind` values: `session-start`, `user-prompt`, `pre-tool-use`,
`post-tool-use`, `pre-compact`, `notification`, `stop`,
`session-end`, `other` (see `ObservationKind` in `ai-memory-core`).

The `description` field isn't read by the runner — it's a comment
for the next human who opens the file.

## What this harness does NOT do

- **Judge quality.** The literal checks above say whether a string
  survived, not whether the page is any good. Structure, scoping and
  judgement are still read by a human; an LLM-as-judge pass is
  deliberately out of scope.
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
   the top k), `recall@k` (fraction of evidence sessions found), and
   `recall-all@k` (all evidence sessions found). Duplicate session hits
   count once. The 30 abstention questions are excluded and reported
   separately.

The dataset (278 MB) is NOT committed; it downloads to
`evals/datasets/` (gitignored) with a pinned sha256 that fails loudly
on upstream drift.

```bash
cargo build --release -p ai-memory-cli   # the server under test
cargo run --release -p ai-memory-eval -- retrieval --fetch   # full 500
cargo run -p ai-memory-eval -- retrieval --sample 10         # smoke
cargo run -p ai-memory-eval -- retrieval --question-id 07741c44,07741c45  # chosen questions
```

Output: a per-slice table on stdout plus
`evals/runs/<timestamp>-retrieval/report.{json,md}` with full
provenance (commit, dataset sha, hardware, per-question scores).
When publishing a baseline, copy the markdown into `docs/benchmarks/`.

### Bundled coding-session retrieval corpus

`--dataset-variant coding-v1` selects four synthetic questions in
`evals/fixtures/retrieval/coding-v1.json`. The file is committed and pinned by
SHA-256, so a zero-LLM run needs neither a dataset download nor a provider.
Use the existing server or grep engine and A/B reporting:

```bash
cargo build -p ai-memory-cli -p ai-memory-eval
cargo run -p ai-memory-eval -- retrieval --dataset-variant coding-v1 \
    --server-bin target/debug/ai-memory --candidate --concurrency 1
cargo run -p ai-memory-eval -- retrieval --dataset-variant coding-v1 --engine grep
```

| Case | Expected evidence | Incorrect alternative |
|---|---|---|
| `coding-architecture` | The write-pool failure and the later actor/read-pool decision | Unrelated implementation sessions |
| `coding-resolved-error` | The observed database error and the retry-path fix | Unrelated operational sessions |
| `coding-correction` | The later 45-second timeout correction | The earlier 15-second proposal |
| `coding-long-session` | The exact checksum header at the end of a 41-turn investigation | The unsuccessful initial header |

Each question has twelve sessions, including distractors, and one or two
expected evidence sessions. `answer_session_ids` drives the existing scorer;
`has_answer` marks the relevant turns. `incorrect_session_ids` is corpus
metadata checked by the fixture test, not an extra scoring rule. It partitions
the remaining sessions and makes stale alternatives explicit for review.
The gold answer is available to the optional QA mode, which still needs a
separately configured provider.

These checks measure evidence retrieval: hit@k, recall@k, recall-all@k,
context-token estimates, and query latency. Retrieving the correction does not
prove that an agent follows it, and a stale hit is not a measured answer error.
The corpus does not exercise `as_of`, typed supersession, handoff claiming,
actual harness switching, private/shared authorization, or concurrent writes.
The existing integration suites cover those contracts. Every replay still uses
the evaluator's Claude Code hook shape and one project per question.

Run identical baseline/candidate settings to check accuracy and context-token
parity; wall-clock latency can vary. Treat the four cases as regression probes,
not representative quality estimates or numbers comparable to LongMemEval.
If a fixture changes, update its pin and record the new corpus version beside
any reported results. `--fetch` cannot download a missing bundled fixture;
restore it from the repository instead.

### coding-v1 baseline receipt (2026-10-25 UTC)

Measured with debug evaluator and server binaries from
`e5ac18d8a819a76990d079ee70329973dd1a2e55`, based on `release/3.3` at
`e7e17555efb6a95596480249598ad517ff587310`. The change adds evaluation inputs;
production code is unchanged. Both A/B sides used that same source and
configuration. The published source commit
`7e9ea65b15215d50cb1593956fb57e45954d5347` has the same Git tree
(`5b896e1bbe3550341f25f64529656b919ee16970`) as that locally built commit.
This is a repeatability baseline, not a measured improvement.

- Corpus SHA-256: `af3382663a3e4e969c8bcfa744b3a794de86cb5932393a88bea16f014193d467`.
- Server binary SHA-256: `2f501060445ff2bae166117b8143622c9f3bee606a8aa4ce34911ba7c827f84a`.
- Evaluator binary SHA-256: `498ee8917655b71438111afc30936a97bb213f8c722fb8dd5635efd16dfed29f`.
- Rust `1.95.0 (59807616e 2026-04-14)`; Linux `6.18.55`, x86_64;
  AMD EPYC 9V74, eight available threads.
- Three A/B trials, four questions and 48 sessions per side, concurrency one.
  Default hook ingestion and per-turn dates; embeddings and reranker off;
  no LLM provider or QA calls.

| Metric | Baseline | Identical candidate | Delta |
|---|---:|---:|---:|
| hit@1 / hit@5 / hit@10 | 1.000 | 1.000 | 0.000 |
| recall@1 | 0.750 | 0.750 | 0.000 |
| recall-all@1 | 0.500 | 0.500 | 0.000 |
| recall@5 / recall@10 | 1.000 | 1.000 | 0.000 |
| recall-all@5 / recall-all@10 | 1.000 | 1.000 | 0.000 |
| Estimated context tokens, mean | 225.0 | 225.0 | 0.0 |

Every question's retrieval scores and context estimate matched across all six
runs. Context tokens use the existing Unicode-character-count divided by four
estimator over returned titles and snippets; no provider token count was taken.

| Trial | Baseline p50 / p95 (ms) | Candidate p50 / p95 (ms) |
|---|---:|---:|
| 1 | 8 / 8 | 7 / 8 |
| 2 | 10 / 11 | 8 / 25 |
| 3 | 7 / 9 | 8 / 12 |

Latency covers the MCP round trip. Each percentile uses only four questions,
and unrelated compilation ran on the same host, so these timings are descriptive
and do not establish a performance difference. A separate offline grep run on
the same fixture found the same retrieval scores. Its zero latency/context
fields are evaluator placeholders and are not server measurements.

Reproduce with the `coding-v1 --candidate --concurrency 1` command above, three
times. The ignored `coding_corpus_replays_offline_with_identical_ab_results`
smoke test also asserts full evidence recall@10 and identical A/B scores/context;
it needs a server binary at `target/release/ai-memory`.

### The triple: accuracy + latency + context-tokens

Every run — single-config or A/B — reports three things per slice, not
just accuracy:

- **accuracy** — `hit@k` / `recall@k` / `recall-all@k`;
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

### `retrieval-compare` — two saved runs, question by question

An A/B run compares two configs of one build in one process. A release
comparison pairs runs of two builds made on different days, and its slice
averages say how much moved, not which questions moved or how.
`retrieval-compare` reads the `report.json` of two single-config runs (the
run directory or the file), pairs questions by id, and prints:

- hit@k and recall@k over the questions both runs scored, with the
  questions gained and lost at each k;
- every question whose first hit moved: the cutoffs it crossed, its recall
  at the largest k and its latency on each side, widest moves and losses
  first;
- per question type, how many moved earlier and how many later.

```bash
cargo run --release -p ai-memory-eval -- retrieval-compare \
    --baseline evals/runs/<previous release local run> \
    --candidate evals/runs/<release candidate local run> \
    --fts evals/runs/<release candidate FTS run>
```

Moves of one cutoff in both directions, spread across types, are what
questions near a cutoff trading places look like. Moves that skip cutoffs,
all go one way, or cluster in one type point at a ranking change (#1854).
`--fts` takes the candidate build's FTS run and marks moved questions whose
candidate row equals the FTS row in every hit, recall, retrieved-session
and context-token value. With a latency at or above the 250 ms query
embedding wait, that is a local query that ran without its vector. An A/B
`report.json` is refused; compare the single-config runs instead.

Strict `recall-all@k` scores each question as 1 only when every evidence
session appears in the top k distinct retrieved sessions, otherwise 0.
Slice scores average those per-question values. Finding one of two required
sessions gives `hit@k = 1`, `recall@k = 0.5`, and `recall-all@k = 0`.
JSON reports use `recall_all_at` for per-question, slice and A/B delta values.

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

### `retrieval` decomposition instruments

These flags split a hit@k number into evidence the index never saw and
ranking on evidence it did. All are opt-in; the defaults reproduce the
published baselines. The results are in
[`docs/benchmarks/longmemeval-s-decomposition-2026-10-12.md`](../docs/benchmarks/longmemeval-s-decomposition-2026-10-12.md).
On a 3.0 server the observation stream is on by default, so reproducing a
2.6-era baseline also needs
`--server-env AI_MEMORY_RETRIEVAL__OBSERVATION_STREAM=false`.

- `--reachability` reads every evidence session back after the query and
  reports, per slice, how many answer-bearing turns (the dataset's
  `has_answer` flag) survive into the session page, the observation bodies,
  and the first 512 tokens the local embedder reads. A turn counts as
  reachable when at least half of its distinctive tokens (content words that
  occur in at most 3 sessions of the haystack) are on that surface. The
  module docs in `src/retrieval/reachability.rs` give the exact definition.
  It needs the local model's tokenizer, which it fetches into
  `evals/models/` if missing.
- `--engine grep` ranks each question's raw sessions by query-term overlap
  in-process, with no server. It follows agentmemory's grep adapter exactly
  and matches its archived per-question results.
- `--ingest whole-session` writes each raw session as one page through
  `POST /admin/write-page` and then queries as usual: ranking on full
  evidence. `--ingest captured-session` writes one page per session holding
  only the text capture keeps (user turns up to 16 KiB, assistant excerpts up
  to 2 KB).
- `--date-prefix turn|session-start|none` moves the session date off every
  turn (`turn` is the default).
- `--stratify N` scores the first N questions of each type, so a cheap run
  covers every slice (`--sample` takes a prefix, which is all one type).
- `--dataset-variant s-cleaned` runs on the cleaned LongMemEval-S
  re-release (pinned by sha256) that other systems report on.

`--candidate-ingest` and `--candidate-date-prefix` put either knob into an
A/B run. The A/B delta table also counts per-question flips (`+gained/-lost`),
which shows whether a net delta is many changes cancelling out or a few in
one direction.

```bash
cargo run --release -p ai-memory-eval -- retrieval --engine grep
cargo run --release -p ai-memory-eval -- retrieval --reachability \
    --ingest hooks --candidate-ingest whole-session
cargo run --release -p ai-memory-eval -- retrieval --stratify 5 \
    --date-prefix turn --candidate-date-prefix session-start
```

Honest-numbers notes:

- capture is production-shaped: excerpts are bounded at the 2 KB
  privacy boundary. The decomposition measured what that costs: pages
  holding exactly the captured text score 0.968 FTS `hit@5` against
  0.972 for whole raw sessions. The larger cost was the zero-LLM session
  page, which indexed only 80-character turn titles; the observation
  stream (on by default in 3.0) puts the observation bodies back into
  ranking;
- each session's original date is prepended to its turn text, since a
  replayed history must carry its own timestamps (the official
  harness exposes the same information to retrievers). The prefix is 39
  characters, about half of a turn title, and production capture adds
  none, so the default `--date-prefix turn` understates the pipeline on
  this content (+0.133 FTS `hit@5` on a 120-question sample with the date
  moved to session start). It stays the default so earlier baselines
  remain reproducible;
- other systems' published numbers on this dataset (e.g. agentmemory's
  95.2% `recall_any@5`, mcp-memory-service 0.804 R@5) use the same
  statistic as our `hit@5`. agentmemory's indexes whole raw sessions
  directly, a component measurement rather than a pipeline one, so it is
  not like-for-like with the default `--ingest hooks` numbers. Its
  0.967 is a different benchmark (the grep baseline on its 15-query
  coding-agent-life-v1 corpus), not LongMemEval-S.
