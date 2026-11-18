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
compatibility, header and reasoning settings apply globally, and so does
`llm_quota_cooldown_secs` (see
[Credit and quota exhaustion](#credit-and-quota-exhaustion)).

Scalar fields have nested environment forms, such as
`AI_MEMORY_LLM__BOOTSTRAP__PROVIDER` and `AI_MEMORY_LLM__BOOTSTRAP__MODEL`.
Fallback lists are configured in `config.toml`, not through environment
variables. For each fallback's required fields, see
[`config-properties.md`](config-properties.md#llm_fallbacks).

## Original chain design

The following describes the original global fallback chain.

## Goals

- Fail over only after a transient provider failure, using the established
  `LlmError::is_transient()` policy, or after the provider reports that the
  account is out of credit, quota or billing (`LlmError::is_quota_exhausted()`).
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
| exhausted credit / quota / billing, disabled account (any status, see below) | Yes | The same request succeeds elsewhere; this provider fails until someone tops up or the quota resets. |
| 400 / 401 / 403 / 404 / 422 without a credit or quota signal | No | Usually request, capability, model, or credential configuration. |
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
one hour. If overlapping requests fail, the circuit keeps the later deadline.
Other candidates remain eligible. A successful response closes that candidate's
circuit. Restarting the server clears circuits. The implementation
uses no durable circuit state and no separate forced request deadline.

If every eligible candidate fails, the wrapper returns a bounded aggregate
error containing provider/model labels and error classes, never response bodies
or secrets. A candidate skipped because its circuit is open appears as
`provider/model: skipped (circuit open: quota-exhausted)` (or `transient`), so
the error says why a candidate was not attempted.

## Credit and quota exhaustion

When an account runs out of credit, the HTTP status alone looks like a
malformed request or a short rate limit: Anthropic sends
`400 invalid_request_error`, OpenAI `429 insufficient_quota` and OpenRouter
`402`. Only the `429` used to advance the chain, so an Anthropic primary with
an empty balance failed every consolidation and the configured fallback never
ran.

`LlmError::failover_class()` returns `Transient`, `QuotaExhausted` or
`Permanent`, and `is_transient()`, `is_quota_exhausted()` and
`should_fail_over()` all derive from it. The quota check lives in
`crates/ai-memory-llm/src/quota.rs`. It reads only the bounded error body that
`LlmError::Provider` already carries (at most 1 KiB, with secrets scrubbed),
returns a boolean, and logs no part of the body.

| Provider | Signal treated as `QuotaExhausted` |
| --- | --- |
| `anthropic`, `anthropic-oauth` | `credit balance is too low` (HTTP 400), `billing_error` (402), `reached your specified API usage limits` (monthly spend limit), `organization has been disabled` (403) |
| `openai` | `insufficient_quota` (429), `billing_hard_limit_reached`, `billing_not_active`, `account_deactivated` (401) |
| `openai-compat` (OpenRouter, DeepSeek and similar gateways) | any `402`; `insufficient credits`, `insufficient balance`, `out of credits`, `requires more credits`, `key limit exceeded` |
| `gemini` | `429 RESOURCE_EXHAUSTED` whose violated quota is a daily one (`PerDay`) or has `limit: 0`; `spending cap`; `credits are depleted` |
| `openai-oauth`, `codex` | `usage_limit_reached` / `usage limit has been reached` (also when it arrives as an SSE terminal error, reported as 502) |
| `copilot` | `quota_exceeded`, `monthly quota` |
| `opencode` | any `402`; `insufficient balance`, `out of credits` |

The markers are matched case-insensitively against every provider's body,
because gateways relay each other's errors. A per-minute rate limit stays
transient. Gemini reuses OpenAI's "exceeded your current quota" wording for its
per-minute limit, so that phrase is not a marker.

A plain `401` (an invalid, revoked or mistyped key) never fails over. The
operator has to fix the key, and moving every call to the fallback would hide
the problem. A `401` or `403` fails over only when its body carries an explicit
billing, quota, credit or disabled-account marker from the table, such as
OpenAI's `account_deactivated` or Anthropic's disabled organization.

The consolidation and bootstrap retry loops do not re-send an exhausted-quota
error two seconds later, even when it arrives as a `429`, because it is not
transient. The SessionEnd consolidation queue keeps such a job on its bounded
backoff instead of marking it terminal: a top-up or quota reset can clear it
within that window, as it always could for a `429`.

A quota failure opens the candidate's circuit for `llm_quota_cooldown_secs`
(default 900, accepted range 30 to 86400, env
`AI_MEMORY_LLM_QUOTA_COOLDOWN_SECS`). A longer provider retry hint, such as a
quota reset time, stretches the window up to one hour or the configured value,
whichever is longer. A transient failure that arrives while the window is open
neither shortens it nor changes its reason. The candidate gets no traffic while
its circuit is open. The first real call after the window tries it again: if it
answers, it takes over again and its circuit closes; if it is still exhausted,
that call costs one rejected request and the window starts again. Nothing
probes in the background, and a restart clears the state.

Example, Anthropic as primary with OpenAI taking over when the Anthropic
balance runs out:

```toml
llm_provider = "anthropic"
llm_model = "claude-sonnet-4-6"
llm_quota_cooldown_secs = 900

[[llm_fallbacks]]
provider = "openai"
model = "gpt-5.4-mini"
api_key_env = "OPENAI_API_KEY"
```

`ANTHROPIC_API_KEY` and `OPENAI_API_KEY` must both be in the server's
environment.

## Health reporting

`ProviderHealth` preserves the global LLM role and its candidate list.
The `llm_task_chains` field reports each explicit task override separately,
with its provider, model, status and candidate list. Task-only configurations
keep the global role disabled while recording their own workload metrics.
Per-candidate state contains:

- provider and model;
- configured / last-selected state;
- last success or error timestamp;
- redacted error class (`quota-exhausted` for a credit or quota failure) and
  HTTP status when available;
- circuit-open-until timestamp and `circuit_reason` (`transient` or
  `quota-exhausted`) while it is open.

`ai-memory status` prints a headline when the primary is out of credit or
quota, for example
`primary anthropic/claude-sonnet-4-6 exhausted (credit/quota); using fallback openai/gpt-5.4-mini`,
and marks that candidate `out of credit/quota; skipped until <time>, then
probed again`.

Status records only calls the server already made. It does not probe fallbacks
or trigger background recovery traffic.

The server log carries the same events as they happen, with the same redacted
fields and never a response body. A transient failure logs a warning naming
the failing candidate's provider, model, chain position, error class, HTTP
status and cooldown; that is the only place the failing model shows up,
because callers such as the reranker label the chain with its first
candidate. A quota failure logs its own warning (`out of credit or quota`) with
the same redacted fields and the quota cooldown. A deterministic failure logs
at debug level, since the caller reports the returned error itself. An info
line records each change of the candidate that answers, so a primary that
stays down yields one line, not one per call.

## Tests

1. Unit-test chain order with fake `LlmProvider` instances.
2. Verify all four trait methods preserve request/schema/operation id.
3. Verify `429`, `5xx`, timeout, and connection failures advance; verify
   deterministic errors stop on the first candidate. Verify each provider's
   credit/quota body advances (unit fixtures in `quota.rs`, real providers
   against a local mock in `tests/suite/fallback_provider.rs`), that the
   exhausted candidate is skipped for the quota window and probed after it,
   and that a plain invalid-key `401` does not advance.
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
