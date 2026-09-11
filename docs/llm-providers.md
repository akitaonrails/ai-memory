# LLM Providers

> Provider configuration for consolidation and embeddings. Moved verbatim from the README front page.

ai-memory runs without an LLM: hooks still capture sessions, search uses
FTS5 + declared entities + graph neighbors, and summaries fall back to
rule-based output. Add an LLM provider
when you want LLM consolidation (on PreCompact, on demand via
`memory_consolidate`, or opt-in at session end with
`AI_MEMORY_CONSOLIDATE_ON_SESSION_END`), richer linting, and bootstrap.
Substantive session ends always write a rule-based summary page + handoff either
way. A session containing only `SessionStart` / `SessionEnd` boundaries is
closed without a page, handoff, or provider job. When that empty session had
accepted startup context, its session-bound handoff is returned to the open
pool for the next receiver instead of being lost.
When the session-end opt-in is enabled, provider work is durably queued after
those deterministic writes and handled by one bounded server worker, so hook
drain latency does not cancel it. Failed jobs retry with backoff and survive a
server restart. A resumed native session is ended again only after its
observation generation advances; the persisted generation watermark makes
duplicate SessionEnd delivery and system clock skew converge without repeated
provider work. The end watermark and automatic handoff commit atomically, and
an interrupted keyed replay finishes the wiki commit, queue insert, and key
completion without duplicating that handoff. On the next SessionStart, the
newest cwd-eligible automatic handoff wins; accepting it expires older eligible
automatic handoffs without consuming manual or sibling-directory work. A new
automatic handoff also expires prior open automatic handoffs from its exact
cwd, so repeated SessionEnds cannot accumulate there before a receiver starts.

To keep consolidation style project-specific, write
`_prompts/consolidation.md` in that project's wiki. Its body can express
preferences such as "prefer Portuguese titles" or "omit routine CI noise".
Automatic, single-page, and multi-page consolidation use the page; a manual
`memory_consolidate` call can pass `instructions` to override it once. ai-memory
sanitizes and caps the value at 2,000 characters, JSON-encodes it in the user
message, and treats it as untrusted advisory data. It cannot supply facts,
request tool use or disclosure, or override the consolidation schema and
faithfulness rules. TTL-expired preference pages are ignored. With no active
page or argument, no preference block is appended.

Recommended defaults:

| Provider | Default | Use when |
|---|---|---|
| `anthropic` | `claude-haiku-4-5` | Best default for consolidation quality and rule classification. |
| `anthropic-oauth` | `claude-sonnet-4-6` | Use a Claude Pro/Max subscription via `claude setup-token`, no API key. |
| `openai` | `gpt-5.4-mini` | Cheaper and faster hosted option. |
| `openai-oauth` | `gpt-5.5` | ChatGPT Pro/Plus/Codex backend via `ai-memory auth login openai-oauth`; no Platform API key. |
| `codex` | `gpt-5.6-luna` | Reuse the Codex CLI-owned `auth.json`; access-token refresh remains owned by `codex app-server`. |
| `copilot` | `gpt-5.5` | GitHub Copilot Chat backend via `ai-memory auth login copilot` or `COPILOT_GITHUB_TOKEN`; requires a Copilot subscription. |
| `gemini` | `gemini-3.5-flash` | Google-hosted option with a generous free tier. |
| `openai-compat` | no default | OpenRouter, Atlas Cloud, OrcaRouter, Ollama, vLLM, LM Studio, and other compatible endpoints. |

`openai-oauth` stores a refresh token in `<data_dir>/auth.json` and talks to
the ChatGPT/Codex Responses backend, not `api.openai.com`. For Docker quick
starts, run `ai-memory auth login openai-oauth` with the wrapper so the token
lands in the same `ai-memory-data` volume as the server.

`codex` is independent from `openai-oauth`: it never copies credentials into
ai-memory's data directory and never reads a refresh token or ID token. It
reads only `tokens.access_token` and `tokens.account_id` from
`$CODEX_HOME/auth.json`, or from the platform home's `.codex/auth.json` when
`CODEX_HOME` is unset or empty. Each request reloads the file. A first 401 may
trigger one serialized `codex app-server --stdio` recovery followed by one
retry; further 401 responses fail with a reauthentication hint. Override the
binary with `AI_MEMORY_CODEX_EXECUTABLE` when `codex` is not on `PATH`.

```bash
export AI_MEMORY_LLM_PROVIDER=codex
export AI_MEMORY_LLM_MODEL=gpt-5.6-luna
export AI_MEMORY_LLM_REASONING_EFFORT=medium
ai-memory llm-test --provider codex --model gpt-5.6-luna --prompt "Reply with OK"
```

Codex credential storage mode `file` is supported. `auto` works only when its
effective credential is present in `auth.json`; keyring-only and ephemeral
credentials are not read and produce an actionable missing-auth-file error.
Docker is not configured automatically: the Codex executable, `CODEX_HOME`,
and its credential file must all exist in the same container/environment as
ai-memory.

`anthropic-oauth` hits the same `/v1/messages` endpoint as `anthropic` but
authenticates with an OAuth bearer token instead of an API key. Run
`claude setup-token` once, then set `AI_MEMORY_LLM_PROVIDER=anthropic-oauth` and
`ANTHROPIC_OAUTH_TOKEN=<token>` (or `CLAUDE_CODE_OAUTH_TOKEN`, which `claude
setup-token` writes automatically). No `ANTHROPIC_API_KEY` is needed. The Docker
wrappers forward either token by name to short-lived helper commands such as
`llm-test`; configure the long-lived server container separately as shown in the
installation guide.

