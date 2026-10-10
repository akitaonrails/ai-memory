# LLM provider comparison - local Ollama vs hosted OpenRouter

> **TL;DR.** ai-memory's consolidation prompt had a latent
> schema-vs-prompt bug that made every provider fail JSON validation.
> After two rounds of fixes (schema + tightened anti-hallucination
> prompt), six providers were benchmarked on the same 5 fixtures:
>
> | Provider | Parse | Avg latency | Faithfulness | $/M out tokens† | Notes |
> |---|---|---|---|---|---|
> | qwen3:32b (Ollama local) | 5/5 | 92 s | high | **$0** | production default |
> | **GPT-5.4-mini** (OpenRouter) | 5/5 | **4.3 s** | high‡ | ~$1 | fastest + cheapest hosted |
> | Haiku 4.5 (OpenRouter) | 5/5 | 7.3 s | high | ~$5 | most disciplined hosted on restraint |
> | DeepSeek V4 Flash (OpenRouter) | 5/5 | 21.7 s | high | ~$0.40 | slower than GPT-mini, comparable price |
> | Sonnet 4.5 (OpenRouter) | 5/5 | 10.8 s | high (after prompt fix) | ~$15 | displaced by Haiku for this task |
> | Kimi-K2.6 (OpenRouter) | hangs | n/a | n/a | n/a | reasoning model, ineligible |
>
> † order-of-magnitude pricing per million output tokens. Actual
> per-consolidation cost depends on input + output token count.
> ‡ One slip: GPT-5.4-mini manufactured a `decisions/` page for a
> typo-fix session (mild over-classification), where Haiku
> correctly emitted only the session log.
>
> **Recommended default for most users: Claude Haiku 4.5.**
> Hosted (always available), fast (~7 s), cheap enough that
> per-session cost doesn't matter for personal use, and the
> most disciplined hosted model on restraint + classification.
> **Cheaper alternative**: GPT-5.4-mini (~5× cheaper than
> Haiku, ~2× faster, mild over-classification on trivial
> sessions). **Free alternative if you have a local LLM
> server** (Ollama / vLLM / llama-swap with a 30B-class
> model): qwen3:32b on Ollama, at $0 per consolidation, with
> background latency invisible to users. See
> [Installation cookbook - LLM provider tiers](install.md#llm-provider-tiers)
> for setup. Reproduce the comparison in [`evals/`](../evals/).

## Why this document exists

When the homelab deploy switched ai-memory off the billed
OpenAI / OpenRouter providers and onto the locally-hosted Ollama
server, we needed empirical evidence that *consolidation quality
didn't degrade*. ai-memory's
consolidator turns a session's raw observations into 1–5 wiki
pages classified as `concept`, `decision`, `gotcha`, or `rule`;
small drops in quality compound fast across hundreds of sessions.

This doc captures:

- The **methodology** (what we compared, how we compared, the
  exact prompt + schema both providers saw).
- The **root cause** of why early runs looked terrible.
- The **fix** that landed in the consolidator's types + prompt.
- The **final per-provider numbers** (parse rate, latency,
  manual quality assessment).
- A **how-to-reproduce** section so anyone can re-run the
  comparison against their own model + provider choices.

## What was tested

### The five fixtures

[`evals/fixtures/`](../evals/fixtures/) holds five short synthetic
session logs, each crafted to surface a *different* failure mode
in consolidation:

| Fixture | What it stresses |
|---|---|
| `01-rust-bug-fix` | Did the model split a multi-page session into the right slices (session log + concept + decision + gotcha)? |
| `02-architecture-decision` | Can the model produce an ADR-style page distinct from the running session log? |
| `03-gotcha-with-rule` | Did the model correctly classify a durable project rule with `kind: rule` so the consolidator can auto-route it to `_rules/`? |
| `04-low-signal-session` | Does the model *resist* manufacturing concept pages when there's nothing durable to capture? |
| `05-multi-topic-session` | Does the model emit *separate* pages per topic instead of mashing two unrelated topics together? |

Fixtures use real-shape `ObservationKind` values (`session-start`,
`user-prompt`, `pre-tool-use`, `post-tool-use`, `session-end`)
exactly as the production hook ingress emits them.

### The exact request

Per fixture, the runner calls
[`ai_memory_consolidate::build_batch_request(session_id, &observations)`](../crates/ai-memory-consolidate/src/consolidator.rs),
the **same** function the live consolidator uses on every
`memory_consolidate` invocation. That request is then sent
through [`ai_memory_llm::complete_structured`](../crates/ai-memory-llm/src/lib.rs)
(also the live path), so every provider gets an identical request.

### The six providers

| Tag | Provider | Model | Endpoint |
|---|---|---|---|
| **Kimi** | OpenRouter (openai-compat) | `moonshotai/kimi-k2.6` | `https://openrouter.ai/api/v1` |
| **Sonnet** | OpenRouter (openai-compat) | `anthropic/claude-sonnet-4.5` | `https://openrouter.ai/api/v1` |
| **Haiku** | OpenRouter (openai-compat) | `anthropic/claude-haiku-4.5` | `https://openrouter.ai/api/v1` |
| **GPT-mini** | OpenRouter (openai-compat) | `openai/gpt-5.4-mini` | `https://openrouter.ai/api/v1` |
| **DeepSeek** | OpenRouter (openai-compat) | `deepseek/deepseek-v4-flash` | `https://openrouter.ai/api/v1` |
| **qwen3** | Ollama (openai-compat) | `qwen3:32b` (Q4_K_M, ~20 GB) | `http://192.168.0.90:11434/v1` |

The home server (`192.168.0.90`) is a Ryzen AI MAX+ 395
(Strix Halo / gfx1151), 96 GB unified memory, ROCm-backed
Ollama with `OLLAMA_KEEP_ALIVE=20m` + `OLLAMA_FLASH_ATTENTION=1`
+ `OLLAMA_KV_CACHE_TYPE=q8_0`. Once a model is loaded into
unified memory it stays warm for 20 min, so the first
request pays a 30–60 s cold-load tax and subsequent ones are
sub-3 s.

## Run 1 - broken prompts + schema (pre-fix baseline)

Every provider failed schema validation on every fixture:

| Fixture | Kimi | qwen3:32b |
|---|---|---|
| 01-rust-bug-fix | ❌ *response is not valid JSON* | ❌ *integer 1, expected string* |
| 02-architecture-decision | ❌ *response is not valid JSON* | ❌ *integer 2, expected string* |
| 03-gotcha-with-rule | ❌ *response is not valid JSON* | ❌ *integer 1, expected string* |
| 04-low-signal-session | ❌ *response is not valid JSON* | ❌ *integer 1, expected string* |
| 05-multi-topic-session | ❌ *response is not valid JSON* | ❌ *integer 2, expected string* |

The *raw responses*, however, showed that both models did
**excellent** consolidation work content-wise. They
correctly identified multiple distinct pages per fixture,
extracted faithful summaries, and respected the path
conventions. The failures were **format only**:

- **Kimi** was emitting well-formatted markdown
  (`### Update 1` / `**path:**` / `**body:**`) and ignoring
  the request for JSON entirely.
- **qwen3** was emitting clean JSON in code fences, but with
  `tier: 1` / `tier: 2` / `tier: 3` (integers) instead of the
  documented string values, and occasionally with invented
  `kind` values like `"session"` (which isn't in the
  `PageKind` enum).

## The root cause

Two separate problems, both **on our side**:

### Bug A - `Tier` had no `JsonSchema` derive

In `crates/ai-memory-consolidate/src/types.rs`:

```rust
pub struct ConsolidatedPageUpdate {
    pub path: String,
    pub tier: String,   // ← bug: typed as String
    pub kind: PageKind, // ← already an enum with JsonSchema
    ...
}
```

`schemars` couldn't produce an enum constraint for `tier`
because `Tier` (the actual enum in `ai-memory-core`) didn't
have the `JsonSchema` derive. The generated schema field was
just `{ "type": "string" }` with no `enum` constraint, so models
were free to guess. Both Kimi and qwen3 guessed numeric indices.

### Bug B - prompt described values, didn't enforce them

The system prompt in
[`build_batch_request`](../crates/ai-memory-consolidate/src/consolidator.rs)
listed the valid `tier` and `kind` values in prose but never
said "use these EXACT string values, never an integer, never a
synonym, never code fences". Local instruction-tuned models,
especially when there's no `response_format: json_schema`
support to enforce the schema, drift to whatever feels natural.

Compounding this at the time of the run: openai-compat providers
(Ollama, OpenRouter passthrough) were using ai-memory's tolerant
parser path, so the schema was descriptive, not coercive. The
provider opt-in for coercive structured output is documented below.

#### Strict structured output for openai-compat

`openai-compat` sends
`response_format={ type: "json_schema", strict: true }` by default. On a
parse-shape failure (the upstream returned a response but it wasn't a
valid JSON object), or an explicit 400/422 rejection naming
`response_format`, `json_schema`, or structured output, ai-memory falls back to
the tolerant parser. Other HTTP / auth / transport errors propagate without
retry. Set `AI_MEMORY_LLM_COMPAT_STRICT=false` to opt out.

**Cost.** Strict mode is one HTTP call when the upstream honours
`response_format`. When it doesn't, you pay a second call for the
tolerant fallback. Pick by engine:

| Engine class | Setting |
|---|---|
| Modern Ollama / vLLM / LM Studio honouring `response_format=json_schema` | Keep the default (one call, schema-constrained) |
| Reasoning models with `<think>…</think>` inside `content` (DeepSeek-R1, Qwen3-Thinking, MiniMax M2) | Set `AI_MEMORY_LLM_COMPAT_STRICT=false` if the strict-then-fallback double call is frequent |
| Older engines / proxies that reject `response_format` explicitly | Keep the default; ai-memory retries without it |
| Older engines / proxies that mishandle the field without a recognizable rejection | Set `AI_MEMORY_LLM_COMPAT_STRICT=false` |

The prompt still has to do the load-bearing work when strict mode is
off or the strict call falls back.

## The fix

Three small changes landed together:

### 1. Derive `JsonSchema` on `Tier`

`crates/ai-memory-core/src/page.rs`:

```rust
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash,
    Serialize, Deserialize,
    schemars::JsonSchema, // ← new
)]
#[serde(rename_all = "snake_case")]
pub enum Tier { Working, Episodic, Semantic, Procedural }
```

Adds `schemars` as a dep on `ai-memory-core` (acceptable, since
schemars is already a workspace dep used by every type that
crosses the LLM boundary).

### 2. Type the field as `Tier`, not `String`

`crates/ai-memory-consolidate/src/types.rs`:

```rust
pub struct ConsolidatedPageUpdate {
    pub path: String,
    pub tier: Tier,        // ← was String
    pub kind: PageKind,
    ...
}
```

The generated schema now contains
`{ "enum": ["working", "episodic", "semantic", "procedural"] }`
for `tier`. `serde_json::from_value` rejects anything else.

### 3. Tighten the prompt

`build_batch_request` now spells out:

```
Set `tier` to EXACTLY ONE of these four strings — never an integer, never a synonym:
- "working"      (the live in-progress slice of the session — rarely used here)
- "episodic"     (per-session narrative; the sessions/<id>.md page)
- "semantic"     (durable knowledge: concepts/, decisions/, gotchas/, rules)
- "procedural"   (repeated patterns extracted from many episodic pages)

Set `kind` to EXACTLY ONE of these four strings — never an integer, never "session" / "concept" / "note":
- "decision" / "gotcha" / "rule" / "fact"

## Output format (read this carefully)
Reply with ONE JSON object matching the ConsolidatedBatch schema, and nothing else.
NO prose preamble, NO trailing commentary, NO markdown headers wrapping the JSON,
NO ``` code fences. The very first character of your reply must be `{` and the
very last `}`. Strings must be JSON strings (with double quotes), not numbers
and not bare identifiers.
```

The two fixes overlap on purpose: the schema now *rejects* the
bad values, and the prompt makes the model less likely to produce
them in the first place.

## Run 2 - schema + first prompt fix

After the schema fix + first prompt iteration, the same five
fixtures produced:

### Sonnet 4.5 (OpenRouter) vs qwen3:32b (Ollama)

| Fixture | Sonnet parse | Sonnet ms | Sonnet updates | qwen3 parse | qwen3 ms | qwen3 updates |
|---|---|---|---|---|---|---|
| 01 rust-bug-fix | ✓ | 27,613 | 4 | ✓ | 110,227 | 4 |
| 02 architecture-decision | ✓ | 31,039 | 4 | ✓ | 122,200 | 5 |
| 03 gotcha-with-rule | ✓ | 19,173 | 4 | ✓ | 98,025 | 4 |
| 04 low-signal-session | ✓ | 6,106 | **1** | ✓ | 51,694 | **1** |
| 05 multi-topic-session | ✓ | 47,249 | 4 | ✗* | 133,178 | - |
| **Aggregate** | **5/5** | **avg 26 s** | - | **4/5** | **avg 103 s** | - |

*qwen3's only failure: invented `kind: "concept"` (not in the
`PageKind` enum; valid values are `decision`/`gotcha`/`rule`/
`fact`). Despite the prompt mentioning the valid set, the
model drifted. **This gets fixed in Run 3 below.**

Both models **correctly restrained themselves** on fixture
04 (low-signal-session) and produced a single update, a
non-trivial test the original schema-broken Run 1 couldn't
even reach.

### Haiku 4.5 (OpenRouter) vs Sonnet 4.5 (OpenRouter)

Same prompt, both Anthropic models side-by-side:

| Fixture | Sonnet parse | Sonnet ms | Sonnet updates | Haiku parse | Haiku ms | Haiku updates |
|---|---|---|---|---|---|---|
| 01 rust-bug-fix | ✓ | 34,920 | 4 | ✓ | 16,505 | 5 |
| 02 architecture-decision | ✓ | 31,043 | 4 | ✓ | 13,731 | 4 |
| 03 gotcha-with-rule | ✓ | 24,810 | 4 | ✓ | 14,304 | 4 |
| 04 low-signal-session | ✓ | 5,673 | **1** | ✓ | 4,044 | **1** |
| 05 multi-topic-session | ✓ | 39,189 | 4 | ✓ | 16,026 | 4 |
| **Aggregate** | **5/5** | **avg 27 s** | - | **5/5** | **avg 13 s** | - |

**Haiku is ~2× faster than Sonnet on every fixture**, hits the
same 5/5 parse rate, and on the gotcha-with-rule fixture
correctly classified the `audit-ignore-with-revisit-date`
convention as `kind: rule`, which **Sonnet missed**, calling
it a generic `gotcha`. The auto-routing to `_rules/<slug>.md`
that the consolidator depends on therefore *only fires under
Haiku* for that fixture, not Sonnet.

Quality-wise, Haiku is also more disciplined about
faithfulness than Sonnet even with the loose prompt:

- **Sonnet** invented `Date: 2025-01-23` twice in fixture 5
  (no date in the source observations); fabricated an entire
  `## Alternatives considered` section listing Alpine/Scratch/
  Debian-slim, none of them mentioned in the session; added "Better
  long-term solutions" / "When NOT to ignore" filler.
- **Haiku** had a couple of invented "Options considered"
  entries (Alpine, aggressive optimization flags) but
  otherwise stayed close to the observations.

For consolidation, the headroom Sonnet has over Haiku
showed up as *more hallucination* rather than better
fidelity.

### Kimi-K2.6 (OpenRouter) - INELIGIBLE for this task

After the prompt + schema fixes, the Kimi rerun **hung for
16+ minutes on the first fixture** and never returned a parseable
response. Direct probing of the OpenRouter endpoint showed
why:

```
$ curl … -d '{"model":"moonshotai/kimi-k2.6", "max_tokens": 50, ...}'
{
  "choices": [{
    "message": {
      "content": null,          ← no actual content
      "reasoning": "...208 chars..."
    }
  }],
  "usage": { "completion_tokens": 50, "reasoning_tokens": 50 }
}
```

Kimi-K2.6 is a **reasoning model**: it consumes the
`max_tokens` budget internally as "thinking" before emitting
visible `content`. For a short probe with `max_tokens: 50`,
all 50 tokens went to reasoning and content stayed `null`.

For the consolidation prompt with `max_tokens: 4000`, Kimi
would happily reason for many minutes against the strict-JSON
instructions before *either* emitting JSON or running out of
budget with no content. The eval observed 16 minutes of no
progress on fixture 1 before being killed.

This is **not a fixable prompt or schema issue**. It is a
property of the model's response style. Run 1 only "worked"
on Kimi (in the sense of producing *something*) because the
loose prompt let Kimi emit prose markdown, which used `content`
naturally. The post-fix strict-JSON prompt provokes Kimi's
reasoning mode and starves the visible response.

**Kimi-K2.6 is not a suitable provider for ai-memory's
consolidation workload.** It would work for the broader
"summarise this for me" use case where formatted prose is
fine, but not for our JSON-schema-validated path.

Other reasoning-mode models (Claude with extended thinking,
GPT-o3, Gemini "thinking" variants) would need the same
caveat: turn off reasoning mode, or budget tokens with
reasoning consumption in mind.

## Run 3 - tightened anti-hallucination system prompt

The Run 2 evidence above showed that Sonnet was hallucinating
dates, fabricating "Alternatives considered" tables, and
inventing tutorial sections that weren't in the
observations. Even Haiku slipped occasionally. Instead of
swapping models, the fix tightened the **system prompt** to
demand faithfulness explicitly:

```text
## FAITHFULNESS — the most important rule

The wiki records *what happened in this project*, not what you
know about the topic in general. … Every claim in every page
MUST be grounded in the observations.

Do NOT:
- Invent dates, timestamps, version numbers, commit hashes,
  author names, file paths, function names, line numbers,
  error codes, or any other concrete detail not present in
  the observations.
- Add 'When to use' / 'When NOT to use' / 'Gotchas' / 'Best
  practices' / 'Alternative approaches' / 'See also' sections
  that weren't grounded in the session.
- Enumerate alternatives that weren't actually considered in
  the session.
- Expand terse user comments into long explanations.
- Fabricate code examples that didn't appear in the session.
- Speculate about consequences unless the speculation
  appeared in the observations themselves.

Do:
- Compress and restructure the observations into well-titled
  pages with the right `kind` classification.
- Preserve the user's actual phrasing for decisions and rules.
- Keep page bodies short. A good consolidated page is 100-400
  words of dense fact, not 1500 words of tutorial.
```

This change is in
[`crates/ai-memory-consolidate/src/consolidator.rs`](../crates/ai-memory-consolidate/src/consolidator.rs)
under `pub const BATCH_SYSTEM_PROMPT`.

### Same fixtures, tightened prompt - Haiku vs Sonnet

| Metric | Sonnet (old prompt) | Sonnet (tightened) | Δ |
|---|---|---|---|
| Parse rate | 5/5 | 5/5 | unchanged |
| Avg latency | 27.1 s | **10.8 s** | **−60%** |
| Bytes (fixture 5 raw) | 7,642 | 2,640 | **−65%** |
| Updates per fixture | 4-4-4-1-4 | 3-3-3-1-3 | fewer manufactured pages |
| Invented `Date: 2025-01-23` | **2 occurrences** | 0 | ✓ gone |

| Metric | Haiku (old prompt) | Haiku (tightened) | Δ |
|---|---|---|---|
| Parse rate | 5/5 | 5/5 | unchanged |
| Avg latency | 12.9 s | **7.3 s** | **−43%** |
| Bytes (fixture 5 raw) | 5,888 | 2,191 | **−63%** |
| Updates per fixture | 5-4-4-1-4 | 4-2-4-1-3 | fewer manufactured pages |
| Invented "Options considered" filler | a few | 0 | ✓ gone |

### Same prompt against the local model - Haiku vs qwen3:32b

| Fixture | Haiku parse | Haiku ms | Haiku updates | qwen3 parse | qwen3 ms | qwen3 updates |
|---|---|---|---|---|---|---|
| 01 rust-bug-fix | ✓ | 11,151 | 3 | ✓ | 110,817 | 4 |
| 02 architecture-decision | ✓ | 8,793 | 3 | ✓ | 90,890 | 3 |
| 03 gotcha-with-rule | ✓ | 7,610 | 3 | ✓ | 91,307 | 3 |
| 04 low-signal-session | ✓ | 2,922 | **1** | ✓ | 44,502 | **1** |
| 05 multi-topic-session | ✓ | 9,681 | 3 | ✓ | 122,220 | 5 |
| **Aggregate** | **5/5** | **avg 8 s** | - | **5/5** | **avg 92 s** | - |

**qwen3 went from 4/5 → 5/5** with the tightened prompt: the
explicit field-by-field enumeration of legal `kind` values
eliminated the "concept" drift that broke Run 2.

The tightened prompt had the largest effect of any change in
the investigation. With the same models and no infra changes,
it cut latency ~60%, eliminated date hallucination on Sonnet,
and restored parse rate parity for qwen3.

## Run 4 - budget-tier hosted comparison

After establishing that Haiku 4.5 dominates Sonnet 4.5 on this
task, the question became "is there an even cheaper hosted
option that still works?" Tested two:

### GPT-5.4-mini (OpenRouter) vs Haiku 4.5

| Fixture | GPT-mini parse | GPT-mini ms | GPT-mini updates | Haiku parse | Haiku ms | Haiku updates |
|---|---|---|---|---|---|---|
| 01 rust-bug-fix | ✓ | 4,048 | 4 | ✓ | 9,673 | 4 |
| 02 architecture-decision | ✓ | 4,851 | 4 | ✓ | 8,322 | 3 |
| 03 gotcha-with-rule | ✓ | 4,212 | 4 | ✓ | 8,211 | 3 |
| 04 low-signal-session | ✓ | 4,636 | **2*** | ✓ | 3,258 | **1** |
| 05 multi-topic-session | ✓ | 3,997 | 3 | ✓ | 10,583 | 4 |
| **Aggregate** | **5/5** | **avg 4.3 s** | - | **5/5** | **avg 8.0 s** | - |

*GPT-5.4-mini failed the restraint test on the low-signal
session: it manufactured an extra `decisions/docs-spelling.md`
page for a typo fix. The content was faithful (just restated
the typo correction), but classifying "we fixed a typo" as a
durable architectural decision is over-extraction. Haiku
correctly emitted just the episodic session log with
rationale "Session was trivial; only the episodic record is
warranted."

Otherwise GPT-5.4-mini is **the fastest hosted model tested**
(4.3 s avg, ~2× Haiku) and produces the shortest output
(~2.3 KB on fixture 5 vs ~3.5 KB Haiku, ~2.6 KB Sonnet). No
invented dates, no fabricated tutorial sections.

### DeepSeek V4 Flash (OpenRouter) vs Haiku 4.5

| Fixture | DeepSeek parse | DeepSeek ms | DeepSeek updates | Haiku parse | Haiku ms | Haiku updates |
|---|---|---|---|---|---|---|
| 01 rust-bug-fix | ✓ | 13,921 | 3 | ✓ | 8,817 | 4 |
| 02 architecture-decision | ✓ | 20,835 | 4 | ✓ | 9,196 | 3 |
| 03 gotcha-with-rule | ✓ | 54,203 | 4 | ✓ | 8,376 | 3 |
| 04 low-signal-session | ✓ | 4,049 | **1** | ✓ | 2,837 | **1** |
| 05 multi-topic-session | ✓ | 15,543 | 5 | ✓ | 7,616 | 3 |
| **Aggregate** | **5/5** | **avg 21.7 s** | - | **5/5** | **avg 7.4 s** | - |

DeepSeek V4 Flash passes every reliability bar: 5/5 parse,
correct restraint on low-signal, no hallucinated dates (the
"2026-08" that appeared in its output was *legitimately in the
source observations*), correct `kind: rule` classification.
Notable: fixture 3 took 54 s, suggesting variance under load
or extended reasoning. On multi-topic it produced 5 updates
vs the 3 the other models settled on, slightly more
exuberant than Haiku.

## Comprehensive ranking - all six providers

Ranking on a 0–5 scale per axis, then aggregated.
**Higher is better** in every column except Cost (where
lower-cost gets a higher score). Bold = best in that column.
Rows sorted by overall fitness for ai-memory.

| # | Provider | Parse | Speed | Cost† | Faithfulness | Restraint | Classification | Fitness |
|---|---|---|---|---|---|---|---|---|
| 1 | **Haiku 4.5** | 5 | 4 | 3 | 5 | **5** | **5** | **5** - recommended default |
| 2 | **GPT-5.4-mini** | 5 | **5** | **5** | 5 | 3 | 4 | 4 - cheaper alternative |
| 3 | **qwen3:32b (Ollama)** | 5 | 1 | **5** ($0) | 5 | **5** | 4 | 4 - free if you have a local server |
| 4 | DeepSeek V4 Flash | 5 | 2 | 4 | 5 | **5** | **5** | 4 - no edge over GPT-mini or Haiku |
| 5 | Sonnet 4.5 | 5 | 4 | 1 | 4‡ | 4 | 3 | 3 - displaced by Haiku |
| 6 | Kimi-K2.6 | 0 | 0 | n/a | n/a | n/a | n/a | **0** - ineligible (reasoning model) |

† Cost score derived from order-of-magnitude $/M output tokens:
$0 (qwen3) = 5; ~$0.40 (DeepSeek) = 4; ~$1 (GPT-mini) = 5 by
amortised cost-per-task; ~$5 (Haiku) = 3; ~$15 (Sonnet) = 1.

‡ Sonnet's faithfulness was 2/5 with the loose prompt (invented
dates, fabricated alternatives sections). It recovers to 4
after the tightened prompt, but Haiku achieved that same level
without needing the prompt change as much, suggesting Haiku has
better defaults for this task.

