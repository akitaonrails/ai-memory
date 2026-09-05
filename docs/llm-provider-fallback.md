# Proposed LLM provider fallback chains

Issue: #648

## Problem

The server currently constructs one `Arc<dyn LlmProvider>` for every LLM-backed
operation. Existing retry loops can retry a transient failure against that same
provider, but cannot use a second configured provider. A temporary rate limit
or upstream outage can therefore degrade a bootstrap, consolidation, lint, or
review even when another provider is healthy.

This proposal introduces an opt-in ordered chain behind the existing
`LlmProvider` trait. It deliberately leaves the zero-LLM and existing
single-provider paths unchanged.

## Goals

- Fail over only after a transient provider failure, using the established
  `LlmError::is_transient()` policy.
- Preserve the original request, JSON schema, and logical operation id on every
  candidate attempt.
- Keep credentials in the existing one-time configuration load and out of logs,
  status payloads, and persisted state.
- Avoid a global timeout policy. Each candidate uses its own configured request
  timeout.
- Make the selected candidate and failover outcome observable through passive
  provider-health reporting.

## Non-goals

- A router for arbitrary third-party APIs or a Command Code integration.
- Reading credentials from another application's database or installing a
  provider CLI.
- Retrying deterministic failures such as authentication, invalid requests,
  unsupported schema, or malformed responses.
- Changing embeddings, hook latency, wiki/storage behavior, or MCP schemas in
  the first implementation.

## Configuration shape

The current top-level LLM fields remain the primary profile. Fallbacks live in
`config.toml` so a profile can carry an explicit base URL and a credential
*environment-variable name* without putting credential values in config.

```toml
llm_provider = "opencode"
llm_model = "mimo-v2.5-free"

[[llm_fallbacks]]
provider = "openai-compat"
model = "poolside/laguna-s-2.1-free"
base_url = "http://127.0.0.1:49375/v1"
api_key_env = "AI_MEMORY_LOCAL_ROUTER_TOKEN"

[[llm_fallbacks]]
provider = "gemini"
model = "gemini-3.5-flash"
api_key_env = "GEMINI_API_KEY"
```

`api_key_env` is optional when the provider already has a native credential
source, such as OpenAI OAuth or Copilot. The loader validates every profile and
resolves all credential material once. A missing credential, invalid provider,
or malformed profile fails startup rather than leaving a latent fallback that
only fails under an outage.

The proposed initial semantics are append-only: the primary runs first, then
the fallbacks in declaration order. No environment-variable shorthand is added
until it can encode profile boundaries and credential names unambiguously.

## Implementation boundary

Add `FallbackLlmProvider` in `crates/ai-memory-llm/src/fallback.rs`:

```text
Config::load
  -> primary ProviderConfig + fallback ProviderConfig values
  -> build_provider for each value
  -> FallbackLlmProvider(Vec<Candidate>)
  -> existing Arc<dyn LlmProvider> consumers
```

Each `Candidate` records only a provider name and model for diagnostics plus its
`Arc<dyn LlmProvider>`. It does not retain raw configuration or credentials.
The wrapper implements all four trait entry points:

- `complete`
- `complete_with_operation_id`
- `complete_structured_raw`
- `complete_structured_raw_with_operation_id`

Each method calls the corresponding candidate method. A structured call passes
the unchanged schema to every eligible candidate; an operation-aware call passes
the same `LlmOperationId` to every eligible candidate. This keeps caller retry
and idempotency semantics intact.

`build_provider` remains the sole dialect/auth construction path. The CLI stays
thin: `Config::llm_provider_chain()` can return either `None`, one provider, or
the wrapper, while callers still receive `Option<Arc<dyn LlmProvider>>`.

## Failure policy and circuit state

| Failure | Try next candidate? | Rationale |
| --- | --- | --- |
| 429 | Yes | Existing transient policy. |
| 5xx | Yes | Existing transient policy. |
| timeout / connection error | Yes | Existing transient policy. |
| 400 / 401 / 403 / 404 / 422 | No | Usually request, capability, model, or credential configuration. |
| schema / response-shape / deserialize error | No | Same input would deterministically fail again. |

Before a candidate is called, the wrapper checks an in-memory circuit keyed by
`(provider, model)`. A transient error opens that candidate's circuit for a
bounded cooldown; other candidates remain eligible. A successful response
closes that candidate's circuit. Restarting the server clears circuits. The
initial implementation uses no durable circuit state and no separate forced
request deadline.

If every eligible candidate fails, the wrapper returns a bounded aggregate
error containing provider/model labels and error classes, never response bodies
or secrets.

## Health reporting

`ProviderHealth` currently represents one LLM role. Extend its LLM snapshot
with a candidate list while preserving the existing top-level active
provider/model fields for compatibility. Per-candidate state contains:

- provider and model;
- configured / last-selected state;
- last success or error timestamp;
- redacted error class and HTTP status when available;
- circuit-open-until timestamp.

Status records only calls the server already made. It does not probe fallbacks
or trigger background recovery traffic.

## Tests

1. Unit-test chain order with fake `LlmProvider` instances.
2. Verify all four trait methods preserve request/schema/operation id.
3. Verify `429`, `5xx`, timeout, and connection failures advance; verify
   deterministic errors stop on the first candidate.
4. Verify an open circuit skips only its `(provider, model)` candidate and a
   success closes it.
5. Test configuration validation, including missing/empty profiles and missing
   credentials, through `Config::load()` without reading a real home directory.
6. Test health snapshots for candidate labels and redaction.
7. Keep existing single-provider config tests byte-for-byte compatible.

## Delivery sequence

1. Add profile parsing, validation, and tests with no behavior change when no
   fallback profile exists.
2. Add the `FallbackLlmProvider` wrapper and focused fake-provider tests.
3. Wire it into `serve`, extend passive health, and add the configuration and
   status documentation plus `CHANGELOG.md` entry.
4. Run the workspace gates and manual `llm-test` cases against a local
OpenAI-compatible endpoint that first returns a transient failure and then a
valid structured completion.
