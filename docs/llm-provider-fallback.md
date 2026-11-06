# LLM provider fallback chains

Issue: #648

The top-level LLM provider can use an ordered fallback chain. Individual tasks
can also select their own provider and fallback chain.

## Task-specific providers

Configure an optional `[llm.<task>]` table for any of these tasks:
`consolidation`, `bootstrap`, `auto_improve`, `experience`, `dream`, `lint`,
`profile`, `rerank`, and `query_answer`. The `query_answer` provider handles
both `memory_query(answer=true)` and `memory_explore`. Scheduled lint is
rule-only unless `[maintenance] lint_llm_interval_secs` turns on the LLM pass,
which uses the `lint` provider.

```toml
[llm.bootstrap]
provider = "openai-compat"
model = "local-model"
base_url = "http://127.0.0.1:49375/v1"
api_key_env = "AI_MEMORY_LOCAL_ROUTER_TOKEN"

[[llm.bootstrap.fallbacks]]
provider = "gemini"
model = "gemini-3.5-flash"
api_key_env = "GEMINI_API_KEY"
```

The task's `provider` is required and nonblank. `model` defaults to that
provider's default model, except `openai-compat` requires both `model` and
`base_url`. A task never inherits the global endpoint. `api_key_env` names an
environment variable, not a credential value. Without it, the provider uses
its normal credential source. OpenAI OAuth, Codex and Cursor keep their
native login files or CLI and reject `api_key_env`. An explicitly named
variable that is missing fails `ai-memory serve` startup and never falls
back to the default provider's key.

When a task has no profile, it shares the same default provider instance and
fallback chain. A task profile replaces that chain with its own primary and
ordered fallbacks; global fallbacks are not appended. Task profiles work when
the global provider is unset, without enabling other tasks. Existing timeout,
compatibility, header and reasoning settings apply globally.

Scalar fields have nested environment forms, such as
`AI_MEMORY_LLM__BOOTSTRAP__PROVIDER` and `AI_MEMORY_LLM__BOOTSTRAP__MODEL`.
Fallback lists are configured in `config.toml`, not through environment
variables. For each fallback's required fields, see
[`config-properties.md`](config-properties.md#llm_fallbacks).

## Original chain design

The following describes the original global fallback chain.

## Goals

- Fail over only after a transient provider failure, using the established
  `LlmError::is_transient()` policy.
- Preserve the original request, JSON schema, and logical operation id on every
  candidate attempt.
- Keep credentials in the existing one-time configuration load and out of logs,
  status payloads, and persisted state.
- Use the configured global request timeout for each candidate.
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
resolves all credential material once. An invalid provider or malformed profile
fails every command at load. A credential whose `api_key_env` variable is absent
fails `ai-memory serve` at startup, rather than leaving a latent fallback that
only fails under an outage, but does not stop other commands: `ai-memory
status` and the other thin clients never build the chain, so the variable only
has to be in the server's environment (for example a service wrapper's env
block), not exported in every shell.

The primary runs first, then the fallbacks in declaration order. Fallback
profiles have no environment-variable shorthand.

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

The same rule covers the credential exchanges in front of a provider: the
Copilot token exchange and the `openai-oauth`/OIDC refresh. A 429 or 5xx from
those token endpoints is transient, because the grant was not rejected; any
other non-2xx stays an auth failure with its re-login hint.

Before a candidate is called, the wrapper checks that chain entry's in-memory
circuit. Two entries with the same provider and model but different credentials
therefore rotate independently. A transient error opens only that entry's
circuit. The default cooldown is 30 seconds; a delta-seconds `Retry-After`
header or Google `RetryInfo.retryDelay` response detail extends it, clamped to
one hour. Other candidates remain eligible. A successful response closes that
candidate's circuit. Restarting the server clears circuits. The implementation
uses no durable circuit state and no separate forced request deadline.

If every eligible candidate fails, the wrapper returns a bounded aggregate
error containing provider/model labels and error classes, never response bodies
or secrets.

## Health reporting

`ProviderHealth` preserves the global LLM role and its candidate list.
The `llm_task_chains` field reports each explicit task override separately,
with its provider, model, status and candidate list. Task-only configurations
keep the global role disabled while recording their own workload metrics.
Per-candidate state contains:

- provider and model;
- configured / last-selected state;
- last success or error timestamp;
- redacted error class and HTTP status when available;
- circuit-open-until timestamp.

Status records only calls the server already made. It does not probe fallbacks
or trigger background recovery traffic.

The server log carries the same events as they happen, with the same redacted
fields and never a response body. A transient failure logs a warning naming
the failing candidate's provider, model, chain position, error class, HTTP
status and cooldown; that is the only place the failing model shows up,
because callers such as the reranker label the chain with its first
candidate. A deterministic failure logs at debug level, since the caller
reports the returned error itself. An info line records each change of the
candidate that answers, so a primary that stays down yields one line, not one
per call.

## Tests

1. Unit-test chain order with fake `LlmProvider` instances.
2. Verify all four trait methods preserve request/schema/operation id.
3. Verify `429`, `5xx`, timeout, and connection failures advance; verify
   deterministic errors stop on the first candidate.
4. Verify an open circuit skips only its `(provider, model)` candidate and a
   success closes it.
5. Test configuration validation, including missing/empty profiles and missing
   credentials, through `Config::load()` without reading a real home directory;
   a missing credential is checked by `require_llm_fallback_credentials()` and
   `llm_provider_chain()`, not by `load`.
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