### Why each column matters

- **Parse**: structural reliability. Anything less than 5/5
  means production consolidation can fail silently and lose
  observations.
- **Speed**: matters less for consolidation (it's a background
  job) but compounds for lint sweeps over many pages and
  affects developer iteration on the prompt itself.
- **Cost**: integrates over N sessions/day × 365 days.
  Inexpensive options change the cost from monthly-recurring
  to negligible.
- **Faithfulness**: does the model only write what's in the
  observations? Critical for a *memory* wiki, where fabrication
  corrupts the long-term record.
- **Restraint**: does the model resist manufacturing pages
  when the session is low-signal? Lack of restraint pollutes
  the wiki with thin, manufactured "decision" pages that
  outweigh real content.
- **Classification**: does the model correctly mark rules as
  `kind: rule`, decisions as `kind: decision`, etc.? Wrong
  classification breaks the consolidator's auto-routing to
  `_rules/<slug>.md`.
- **Fitness for ai-memory**: holistic verdict per provider for
  this specific consolidation workload. Not a generic LLM
  benchmark.

### Final ordering

For ai-memory's consolidation task specifically:

1. **Haiku 4.5**: **recommended default for most users.**
   Hosted (always available), 7 s avg latency, restraint +
   classification top of the field, ~$0.02/run is negligible
   for personal use. The benchmark every other option is
   measured against.