For both Anthropic providers, ai-memory omits `temperature` for Claude
4.7 and later models and Claude Mythos Preview because those models reject
non-default sampling parameters. `llm-test` sends the same representative 0.2
value as the normal pipeline before the provider applies that compatibility
rule.

**⚠️ Unofficial and against Anthropic's usage policies — use at your own risk;
it may get your account rate-limited or banned. See
[the warning in `docs/install.md`](docs/install.md#anthropic-via-claude-subscription-oauth).**

`copilot` stores a GitHub user token in the same auth file, exchanges it for a
short-lived Copilot API token via GitHub's `/copilot_internal/v2/token`, and
uses the Copilot Chat endpoint with `vscode-chat` integration headers. You can
also set `COPILOT_GITHUB_TOKEN`, `GH_TOKEN`, or `GITHUB_TOKEN` on the server.

> [!TIP]
> **For the OAuth/subscription backends (`anthropic-oauth`, `openai-oauth`, `codex`,
> `copilot`), pick a small, fast model** via `AI_MEMORY_LLM_MODEL` — e.g.
> `claude-haiku-4-5` or `gpt-5-mini`. ai-memory's LLM work (consolidation,
> lint, explore) is summarisation, not hard reasoning, so a Haiku/mini-class
> model is plenty and is much easier on subscription rate limits. Save the
> high-effort thinking models for your coding agent.

> [!TIP]
> **OpenAI-compatible structured output is schema-constrained by default.**
> ai-memory sends each operation's JSON Schema through
> `response_format=json_schema`, which recent Ollama, vLLM, LM Studio, and
> llama.cpp releases honour. It falls back to the tolerant parser when an
> endpoint explicitly rejects that field or returns a malformed shape. Set
> `AI_MEMORY_LLM_COMPAT_STRICT=false` only for an incompatible endpoint.

For small-context local models, configure both consolidation limits. The input
target accounts for the complete rendered prompt, including bounded slot and
current-page context plus the structured-output schema; the output limit is
sent to the provider. Their sum must fit the model context window, with extra
headroom because provider tokenizers differ:

```toml
[consolidation]
max_input_tokens = 6500
max_output_tokens = 1000
```

The equivalent environment variables are
`AI_MEMORY_CONSOLIDATION__MAX_INPUT_TOKENS` and
`AI_MEMORY_CONSOLIDATION__MAX_OUTPUT_TOKENS`. Provider failures during an
automatic PreCompact/PostCompaction checkpoint fall back to the deterministic
rule-based page; admission, storage, and scope errors still fail closed. The
validated minimums are 6,000 input and 1,000 output tokens.

Every chat provider bounds each completion request at 300 seconds
(`AI_MEMORY_LLM_TIMEOUT_SECS` to override, or `llm_timeout_secs = 900` in
config.toml; the quick openai-oauth token refresh keeps the default ceiling).
The default tolerates a local engine cold-loading a large model; slow hosted
gateways whose long completions exceed the ceiling fail every request with
`http: error sending request`, so raise the value there instead of watching
consolidation exhaust its retries.

Reranking is optional and off by default. With an LLM provider configured,
`AI_MEMORY_RERANKER=llm` makes project and explicit-scope `memory_query`
calls over-fetch from the hybrid stage, fuse scopes, and make at most one LLM
call to reorder the best candidates. This can promote a relevant page that
RRF ranked below the requested cut, at the cost of LLM latency and usage. The
request sends the query plus at most 30 bounded page titles and search snippets
to the configured provider; all values are JSON-encoded and treated as
untrusted data. A timeout, provider error, or incomplete/invalid score set
preserves the normal order. `global=true` and supplemental global-preference
hits keep their existing non-RRF ranking. Concurrent provider calls are capped
at four; saturated queries keep their local ranking without waiting.

Embeddings are optional and separate from the LLM provider. Set
`AI_MEMORY_EMBEDDING_PROVIDER=openai`, `voyage`, `google`/`gemini`, or
`openai-compat` when you want vector retrieval in addition to FTS5 + entity +
graph-neighbor retrieval. `openai-compat` targets self-hosted engines
(Ollama, LM Studio, vLLM): it needs no API key and requires explicit
`AI_MEMORY_EMBEDDING_BASE_URL`, `AI_MEMORY_EMBEDDING_MODEL`, and
`AI_MEMORY_EMBEDDING_DIM`. The optional `EMBEDDING_API_KEY` credentials the
embedder alone and is checked before `OPENAI_API_KEY` and `LLM_API_KEY`, so
embeddings can run on a different provider than the LLM. Both the FTS-only and
hybrid paths apply the same bounded page-authority adjustment after candidate
generation; embeddings improve relevance recall but do not decide which source
is canonical.

`AI_MEMORY_EMBEDDING_PROVIDER=local` needs no key and no server at all:
sentence embeddings run in-process (pure-Rust `all-MiniLM-L6-v2`,
384-dim), with the model fetched once into `<data_dir>/models/` under
pinned checksums — see [`docs/local-embeddings.md`](local-embeddings.md).

See [`docs/install.md#llm-provider-tiers`](docs/install.md#llm-provider-tiers)
for env vars and Ollama/OpenRouter/Atlas Cloud/OrcaRouter examples, and
[`docs/llm-provider-comparison.md`](docs/llm-provider-comparison.md)
for the empirical model comparison.
