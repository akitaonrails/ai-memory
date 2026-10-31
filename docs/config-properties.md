# Configuration properties

Every key ai-memory accepts in `config.toml`, one table per section. Each row gives the default as shipped in code, the environment variable that sets the same key, and a one-line purpose. For commented examples and longer explanations, read `crates/ai-memory-cli/templates/config.default.toml`, which `ai-memory init` writes into a new data directory.

Environment variables use the prefix `AI_MEMORY_`. A top-level key `bind` is `AI_MEMORY_BIND`, and a key inside a section uses a double underscore, so `decay.lambda` is `AI_MEMORY_DECAY__LAMBDA`. A list is set from the environment as an array, for example `["a","b"]`. The keys `allowed_hosts`, `cors_allow_origins`, `ingest.drop_events` and `auth.trusted_proxy_cidrs` also accept a comma separated string. Where a key reads a different variable, the Env column names it. A dash in the Env column means the key has no environment form. Order of precedence: built-in defaults, then `config.toml`, then the environment, then command-line flags.

Defaults that come from a named constant or from another crate say so in the Default cell. Settings that exist only as environment variables are not keys, so they are not listed.

## Top-level keys

| Key | Default | Env equivalent | Purpose |
| --- | --- | --- | --- |
| `language` | `"en"` | `AI_MEMORY_LANGUAGE` | Human-facing CLI language (`en` or `pt-BR`); overridden by `--language`. Browser language is per request. |
| `bind` | `127.0.0.1:49374` (`DEFAULT_BIND`) | `AI_MEMORY_BIND` | HTTP bind address used by `ai-memory serve`. |
| `tcp_keepalive_secs` | `60` (`DEFAULT_TCP_KEEPALIVE_SECS`) | `AI_MEMORY_TCP_KEEPALIVE_SECS` | Idle seconds before TCP keepalive probes start on an accepted connection. 0 disables keepalive. |
| `server_url` | Derived from `bind`; `http://127.0.0.1:49374` for the default bind (`DEFAULT_SERVER_URL`) | `AI_MEMORY_SERVER_URL` | Base URL thin-client commands use to reach the server. |
| `release_base_url` | unset (the GitHub Releases page of `akitaonrails/ai-memory`) | `AI_MEMORY_RELEASE_BASE_URL` | Releases URL `ai-memory upgrade` downloads from, for mirrors and hermetic tests. |
| `base_path` | empty | `AI_MEMORY_BASE_PATH` | URL subpath the server is mounted under, prepended by thin-client commands to `/admin/*` requests. |
| `log_level` | `info` | `AI_MEMORY_LOG_LEVEL` | Default tracing filter. `RUST_LOG` overrides it. |
| `allowed_hosts` | `["localhost", "127.0.0.1", "::1"]` | `AI_MEMORY_ALLOWED_HOSTS` | Host header allowlist for the HTTP server (DNS-rebinding defence). |
| `cors_allow_origins` | `[]` | `AI_MEMORY_CORS_ALLOW_ORIGINS` | Origins allowed to make cross-origin requests to `/api/v1`. Empty means same-origin only. |
| `hook_rate_per_sec` | `0.0` | `AI_MEMORY_HOOK_RATE_PER_SEC` | Hook ingest tokens per second per actor and session source. 0 disables the limiter. |
| `hook_rate_burst` | `0.0` (falls back to the rate, minimum one token, when the limiter is on) | `AI_MEMORY_HOOK_RATE_BURST` | Burst size of the hook ingest limiter. |
| `capture_assistant` | `false` | `AI_MEMORY_CAPTURE_ASSISTANT` | Server side half of the assistant turn capture opt-in on Claude Code `Stop` events. |
| `consolidate_on_session_end` | `false` | `AI_MEMORY_CONSOLIDATE_ON_SESSION_END` | Queue LLM consolidation on SessionEnd when an LLM provider is configured. |
| `backfill_on_start` | `true` | `AI_MEMORY_BACKFILL_ON_START` | Import a project's existing local harness history once, when SessionStart finds its store empty. |
| `run_autowire` | `true` | `AI_MEMORY_RUN_AUTOWIRE` | Install a harness's hooks and MCP server on the first `ai-memory run` launch when they are missing. |
| `claude_true_yolo` | `false` | `AI_MEMORY_CLAUDE_TRUE_YOLO` | Make `ai-memory run --yolo` on Claude force `bypassPermissions` over any settings `defaultMode`. |
| `strip_root_combinators` | `false` | `AI_MEMORY_STRIP_ROOT_COMBINATORS` | Strip root level `anyOf`, `oneOf` and `allOf` from MCP tool input schemas on every `tools/list`. |
| `gemini_safe_schemas` | `false` | `AI_MEMORY_GEMINI_SAFE_SCHEMAS` | Serve MCP tool schemas in the subset Vertex and Gemini accept. Implies `strip_root_combinators`. |
| `reranker` | unset | `AI_MEMORY_RERANKER` | Post-fusion reranker for `memory_query`. The only value is `llm`, which needs an LLM provider. |
| `rerank_timeout_secs` | `20` (`ai_memory_llm::reranker::RERANK_TIMEOUT`) | `AI_MEMORY_RERANK_TIMEOUT_SECS` | Seconds one rerank call may take before `memory_query` keeps the pre-rerank order. Must be at least 1. |
| `llm_provider` | unset | `AI_MEMORY_LLM_PROVIDER` | LLM provider: `anthropic`, `openai`, `gemini`, `openai-compat`, `openai-oauth`, `anthropic-oauth`, `codex`, `copilot`, `opencode` or `cursor`. |
| `llm_model` | unset | `AI_MEMORY_LLM_MODEL` | Model override for the LLM provider. |
| `llm_base_url` | unset | `AI_MEMORY_LLM_BASE_URL` | Endpoint override for the LLM provider. Required for `openai-compat`. |
| `llm_compat_strict` | `true` | `AI_MEMORY_LLM_COMPAT_STRICT` | Send `response_format=json_schema` (strict) to the `openai-compat` provider. |
| `llm_compat_disable_thinking` | `false` | `AI_MEMORY_LLM_COMPAT_DISABLE_THINKING` | Send `chat_template_kwargs: {"enable_thinking": false}` on every `openai-compat` chat request. |
| `llm_compat_stream` | unset | `AI_MEMORY_LLM_COMPAT_STREAM` | Set `false` to request non-streaming JSON from `openai-compat`; `true` is rejected because SSE is unsupported. |
| `llm_timeout_secs` | `300` (`ai_memory_llm::DEFAULT_REQUEST_TIMEOUT_SECS`) | `AI_MEMORY_LLM_TIMEOUT_SECS` | Per-request timeout for chat completions and the Copilot token exchange. The openai-oauth token refresh keeps its own ceiling. |
| `bootstrap_max_concurrent` | `1` | `AI_MEMORY_BOOTSTRAP_MAX_CONCURRENT` | How many live `bootstrap` runs may call the LLM at once across projects. Runs for the same project always wait for each other. Must be at least 1. |
| `llm_reasoning_effort` | unset (the model default applies) | `AI_MEMORY_LLM_REASONING_EFFORT` | Reasoning effort: `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, `ultra` or `persistent`. |
| `llm_headers` | `[]` | `AI_MEMORY_LLM_HEADERS` | Extra HTTP headers on every LLM chat request, as `Name: Value` or `Name=Value` entries. |
| `embedding_provider` | unset (in-process local embeddings) | `AI_MEMORY_EMBEDDING_PROVIDER` | Embedding provider: `none`, `openai`, `voyage`, `google` or `gemini`, or `openai-compat`. |
| `embedding_model` | unset | `AI_MEMORY_EMBEDDING_MODEL` | Provider-specific embedding model override. |
| `embedding_dim` | unset | `AI_MEMORY_EMBEDDING_DIM` | Embedding dimension override. It must match the model. |
| `embedding_base_url` | unset | `AI_MEMORY_EMBEDDING_BASE_URL` | Embedding endpoint override. Required when `embedding_provider` is `openai-compat`. |
| `embedding_query_prefix` | unset | `AI_MEMORY_EMBEDDING_QUERY_PREFIX` | Text prepended to every embedding query for the `openai` and `openai-compat` embedders. Kept verbatim. |
| `embedding_document_prefix` | unset | `AI_MEMORY_EMBEDDING_DOCUMENT_PREFIX` | Text prepended to every page before embedding. Part of the stored embedding identity. |
| `contradiction_band_min` | `0.4` (`DEFAULT_CONTRADICTION_SIM_LOW`) | `AI_MEMORY_CONTRADICTION_BAND_MIN` | Lower edge, inclusive, of the cosine band `memory_lint` uses to flag likely contradictions. |
| `contradiction_band_max` | `0.75` (`DEFAULT_CONTRADICTION_SIM_HIGH`) | `AI_MEMORY_CONTRADICTION_BAND_MAX` | Upper edge, exclusive, of that band. At or above it, two pages count as near-duplicates. |

## `[[llm_fallbacks]]`

An array of tables. Each entry is tried in order after the primary provider, only when a call fails transiently. The default is an empty list. Config load rejects an entry with no provider or model. The list has no environment form.

| Key | Default | Env equivalent | Purpose |
| --- | --- | --- | --- |
| `provider` | empty (required) | - | Provider for this candidate, using the same names as `llm_provider`. |
| `model` | empty (required) | - | Model id for this candidate. |
| `base_url` | unset | - | Endpoint override for this candidate. Required for `openai-compat`. |
| `api_key_env` | unset | - | Name of the environment variable holding this candidate's key. Required unless the provider has a native credential source. |

## `[[admission_webhooks]]`

An array of tables, defined by `WebhookConfig` in `ai-memory-wiki`. The default is an empty list, which attaches no chain.

| Key | Default | Env equivalent | Purpose |
| --- | --- | --- | --- |
| `name` | required | `AI_MEMORY_ADMISSION_WEBHOOKS_JSON` | Stable identifier used in loop-prevention skip lists and log fields. |
| `url` | required | `AI_MEMORY_ADMISSION_WEBHOOKS_JSON` | HTTP endpoint the payload is POSTed to. |
| `timeout_ms` | `2000` | `AI_MEMORY_ADMISSION_WEBHOOKS_JSON` | Per-request timeout in milliseconds. |
| `failure_policy` | `ignore` | `AI_MEMORY_ADMISSION_WEBHOOKS_JSON` | `ignore` logs a warning and continues. `reject` aborts the write. |
| `events` | required | `AI_MEMORY_ADMISSION_WEBHOOKS_JSON` | Operations the webhook subscribes to. An empty list means it never fires. |
| `blocking` | `true` | `AI_MEMORY_ADMISSION_WEBHOOKS_JSON` | `true` runs inside the write path and can change the page. `false` fires after the page lands and ignores the response. |

`AI_MEMORY_ADMISSION_WEBHOOKS_JSON` takes a JSON array of these entries and replaces the list from the file. Valid `events` are `write_page`, `consolidate`, `delete`, `delete_event_ledger`, `purge_project`, `purge_session`, `purge_workspace`, `move_project`, `move_session`, `handoff_begin`, `handoff_accept`, `handoff_cancel`, `message_send`, `message_pop` and `message_cancel`.

## `[run]`

| Key | Default | Env equivalent | Purpose |
| --- | --- | --- | --- |
| `require_server` | `false` | `AI_MEMORY_RUN_REQUIRE_SERVER` | Fail a managed launch when the server is unreachable. The variable can only turn it on. |
| `exit_prompt` | `true` | `AI_MEMORY_RUN_EXIT_PROMPT` | Offer the switch and rerun menu after a successful interactive session. `ai-memory run --no-exit-prompt` skips it for one launch. |
| `profiles.<name>.env` | none | - | Named table of string environment overrides, selected with `ai-memory run --profile <name>`. |

Profile names use 1 to 64 ASCII letters, digits, `.`, `_` or `-`. A config holds at most 128 profiles, each with at most 128 entries. A key is at most 256 bytes and a value at most 64 KiB.

## `[sessions]`

| Key | Default | Env equivalent | Purpose |
| --- | --- | --- | --- |
| `idle_end_after_hours` | `0` (off) | `AI_MEMORY_SESSIONS__IDLE_END_AFTER_HOURS` | Hours of silence after which the maintenance tick ends an open session that never got a SessionEnd. 0 or 1 to 8760. |

## `[decay]`

The defaults for `lambda` through `hard_delete_after_days` come from `ai_memory_store::DecayParams::default()`.

| Key | Default | Env equivalent | Purpose |
| --- | --- | --- | --- |
| `lambda` | `0.02` | `AI_MEMORY_DECAY__LAMBDA` | Per-day decay rate of page retention. |
| `sigma` | `0.6` | `AI_MEMORY_DECAY__SIGMA` | Magnitude of the access reinforcement. |
| `mu` | `0.04` | `AI_MEMORY_DECAY__MU` | Per-day decay of the access reinforcement. |
| `salience_default` | `1.0` | `AI_MEMORY_DECAY__SALIENCE_DEFAULT` | Default page salience. |
| `cold_threshold` | `0.20` | `AI_MEMORY_DECAY__COLD_THRESHOLD` | Retention score below which a page is evicted. |
| `hard_delete_after_days` | `180` | `AI_MEMORY_DECAY__HARD_DELETE_AFTER_DAYS` | Grace period before an evicted version chain is deleted permanently. |
| `breadth_weight` | `0.0` | `AI_MEMORY_DECAY__BREADTH_WEIGHT` | Weight for the number of distinct authenticated readers of a page. |
| `observation_retention_days` | `0` (off) | `AI_MEMORY_DECAY__OBSERVATION_RETENTION_DAYS` | Age past which a consolidated session's raw observations may be pruned. Irreversible. |
| `observation_prune_batch` | `5000` (`DEFAULT_OBSERVATION_PRUNE_BATCH`) | `AI_MEMORY_DECAY__OBSERVATION_PRUNE_BATCH` | Observation rows deleted per prune transaction. |
| `ledger_retention_months` | `0` (off) | `AI_MEMORY_DECAY__LEDGER_RETENTION_MONTHS` | Whole UTC months after a month ends before the sweep deletes its `log-YYYY-MM.md` ledger. |
| `compact_cold_episodic` | `false` | `AI_MEMORY_DECAY__COMPACT_COLD_EPISODIC` | Compact a cold episodic page to its abstract, summary and keep-token set instead of evicting it. |
| `dedup_cold_clusters` | `false` | `AI_MEMORY_DECAY__DEDUP_COLD_CLUSTERS` | Collapse near-duplicate cold episodic pages to one survivor. Needs an embedder. |
| `dedup_min_pts` | `0` (uses 2, `DEFAULT_DEDUP_MIN_PTS`) | `AI_MEMORY_DECAY__DEDUP_MIN_PTS` | DBSCAN density floor for the dedup pass. |
| `dedup_max_eps` | `0.0` (uses 0.15, `DEFAULT_DEDUP_MAX_EPS`) | `AI_MEMORY_DECAY__DEDUP_MAX_EPS` | Ceiling on the adaptive eps, as cosine distance. Lower merges less. |

## `[decay.half_life_days]`

Per-tier half-lives in days. A tier that sets one decays at `ln(2) / days`. A tier that leaves it unset keeps `decay.lambda`. Each value must be finite and above zero.

| Key | Default | Env equivalent | Purpose |
| --- | --- | --- | --- |
| `working` | unset | `AI_MEMORY_DECAY__HALF_LIFE_DAYS__WORKING` | Half-life for `working` tier pages. |
| `episodic` | unset | `AI_MEMORY_DECAY__HALF_LIFE_DAYS__EPISODIC` | Half-life for `episodic` tier pages. |
| `semantic` | unset | `AI_MEMORY_DECAY__HALF_LIFE_DAYS__SEMANTIC` | Half-life for `semantic` tier pages. |
| `procedural` | unset | `AI_MEMORY_DECAY__HALF_LIFE_DAYS__PROCEDURAL` | Half-life for `procedural` tier pages. |

## `[maintenance]`

| Key | Default | Env equivalent | Purpose |
| --- | --- | --- | --- |
| `enabled` | `true` | `AI_MEMORY_MAINTENANCE__ENABLED` | Master switch for scheduled server jobs. |
| `forget_sweep_interval_secs` | `86400` | `AI_MEMORY_MAINTENANCE__FORGET_SWEEP_INTERVAL_SECS` | Interval of the retention forget sweep. 0 disables the job. |
| `lint_interval_secs` | `86400` | `AI_MEMORY_MAINTENANCE__LINT_INTERVAL_SECS` | Interval of rule-based wiki lint. 0 disables the job. |
| `embedding_backfill_interval_secs` | `0` (off) | `AI_MEMORY_MAINTENANCE__EMBEDDING_BACKFILL_INTERVAL_SECS` | Interval of embedding backfill. Off by default because it may call a paid provider. |
| `reconcile_tombstones_deleted_pages` | `false` | `AI_MEMORY_MAINTENANCE__RECONCILE_TOMBSTONES_DELETED_PAGES` | Let the watcher tombstone an OKF-imported page whose file stayed missing on two consecutive passes. |

## `[dream]`

A zero in a numeric key selects the built-in default named in the Default cell.

| Key | Default | Env equivalent | Purpose |
| --- | --- | --- | --- |
| `enabled` | `false` | `AI_MEMORY_DREAM__ENABLED` | Master switch for the LLM pass that rewrites and merges clusters of cold pages. |
| `interval_secs` | `3600` | `AI_MEMORY_DREAM__INTERVAL_SECS` | How often the scheduler considers a run. 0 uses 3600. |
| `idle_window_secs` | `0` (uses 300, `DEFAULT_DREAM_IDLE_WINDOW_SECS`) | `AI_MEMORY_DREAM__IDLE_WINDOW_SECS` | Quiet seconds required before a run starts, and after which returning activity cancels it. |
| `min_pts` | `0` (uses 2, `DEFAULT_DEDUP_MIN_PTS`) | `AI_MEMORY_DREAM__MIN_PTS` | DBSCAN density floor. |
| `max_eps` | `0.0` (uses 0.15, `DEFAULT_DEDUP_MAX_EPS`) | `AI_MEMORY_DREAM__MAX_EPS` | Ceiling on eps, as cosine distance. Lower merges less. |
| `max_clusters_per_run` | `0` (uses 8, `DEFAULT_DREAM_MAX_CLUSTERS_PER_RUN`) | `AI_MEMORY_DREAM__MAX_CLUSTERS_PER_RUN` | Clusters rewritten in one run. |
| `min_cold_pages` | `0` (uses 2, `DEFAULT_DREAM_MIN_COLD_PAGES`) | `AI_MEMORY_DREAM__MIN_COLD_PAGES` | Cold pages needed before a run does work. |

## `[retrieval]`

The defaults come from `ai_memory_store::RetrievalTuning::default()`.

| Key | Default | Env equivalent | Purpose |
| --- | --- | --- | --- |
| `query_intent` | `false` | `AI_MEMORY_RETRIEVAL__QUERY_INTENT` | Route queries that ask for a past session so session pages skip the kind and tier authority penalty they normally pay. |
| `session_recall_bonus` | `0.25` | `AI_MEMORY_RETRIEVAL__SESSION_RECALL_BONUS` | Extra authority for session pages when the routing fires, on top of cancelling that penalty. |
| `abstract_vectors` | `false` | `AI_MEMORY_RETRIEVAL__ABSTRACT_VECTORS` | Add the L0 abstract embedding stream to the rank fusion. |
| `belief_authority_weight` | `0.0` | `AI_MEMORY_RETRIEVAL__BELIEF_AUTHORITY_WEIGHT` | Weight of a page's evidence-derived confidence in its authority. 0 is inert and negatives clamp to 0. |
| `observation_stream` | `true` | `AI_MEMORY_RETRIEVAL__OBSERVATION_STREAM` | Add the observation evidence stream, where matching observation bodies vote for their session's page. |

## `[search.fts]`

| Key | Default | Env equivalent | Purpose |
| --- | --- | --- | --- |
| `stopwords` | unset (the built-in English list) | `AI_MEMORY_SEARCH_FTS_STOPWORDS` (comma separated, blank ignored) | Words dropped from a bare natural-language query. `[]` turns the filter off. A list replaces the built-in one. |

## `[query_answer]`

| Key | Default | Env equivalent | Purpose |
| --- | --- | --- | --- |
| `max_context_tokens` | unset (no limit) | `AI_MEMORY_QUERY_ANSWER__MAX_CONTEXT_TOKENS` | Estimated token budget for the evidence `memory_query(answer=true)` sends to the model. Each UTF-8 byte counts as one token. Omitted or clipped evidence is reported in `answer_context`, and zero or a too-small value prevents synthesis. |

## `[slots]`

| Key | Default | Env equivalent | Purpose |
| --- | --- | --- | --- |
| `per_user` | `false` | `AI_MEMORY_SLOTS__PER_USER` | Namespace engine-written slots under the operator who produced them and inject only shared plus own slots. |

## `[consolidation]`

| Key | Default | Env equivalent | Purpose |
| --- | --- | --- | --- |
| `verbatim_residue` | `false` | `AI_MEMORY_CONSOLIDATION__VERBATIM_RESIDUE` | Add bounded literal excerpts of observations the normal projection omitted or truncated. |
| `max_input_tokens` | `100000` (`DEFAULT_CONSOLIDATION_MAX_INPUT_TOKENS`) | `AI_MEMORY_CONSOLIDATION__MAX_INPUT_TOKENS` | Approximate token target for the whole consolidation prompt. |
| `max_output_tokens` | `32000` (`DEFAULT_CONSOLIDATION_MAX_OUTPUT_TOKENS`) | `AI_MEMORY_CONSOLIDATION__MAX_OUTPUT_TOKENS` | Maximum tokens the provider may generate for one consolidation response. |
| `input_token_safety_margin` | `0.8` (`DEFAULT_CONSOLIDATION_INPUT_TOKEN_SAFETY_MARGIN`) | `AI_MEMORY_CONSOLIDATION__INPUT_TOKEN_SAFETY_MARGIN` | Multiplier on the input budget, above 0 and at most 1. |

## `[auto_improve]`

The defaults named `DEFAULT_AUTO_IMPROVE_*` are constants in `ai-memory-consolidate`.

| Key | Default | Env equivalent | Purpose |
| --- | --- | --- | --- |
| `require_approval` | `false` | `AI_MEMORY_AUTO_IMPROVE__REQUIRE_APPROVAL` | Keep proposals pending until an admin approves them. |
| `on_session_end` | `false` | `AI_MEMORY_AUTO_IMPROVE__ON_SESSION_END` | Schedule a reviewer run on SessionEnd. |
| `min_observations` | `8` (`DEFAULT_AUTO_IMPROVE_MIN_OBSERVATIONS`) | `AI_MEMORY_AUTO_IMPROVE__MIN_OBSERVATIONS` | Observations a session needs before it is reviewed. |
| `min_session_duration_secs` | `120` (`DEFAULT_AUTO_IMPROVE_MIN_SESSION_DURATION_SECS`) | `AI_MEMORY_AUTO_IMPROVE__MIN_SESSION_DURATION_SECS` | Minimum span between first and last observation before review. |
| `min_confidence` | `0.75` (`DEFAULT_AUTO_IMPROVE_MIN_CONFIDENCE`) | `AI_MEMORY_AUTO_IMPROVE__MIN_CONFIDENCE` | Lowest model confidence validation accepts. |
| `max_input_tokens` | `24000` (`DEFAULT_AUTO_IMPROVE_MAX_INPUT_TOKENS`) | `AI_MEMORY_AUTO_IMPROVE__MAX_INPUT_TOKENS` | Approximate prompt budget for one review. |
| `max_proposals_per_run` | `5` (`DEFAULT_AUTO_IMPROVE_MAX_PROPOSALS`) | `AI_MEMORY_AUTO_IMPROVE__MAX_PROPOSALS_PER_RUN` | Validated proposals returned from one run. |
| `max_patchable_pages` | `8` (`DEFAULT_AUTO_IMPROVE_MAX_PATCHABLE_PAGES`) | `AI_MEMORY_AUTO_IMPROVE__MAX_PATCHABLE_PAGES` | Existing pages included as context for patch proposals. |
| `patchable_page_prefixes` | `["_rules/", "procedures/"]` (`DEFAULT_AUTO_IMPROVE_PATCHABLE_PAGE_PREFIXES`) | `AI_MEMORY_AUTO_IMPROVE__PATCHABLE_PAGE_PREFIXES` | Wiki folder prefixes whose page bodies the reviewer may read. |
| `max_patchable_body_chars` | `8000` (`DEFAULT_AUTO_IMPROVE_MAX_PATCHABLE_BODY_CHARS`) | `AI_MEMORY_AUTO_IMPROVE__MAX_PATCHABLE_BODY_CHARS` | Body characters rendered per patchable page. |
| `max_edits_per_proposal` | `5` (`DEFAULT_AUTO_IMPROVE_MAX_EDITS_PER_PROPOSAL`) | `AI_MEMORY_AUTO_IMPROVE__MAX_EDITS_PER_PROPOSAL` | Patch edits in one proposal. |
| `max_edit_content_chars` | `4000` (`DEFAULT_AUTO_IMPROVE_MAX_EDIT_CONTENT_CHARS`) | `AI_MEMORY_AUTO_IMPROVE__MAX_EDIT_CONTENT_CHARS` | Content characters in one patch edit. |
| `max_changed_chars_per_proposal` | `12000` (`DEFAULT_AUTO_IMPROVE_MAX_CHANGED_CHARS_PER_PROPOSAL`) | `AI_MEMORY_AUTO_IMPROVE__MAX_CHANGED_CHARS_PER_PROPOSAL` | Aggregate changed characters in one patch proposal. |
| `max_patch_edits_per_run` | `8` (`DEFAULT_AUTO_IMPROVE_MAX_PATCH_EDITS_PER_RUN`) | `AI_MEMORY_AUTO_IMPROVE__MAX_PATCH_EDITS_PER_RUN` | Patch edits accepted across one review run. |
| `max_rejection_context` | `50` (`DEFAULT_AUTO_IMPROVE_MAX_REJECTION_CONTEXT`) | `AI_MEMORY_AUTO_IMPROVE__MAX_REJECTION_CONTEXT` | Recent rejection-buffer entries rendered into the prompt. |
| `rejection_context_days` | `180` (`DEFAULT_AUTO_IMPROVE_REJECTION_CONTEXT_DAYS`) | `AI_MEMORY_AUTO_IMPROVE__REJECTION_CONTEXT_DAYS` | Maximum age in days of a rejection-buffer entry in the prompt. |
| `max_final_body_chars` | `32000` (`DEFAULT_AUTO_IMPROVE_MAX_FINAL_BODY_CHARS`) | `AI_MEMORY_AUTO_IMPROVE__MAX_FINAL_BODY_CHARS` | Largest materialized body a proposal may produce. Code caps it at 32000, so a higher value has no effect. |
| `max_rule_page_tokens` | `2000` (`DEFAULT_AUTO_IMPROVE_MAX_RULE_PAGE_TOKENS`) | `AI_MEMORY_AUTO_IMPROVE__MAX_RULE_PAGE_TOKENS` | Approximate tokens allowed in one `_rules/` page. |
| `max_procedure_page_tokens` | `2000` (`DEFAULT_AUTO_IMPROVE_MAX_PROCEDURE_PAGE_TOKENS`) | `AI_MEMORY_AUTO_IMPROVE__MAX_PROCEDURE_PAGE_TOKENS` | Approximate tokens allowed in one `procedures/` page. |
| `include_raw_fallback` | `false` | `AI_MEMORY_AUTO_IMPROVE__INCLUDE_RAW_FALLBACK` | Let the reviewer include raw observation fallback details. |
| `proposal_actor` | `auto_improve` (`DEFAULT_AUTO_IMPROVE_PROPOSAL_ACTOR`) | `AI_MEMORY_AUTO_IMPROVE__PROPOSAL_ACTOR` | Synthetic actor recorded as the author of autonomous proposals. |
| `pending_path` | `_pending/auto-improve` (`DEFAULT_AUTO_IMPROVE_PENDING_PATH`) | `AI_MEMORY_AUTO_IMPROVE__PENDING_PATH` | Wiki-relative folder for non-indexed pending proposal files. |

## `[auto_improve.eval]`

| Key | Default | Env equivalent | Purpose |
| --- | --- | --- | --- |
| `enabled` | `false` | `AI_MEMORY_AUTO_IMPROVE__EVAL__ENABLED` | Turn on the proposal evaluation gate. |
| `command` | empty | `AI_MEMORY_AUTO_IMPROVE__EVAL__COMMAND` | Executable plus whitespace-separated arguments, run directly without a shell. |
| `builtin` | empty | `AI_MEMORY_AUTO_IMPROVE__EVAL__BUILTIN` | Scorer run in process instead of `command`. The only value is `structure`. Set `builtin` or `command`, not both. |
| `timeout_secs` | `120` | `AI_MEMORY_AUTO_IMPROVE__EVAL__TIMEOUT_SECS` | Timeout per proposal eval command. |
| `targets` | `["_rules", "procedures"]` (`default_auto_improve_eval_targets`) | `AI_MEMORY_AUTO_IMPROVE__EVAL__TARGETS` | Wiki path prefixes that require the eval when it is enabled. |
| `min_delta` | `0.0` | `AI_MEMORY_AUTO_IMPROVE__EVAL__MIN_DELTA` | Required `score_after` minus `score_before` when scores are present. |

## `[auto_improve.scheduler]`

| Key | Default | Env equivalent | Purpose |
| --- | --- | --- | --- |
| `enabled` | `true` | `AI_MEMORY_AUTO_IMPROVE__SCHEDULER__ENABLED` | Review newly completed sessions in the background. |
| `interval_secs` | `3600` | `AI_MEMORY_AUTO_IMPROVE__SCHEDULER__INTERVAL_SECS` | Scheduler cadence. 0 disables the scheduler and keeps manual runs. |
| `max_sessions_per_tick` | `1` | `AI_MEMORY_AUTO_IMPROVE__SCHEDULER__MAX_SESSIONS_PER_TICK` | Sessions reviewed per project in one tick. 0 disables the scheduler. |
| `min_session_age_secs` | `600` | `AI_MEMORY_AUTO_IMPROVE__SCHEDULER__MIN_SESSION_AGE_SECS` | Minimum age after SessionEnd before a session is eligible. |
| `experience_every_sessions` | `0` (off) | `AI_MEMORY_AUTO_IMPROVE__SCHEDULER__EXPERIENCE_EVERY_SESSIONS` | Run the cross-session experience pass after this many new completed sessions per project. |
| `experience_sessions` | `10` | `AI_MEMORY_AUTO_IMPROVE__SCHEDULER__EXPERIENCE_SESSIONS` | Recent session summary pages one experience pass reads. |

## `[auto_improve.scheduler.experience_entropy_filter]`

Defined by `EntropyFilterConfig` in `ai-memory-consolidate`. A skipped page is left alone and never deleted.

| Key | Default | Env equivalent | Purpose |
| --- | --- | --- | --- |
| `enabled` | `false` | `AI_MEMORY_AUTO_IMPROVE__SCHEDULER__EXPERIENCE_ENTROPY_FILTER__ENABLED` | Skip low-information session pages in the experience pass. |
| `min_chars` | `16` | `AI_MEMORY_AUTO_IMPROVE__SCHEDULER__EXPERIENCE_ENTROPY_FILTER__MIN_CHARS` | Minimum non-whitespace characters. Shorter text is skipped as near-empty. |
| `min_entropy_bits_per_char` | `2.0` | `AI_MEMORY_AUTO_IMPROVE__SCHEDULER__EXPERIENCE_ENTROPY_FILTER__MIN_ENTROPY_BITS_PER_CHAR` | Minimum Shannon entropy per character. Finite and at least 0. |
| `max_repetition_ratio` | `0.7` | `AI_MEMORY_AUTO_IMPROVE__SCHEDULER__EXPERIENCE_ENTROPY_FILTER__MAX_REPETITION_RATIO` | Repetition ratio (`1 - distinct_tokens / total_tokens`) above which text is skipped. 0.0 to 1.0. |
| `repetition_min_tokens` | `6` | `AI_MEMORY_AUTO_IMPROVE__SCHEDULER__EXPERIENCE_ENTROPY_FILTER__REPETITION_MIN_TOKENS` | Token count a body needs before the repetition check applies. |

## `[handoff]`

| Key | Default | Env equivalent | Purpose |
| --- | --- | --- | --- |
| `claim_on_session_start` | `true` | `AI_MEMORY_HANDOFF__CLAIM_ON_SESSION_START` | Claim a pending handoff automatically at SessionStart. `false` renders a notice and waits for `memory_handoff_accept`. |
| `create_on_session_end` | `true` | `AI_MEMORY_HANDOFF__CREATE_ON_SESSION_END` | Create an automatic open handoff when an unmanaged session ends. |

## `[profile]`

Defined by `ProfileSettings` in `ai-memory-core`.

| Key | Default | Env equivalent | Purpose |
| --- | --- | --- | --- |
| `enabled` | `auto` | `AI_MEMORY_PROFILE__ENABLED` | `auto`, `true` or `false`. Auto turns the profile on for a single-operator server and off for a multi-user one. |
| `share` | `auto` | `AI_MEMORY_PROFILE__SHARE` | Where the profile lives: `auto`, `global`, `workspace`, `user` or `off`. |
| `min_projects` | `2` (`DEFAULT_MIN_PROJECTS`) | `AI_MEMORY_PROFILE__MIN_PROJECTS` | Distinct projects a choice must appear in to become an entry. |
| `inject_on_session_start` | `true` | `AI_MEMORY_PROFILE__INJECT_ON_SESSION_START` | Deliver the profile digest at SessionStart. |
| `digest_max_bytes` | `3000` (`DEFAULT_DIGEST_MAX_BYTES`) | `AI_MEMORY_PROFILE__DIGEST_MAX_BYTES` | Digest budget in UTF-8 bytes, clamped to 1500 through 12000. |
| `baseline_max_bytes` | `6000` (`DEFAULT_BASELINE_MAX_BYTES`) | `AI_MEMORY_PROFILE__BASELINE_MAX_BYTES` | Digest budget for a project with no pages yet, clamped to 2000 through 20000. |
| `apply_max_lines` | `40` (`DEFAULT_APPLY_MAX_LINES`) | `AI_MEMORY_PROFILE__APPLY_MAX_LINES` | Most lines `ai-memory profile apply` writes into a rules file. |
| `max_output_tokens` | `2000` (`DEFAULT_MAX_OUTPUT_TOKENS`) | `AI_MEMORY_PROFILE__MAX_OUTPUT_TOKENS` | Output-token allowance for one profile LLM call (classification and merge). Raise it when the provider spends part of the budget before the structured JSON, e.g. a thinking model, and the call fails with `finish_reason=length`. |
| `llm` | `true` | `AI_MEMORY_PROFILE__LLM` | Use the configured LLM provider for the profile when there is one. |

## `[sanitize]`

Defined by `SanitizeConfig` in `ai-memory-core`. Built-in redaction patterns always run.

| Key | Default | Env equivalent | Purpose |
| --- | --- | --- | --- |
| `extra_patterns` | `[]` | `AI_MEMORY_SANITIZE__EXTRA_PATTERNS` | Additional regex patterns to redact. An invalid regex aborts startup. |
| `allowlist` | `[]` | `AI_MEMORY_SANITIZE__ALLOWLIST` | Substrings that are never redacted, even when a pattern matches them. Blank entries are dropped. |

## `[ingest]`

| Key | Default | Env equivalent | Purpose |
| --- | --- | --- | --- |
| `drop_tool_use_id_prefixes` | `[]` | `AI_MEMORY_INGEST__DROP_TOOL_USE_ID_PREFIXES` | Drop a tool event whose `tool_use_id` starts with one of these. |
| `drop_tool_names` | `[]` | `AI_MEMORY_INGEST__DROP_TOOL_NAMES` | Drop a tool event whose tool name equals one of these exactly. |
| `drop_events` | `[]` | `AI_MEMORY_INGEST__DROP_EVENTS` | Drop whole hook event kinds. `session-start` and `session-end` are rejected at load. |
| `record_tool_names` | `false` | `AI_MEMORY_INGEST__RECORD_TOOL_NAMES` | Name each tool call in its observation (`tool non-file: Bash`, plus a Claude Code skill or subagent type) instead of only its family. Never a tool's arguments. |

## `[hook_spool]`

| Key | Default | Env equivalent | Purpose |
| --- | --- | --- | --- |
| `max_attempts` | `8` (`DEFAULT_HOOK_SPOOL_MAX_ATTEMPTS`) | `AI_MEMORY_HOOK_SPOOL_MAX_ATTEMPTS` | Failed drain passes before a spooled hook event is dropped. 0 disables the limit. |

## `[routing]`

| Key | Default | Env equivalent | Purpose |
| --- | --- | --- | --- |
| `mid_session` | `follow-cwd` | `AI_MEMORY_ROUTING__MID_SESSION` | Attribution of a mid-session event after the cwd moved: `follow-cwd` or `sticky`. |

## `[auto_scope]`

| Key | Default | Env equivalent | Purpose |
| --- | --- | --- | --- |
| `mode` | `per_actor` | `AI_MEMORY_AUTO_SCOPE__MODE` | Keying of the active-project pointer: `per_actor`, `per_session` or `single`. |
| `session_ttl_secs` | `3600` (`DEFAULT_PER_KEY_TTL`) | `AI_MEMORY_AUTO_SCOPE__SESSION_TTL_SECS` | Lifetime of a per-key entry in the `per_session` and `per_actor` modes. 0 uses the default. |
| `max_entries` | `4096` (`DEFAULT_MAX_ENTRIES`) | `AI_MEMORY_AUTO_SCOPE__MAX_ENTRIES` | Cap on the per-key map, evicting the oldest insertions first. |

## `[auth]`

Keys that `ai-memory init` writes or generates are marked in the Purpose column. Prefer the environment variables for secrets.

| Key | Default | Env equivalent | Purpose |
| --- | --- | --- | --- |
| `bearer_token` | unset | `AI_MEMORY_AUTH_TOKEN` | Shared bearer token required on HTTP routes. Generate one with `ai-memory generate-auth-token`. |
| `bearer_token_file` | unset | `AI_MEMORY_AUTH__BEARER_TOKEN_FILE` | Owner-only file holding the shared bearer token. Managed by `ai-memory init` and `ai-memory auth enable`, which set `auth-tokens/default`. |
| `new_projects_restricted` | `false` | `AI_MEMORY_AUTH__NEW_PROJECTS_RESTRICTED` | Create new projects as `restricted` instead of `open`. |
| `secure_cookie` | `false` | `AI_MEMORY_AUTH__SECURE_COOKIE` | Mark the browser session cookie `Secure`, for `/web` behind an HTTPS-terminating proxy. |
| `root_username` | unset | `AI_MEMORY_AUTH__ROOT_USERNAME` | Username attributed to writes authenticated by the bearer token. |
| `root_issuer` | unset | `AI_MEMORY_AUTH__ROOT_ISSUER` | OIDC issuer of the root operator behind a trusted proxy. Set together with `root_subject`. |
| `root_subject` | unset | `AI_MEMORY_AUTH__ROOT_SUBJECT` | OIDC subject of the root operator. Set together with `root_issuer`. |
| `root_email` | unset | `AI_MEMORY_AUTH__ROOT_EMAIL` | Email shown for the root user in the web UI and `/api/v1`. |
| `root_name` | unset | `AI_MEMORY_AUTH__ROOT_NAME` | Display name of the root user. Falls back to `root_username`. |
| `token_pepper` | unset | `AI_MEMORY_AUTH__TOKEN_PEPPER` | Per-server pepper for native API key hashes. Generated by `ai-memory init`. Do not change it after the first key is added. |
| `actor_proxy_bearer_token` | unset | `AI_MEMORY_AUTH__ACTOR_PROXY_BEARER_TOKEN` | Bearer token for a trusted authenticating proxy that names the real user in `X-Memory-Actor-*` headers. |
| `initial_root_password` | unset | `AI_MEMORY_AUTH__INITIAL_ROOT_PASSWORD` | One-shot root password consumed on greenfield bootstrap. Set it through the environment. |
| `recovery_token` | unset | `AI_MEMORY_AUTH__RECOVERY_TOKEN` | Break-glass secret for `POST /auth/recovery`. Set it through the environment. |
| `trusted_proxy_cidrs` | `[]` | `AI_MEMORY_AUTH__TRUSTED_PROXY_CIDRS` | CIDRs allowed to supply `X-Forwarded-For` for login rate limiting. |