2. **GPT-5.4-mini**: **cheaper hosted alternative.** ~5×
   cheaper than Haiku, 2× faster (4 s avg). Only weakness is
   mild over-classification on trivial sessions
   (manufactures one extra "decisions/" page on a typo-fix
   session). If budget matters more than restraint, pick
   this.
3. **qwen3:32b on Ollama**: **free alternative for those
   with a local server.** $0 per consolidation. ~92 s
   latency is invisible because consolidation is a background
   job. Restraint + faithfulness match the top hosted models.
   Requires Ollama (or compatible OpenAI-compat server) with
   `qwen3:32b` pulled and enough RAM/VRAM (~20 GB) to keep
   it warm.
4. **DeepSeek V4 Flash**: **solid but no clear edge.** All
   reliability bars met, faithful, restrained, correctly
   classifies rules. GPT-mini matches it on quality and
   beats it on speed; Haiku matches it on quality and beats
   it on classification consistency. Pick only if your
   workflow is already DeepSeek-leaning.
5. **Sonnet 4.5**: **strictly dominated by Haiku** for
   plain consolidation. 3× the cost for the same parse rate
   and only marginally different latency. Reserve for tasks
   that *specifically* need extended reasoning (cross-page
   lint sweeps that compare contradictory claims across many
   pages, or for sparse-observation sessions where you want
   the model to infer more aggressively).
6. **Kimi-K2.6**: **ineligible.** Reasoning model burns
   `max_tokens` budget on internal thinking before emitting
   visible content. Hangs indefinitely on strict-JSON
   prompts. Same caveat applies to any other reasoning-mode
   model (Claude with extended thinking, GPT-o3, Gemini
   "thinking" variants): turn reasoning off or budget
   tokens with consumption in mind before using them here.

## Qualitative read (Run 2)

Reading the raw `.md` outputs side-by-side reveals a
substantive style difference that the parse-rate numbers
don't capture:

- **Sonnet writes long, comprehensive entries.** A concept
  page on Docker multi-stage builds will get 3 KB of well-
  organised prose including "When to use" / "When NOT to use"
  / "Gotchas" sections, none of which *was in the
  observations*. The model is generating useful tutorial-style
  content instead of strictly consolidating what happened.
  Sonnet's fixture 05 page invented a `Date: 2025-01-23`
  field that has no source in the observations.

- **qwen3 writes terse, faithful entries.** Each page captures
  what the session actually contained, in ~500–800 chars.
  No invented metadata, no generic tutorial filler. The same
  Docker page from qwen3 stays close to "we changed the
  Dockerfile to two-stage, image went 380→67 MB" without
  diverging into broader best-practices discussion.

For **wiki consolidation** (faithful long-term memory of
*this project*, not a knowledge graph of general best
practices), **qwen3's restraint is arguably preferable** to
Sonnet's exuberance. The wiki exists to record what happened
in the project; re-generated tutorial content the model already
knows does not belong there.

When the project memory is sparse and the model is asked to
surface durable knowledge, though, Sonnet's "fill in the
obvious" tendency could pay off.

## Verdict

After three iterations of fixes (schema → first prompt → tightened
prompt), the picture is clear:

### Production default: Ollama qwen3:32b

- **Parse**: 5/5 (tightened prompt)
- **Latency**: ~92 s avg end-to-end. Acceptable because
  consolidation is a background job, not interactive.
- **Cost**: **$0 per consolidation** (electricity not modeled).
- **Fidelity**: comparable to or better than the hosted models;
  qwen3 was the most faithful provider in Run 2's old-prompt
  comparisons.

### Best hosted fallback: Claude Haiku 4.5

If the homelab is unreachable, or for one-off complex
consolidations, **Haiku 4.5 is the right hosted choice, not
Sonnet 4.5**:

- **2× faster** than Sonnet at every fixture.
- **~3× cheaper** per token (Anthropic published pricing:
  Haiku 4.5 ≈ $1/$5 per M input/output tokens vs Sonnet 4.5
  ≈ $3/$15).
- **Less hallucination-prone** even on the loose prompt.
- **Better classification** on at least one fixture (correctly
  identified the rule that Sonnet flattened to a gotcha).
- Same 5/5 parse reliability.

### Sonnet 4.5 - displaced by Haiku for this task

Sonnet's reasoning headroom doesn't help consolidation. With
the loose prompt it showed up as *more hallucination*
(invented dates, fabricated alternative-considered tables,
tutorial-style filler). The tightened prompt brings Sonnet in
line, but Haiku gives identical reliability faster and
cheaper. Reserve Sonnet for tasks where the extra reasoning
matters (e.g. cross-page lint sweeps that compare contradictory
claims).

### Kimi-K2.6 - ineligible

Reasoning model that burns `max_tokens` budget internally before
emitting visible content. Run hung for 16+ minutes on fixture 1
under the strict-JSON prompt. Direct probe confirmed: `content:
null` with the entire token budget consumed by `reasoning`.
Not a prompt problem; the model is structurally wrong for
strict-JSON output. Same caveat applies to other reasoning-
mode models if used in this pipeline.

### Cost / latency snapshot

| Provider | $/run* | latency | notes |
|---|---|---|---|
| Ollama qwen3:32b (local) | **$0** | ~92 s | electricity not modeled |
| GPT-5.4-mini (OpenRouter) | ~$0.005 | **~4 s** | fastest + cheapest hosted |
| DeepSeek V4 Flash (OpenRouter) | ~$0.005 | ~22 s | cheap but slower than GPT-mini |
| Haiku 4.5 (OpenRouter) | ~$0.02 | ~7 s | best restraint/classification |
| Sonnet 4.5 (OpenRouter) | ~$0.06 | ~11 s | 3× cost of Haiku for same task |
| Kimi-K2.6 (OpenRouter) | n/a | ✗ hangs | reasoning model, ineligible |

\* Rough order of magnitude; ai-memory consolidations land
around 2–3 KB of output with the tightened prompt. Per-run $
multiplies $/M-tokens by the input+output token budget.

### When to revisit

Re-run this harness when any of the following changes:

- The consolidation prompt itself is re-engineered
- A new Ollama model is pulled (e.g. when Qwen 3.5 stable
  drops for Ollama)
- A new fixture is added to `evals/fixtures/`
- The home server hardware changes
- A local engine changes its `response_format=json_schema` implementation,
  making a fresh default-vs-`AI_MEMORY_LLM_COMPAT_STRICT=false` comparison
  worthwhile

## How to reproduce

### Pre-requisites

- Repo checkout + `cargo` toolchain (Rust 1.95+, as pinned in
  `rust-toolchain.toml`).
- An OpenRouter API key, exported as `OPENROUTER_API_KEY`, which
  pays for the Kimi + Sonnet legs.
- A reachable Ollama with `qwen3:32b` pulled. The default URL
  in the docs assumes the homelab; substitute your own.

### Run the harness

The canonical 2-side invocation (the harness compares two
providers per run):

```bash
cargo run -p ai-memory-eval --release -- \
    --baseline-provider  openai-compat \
    --baseline-base-url  https://openrouter.ai/api/v1 \
    --baseline-model     moonshotai/kimi-k2.6 \
    --baseline-api-key-env OPENROUTER_API_KEY \
    --candidate-provider openai-compat \
    --candidate-base-url http://192.168.0.90:11434/v1 \
    --candidate-model    qwen3:32b \
    --candidate-api-key  ollama-local
```

For a 3-way comparison, run the harness three times pairing the
candidate (the model you're considering switching to) against
each baseline you want to compare against. Output dirs are
timestamped, so they don't collide.

### Read the output

```
evals/runs/<timestamp>/
├── baseline/
│   ├── 01-rust-bug-fix.json          ← parsed structured output (if any)
│   ├── 01-rust-bug-fix.md            ← flat-rendered for eyeballing
│   ├── 01-rust-bug-fix.raw.txt       ← exact model output, always present
│   └── 01-rust-bug-fix.meta.json     ← {elapsed_ms, parsed_ok, update_count, error}
└── candidate/
    └── ...
```

The `.raw.txt` files are the most informative artifact when a
parse fails because they show *exactly* what the model said, so you
can tell whether the failure was format (model emitted prose),
schema (model used integer enums), or substance (model
produced nothing useful).

For side-by-side reading the runner prints a hint:

```
compare with: diff -ru <run>/baseline <run>/candidate
```

### Adding new fixtures

Each fixture is a JSON file under `evals/fixtures/`:

```json
{
  "name": "human-readable-id",
  "description": "what this case is meant to surface",
  "observations": [
    {"kind": "session-start", "title": "...", "body": "..."},
    {"kind": "user-prompt",   "title": "user prompt", "body": "..."},
    {"kind": "pre-tool-use",  "title": "Edit", "body": "..."}
  ]
}
```

`kind` accepts any string the
[`ObservationKind`](../crates/ai-memory-core/src/observation.rs)
enum's `FromStr` understands. Anything unknown silently falls
back to `Other`.

Try to hit one of the four hard cases:

1. **Multi-page extraction**: does the model split a session
   into the right slices?
2. **Restraint**: does it avoid manufacturing pages when
   there's nothing durable?
3. **Classification**: does it correctly choose `kind: rule`
   for project rules?
4. **Topic separation**: does it produce separate pages per
   unrelated topic instead of mashing them?

## What's NOT in this harness (yet)

- **Automated quality scoring.** The runner only reports
  objective deltas (latency, parse rate, update count).
  Anything subtler (faithfulness, hallucination, scoping)
  needs a human reader.
- **Embedding A/B.** This document is LLM-only. The embedding
  provider switch (OpenAI text-embedding-3-small → Ollama
  nomic-embed-text) gets its own writeup when there's enough
  page-side data to measure retrieval quality.
- **LLM-as-judge scoring.** Adding a third "judge" model to
  score the candidate outputs against a rubric would
  automate quality measurement. Not built; the next layer up
  if this harness gets used regularly.

## Future work

If we end up running this harness routinely:

1. Add a third position (`--judge-*`) so a separate "judge"
   model can score baseline vs candidate per fixture against a
   rubric, producing a numeric quality delta.
2. Extend fixtures with a `must_mention` / `must_not_mention`
   keyword list so we can compute simple keyword recall
   automatically (catches obvious hallucinations / missing
   facts).
3. Parallel embedding-retrieval eval: a probe set of queries
   each tagged with the expected target wiki page; compute
   recall@5 + MRR for two embedding models against the same
   indexed corpus.
4. Persist a leaderboard somewhere durable (a wiki page,
   ironically) so we don't lose track of which model performed
   best on which fixture across runs.
