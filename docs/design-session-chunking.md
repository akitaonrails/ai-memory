# Design: processing sessions in chunks

**Status: implemented, with a verified local smoke run.** Processing is asynchronous and durable. It requires explicit activation through configuration and is off by default. Section 13 records the completed checks. The initial settings passed prompt-envelope validation and checks with a controlled provider; cost and fidelity with a real model were not measured.

## 1. Goal and limits

Process the full body of a session's stored observations in bounded LLM calls, combine the results, and publish through the current consolidation or proposal approval path. This covers single-page and multi-page consolidation and the auto-improve session review.

The guarantee is coverage of the stored input: every text range in the snapshot must take part in a completed call with a valid structured response. This does not guarantee that the model's output preserves every fact. The combination step is still a synthesis and must be evaluated for omissions, contradictions and inventions.

The limits that apply before observations are read remain in place. Content excluded by the capture policy, sanitized, or truncated before persistence cannot be recovered by the chunker. This work does not raise capture limits, does not read checkout files to complete observations, and does not change ingestion privacy.

The scope excludes automatic model switching, changes to the Ollama runner, a provider per task, and chunking for bootstrap, dream, experience or profile classification. Those tasks participate in shared server-side call admission when needed.

There is no approved PRD or ADR for this initiative. This document records the implemented design and its limits; it does not replace the existing security contracts.

## 2. Diagnosis of the previous code

The points below motivated the design. With the flag off, these paths keep running as the legacy flow.

- `crates/ai-memory-consolidate/src/consolidator.rs`: `consolidate_session` and `consolidate_session_multi` make one logical call per run, subject to retries. The projection limits the selection to 256 observations and the body to 3,000 characters per observation.
- `crates/ai-memory-consolidate/src/auto_improve.rs`: `run_auto_improve_review` selects observations, limits bodies, and calls validation and eval before handing a report to staging. `input_budget_sampled` reports a partial selection.
- `crates/ai-memory-consolidate/src/projection.rs`: score-based selection, budget-based dropping and final truncation are separate steps. Raising the budget alone does not remove the other cuts.
- `crates/ai-memory-store/src/reader.rs`: `SessionObservationSnapshot` holds a transaction for the duration of a synchronous closure. That transaction must not stay open while waiting for the LLM.
- `crates/ai-memory-store/src/session_consolidation.rs`: legacy generation tracking uses an observation count. A count does not identify a snapshot's content or detect every combination of removal and insertion.
- `crates/ai-memory-consolidate/src/bootstrap.rs` and `V60__bootstrap_chunk_progress.sql`: there is precedent for sequential processing and checkpoints. The storage is specific to bootstrap and lacks the links to session and scope that are needed here.
- `crates/ai-memory-cli/src/commands/serve.rs`: separate workers share the provider. The existing consolidation worker drains pending jobs even without new SessionEnd enqueues.

In the logs examined during the diagnosis, a consolidation with 89,816 tokens was still processing its input close to the 300-second timeout. An auto-improve run started processing its prompt close to the end of its own deadline, after waiting for the runner. Chunking has to control both the size of the calls and their admission.

Design references: [architecture](ARCHITECTURE.md), [auto-improve](auto-improvement-loop.md), [configuration limits](config-properties.md), [security](security-boundaries.md), [lifecycle operations](lifecycle-ops.md), [verbatim residue](design-verbatim-residue.md).

## 3. Activation and compatibility

Public configuration, in `ChunkingSettings` (`ai-memory-core`):

```toml
[chunking]
enabled = false
max_input_tokens = 8192
max_calls_per_run = 128
```

- `enabled` is the activation key for chunked consolidation and auto-improve processing. There is no implicit activation based on session size, timeout or provider error.
- `max_input_tokens` is the approximate budget of one chunked-mode call, including instructions, schema and context. The effective payload also respects the operation's own limit. The 8,192-token value applies to the compact intermediate prompts; with the flag on, loading rejects values below 2,048.
- `max_calls_per_run` limits the total number of provider call attempts, including map, reduce, final and retries, in one logical run. The counter is persisted on the job; a restart or retry does not reset it. The value 128 still needs cost and scale validation. Reaching the limit produces the `budget_exceeded` state, never success with partial coverage. With the flag on, loading rejects `0`. A new equivalent request with a larger `max_calls` requeues the `budget_exceeded` job; the limit is not part of the equivalence key.
- With `enabled = false`, the other two keys are not validated, so a stale value does not break an installation that never opted into the mode.

When enabled, the server validates the full path (map, reduce and the final envelope of each operation) against the budget and fails at startup naming the key that has to change. The final envelope uses its own output ceiling of 8,192 tokens; it does not inherit the 32,000 of the legacy consolidation or the 8,350 floor of the legacy builder.

With the flag absent or `false`:

1. The current paths keep working as before, including selection/truncation, prompts, return values, dry-run, error handling and cadence.
2. No new job is created, the worker does not start, and shared admission is not installed on the provider path.
3. The new asynchronous operations are neither advertised nor accepted. Direct start calls fail explicitly (404 on admin, a tool error on MCP, a disabled state on the CLI); there is no silent fallback to the synchronous flow.
4. The defaults of `[consolidation]` and `[auto_improve]` and the current interface of their clients stay as they are.

With the flag on, `memory_consolidate`, `memory_auto_improve`, the SessionEnd enqueue and the auto-improve scheduler all run through the same durable job, keeping the current response format. The synchronous call drives the job in its own task and, if another worker holds the lease, polls the state every second. Dry-runs stay on the legacy path, which does not call the model. The `[consolidation]` and `[auto_improve]` limits act as per-call ceilings and do not discard the rest of the session. The dedicated budget prevents an installation with `consolidation.max_input_tokens = 100000` from building chunks of that size.

When the server restarts with the flag off, it pauses every active job (`queued`, `running`, `reducing`, `publishing`) and stores the previous phase in `paused_from`. Nothing is published during the pause. If final publication had already started, confirmed effects and IDs are preserved. Resumption handles only the pending tail, with no atomic rollback. Jobs are never converted to the legacy flow. When the flag is turned on again, the worker resumes paused jobs whose fingerprints remain compatible. State lookup stays available for any existing job.

While an equivalent unfinished job (`queued`, `running`, `reducing`, `publishing`, `paused` or `budget_exceeded`) exists for the same session, operation and owner bucket, the legacy path refuses to run. The synchronous call returns an explicit error (409 on admin), the SessionEnd enqueue defers the generation with the same message, and no job ID replaces the result. Without an equivalent job, the legacy path runs as before. Retention cleanup removes jobs idle for more than 7 days, and only then does the legacy path apply again to that session.

The keys belong to the single `Config::load()` path; changes require a server restart, with no hot reload. TOML, env, defaults and validations are in the template and in `docs/config-properties.md`. The example does not authorize changing the operator's local configuration.

## 4. Manifest and chunk planning

### 4.1 Durable snapshot

A short, scoped read identifies the job's observations. The manifest (`processing_job_observations`) stores the order, ID, size and SHA-256 of each observation body, without copying the body. The job identity (`equivalence_key`) covers scope, session, operation, owner bucket, options, and the context and manifest fingerprints. It does not use `COUNT(*)`, a timestamp or a maximum rowid.

No read transaction stays open during network calls. Bodies are re-read through `read_processing_sources`, which checks identity, scope and hash against the manifest. The read goes through the existing scope guards.

Observations that arrive after the snapshot belong to another job generation. V97 triggers on `observations` invalidate, in the same transaction, any active job whose observation changed body, title, type, time or scope, or was removed, discarding its nodes and manifest. Session or project purge, move and merge also invalidate the job in the old scope within the operation's transaction; finished jobs follow the session with their results. Pruning of raw observations excludes sessions with an unfinished job. An explicit purge takes precedence and removes the derived data.

### 4.2 Deterministic coverage

The planner walks every observation in the manifest. It does not call the top-k selector and does not cap the count at 256, 48 or 72 observations.

Whole observations are included when they fit. An observation larger than the available space becomes fragments (`Span`) with a source ordinal and valid UTF-8 offsets, cut at a paragraph, line or space boundary and, failing those, at a character boundary. No remainder of a body is omitted. There is no overlap: each byte belongs to exactly one primary range.

The coverage ledger is maintained by the server from completed map nodes. Completion requires the full union of the manifest's primary ranges, with no gaps. The model does not decide or certify its own coverage.

### 4.3 Budget

Instructions, the JSON schema, delimiters, page context and repeated evidence all count against the budget. Output is reserved separately. Map, reduce and final have their own compact prompts and schemas, charged to `max_input_tokens` before any evidence is admitted, and output ceilings that are also bounded by the operation.

The estimate is conservative: 3 characters per token, with the safety margin from `[consolidation]`, and each non-ASCII character costs 3. It is not any provider's tokenizer. If not even a minimal input with its envelope fits, validation rejects the configuration; a call without evidence is never sent.

An exceeded-context error, a truncated response or invalid output does not allow dropping text or repeating the same payload. The node becomes `recoverable` and is subdivided (map) or regrouped (reduce). On the final call, the available space is reduced up to 3 times; after that the job fails.

## 5. Extraction and combination

### 5.1 Map

Each chunk produces a compact JSON digest: items (`fact`, `decision`, `gotcha`, `rule`, `procedure`, `open`, `correction`) with references to observations and fragments, short quotes, supersessions and conflicts. The digest is not a proposal or a page.

The model treats observations, file names and tool text as untrusted data. The server validates references against the manifest, checks quotes against the evidence shown, and sanitizes the digest before writing the node. A digest holds at most 12 items and 4 conflicts; responses above those ceilings are rejected for subdivision. Items without a valid reference are dropped and counted, and unsupported quotes are removed. The raw provider response and unbounded copies of the prompts are not persisted.

Both `items` and `conflicts` are required, and unknown fields are rejected. An explicit empty digest is valid; an unrelated JSON object is an error, not completed coverage. Every chunk prompt includes its output contract so providers without strict schema support receive the same field names.

The final auto-improve response likewise requires `summary`, `proposals` and `rejected_candidates`, with no unknown top-level fields. Explicit empty lists are valid. A malformed object cannot complete the job as an empty staged review.

A result truncated by the output limit does not count as a completed chunk. The node becomes `recoverable` and is subdivided, without being validated as empty and without repeating the identical call.

### 5.2 Reduce

Digests are combined in groups that respect the same budget, over as many levels as needed. No prompt concatenates all the intermediates. Source and temporal references are preserved, and a late correction can replace an earlier conclusion. Unresolved conflicts stay explicit. The tests evaluate this property without promising that a prompt instruction guarantees fidelity.

If a reducer cannot reduce enough to make progress, the state machine stops with an error instead of recursing without bound, preserving nodes and coverage.

### 5.3 Publication

In consolidation, only the final call produces the existing single-page or multi-page contracts. Publication goes through `Wiki::write_page` or `Wiki::apply_batch` with the existing attribution, sanitization, admission, supersession, indexing and recovery. The base version of each page is captured with the context the model saw; a path outside that context can only be created, never replaced. The context is recomputed after the call. If it has changed, the step repeats with fresh context, at most 3 times.

In auto-improve, the candidates from all chunks are gathered before the final call, which produces the complete report once. Validation, limits (`max_proposals_per_run`, edit budget) and eval run over the final set, once per evaluated proposal. Concurrent changes still produce a conflict, with no last-write-wins. A chunk does not create an `auto_improve_runs` row.

Compatible patches for the same target are combined before the global limits and eval; incompatible candidates are recorded as rejected. Eval uses the base bodies captured with the context. Before running an external command, the server writes a marker; afterward, it writes the validated report and the results. A resume reuses that result. If only the marker exists, the job fails explicitly: the command may have run and is not repeated automatically.

Full-page current-focus proposals carry the body hash captured for the final review and eval. Staging compares it with the live target in the writer transaction; a replaced or deleted base is skipped. If the slot was absent in the captured context, the proposal stays a create even if another writer adds the page before staging.

The publication plan (pages, or proposals with their eval results) and its identity are written before the first effect. Pages receive a `processing_job` stamp in the frontmatter and staging writes it to `config_json.processing_job`. After a crash between the effect commit and the checkpoint, the resume finds the effect through that identity and adopts it without writing again; confirmed IDs are kept in `processing_job_effects`. Publication of different proposals does not become atomic because chunking exists. The current approval policy remains valid, including `require_approval=true`. The `experience` consumer was not changed.

When recovery adopts staged proposals, the final result reads their current decisions under either approval policy. An intervening approval or rejection stays visible, including the approved page ID. A still-pending proposal under manual approval remains pending.

## 6. State, ownership and lifecycle

States: `queued`, `running`, `reducing`, `publishing`, `completed`, `paused`, `budget_exceeded`, `failed` and `invalidated`. The terminal states are `completed`, `failed` and `invalidated`. The final result distinguishes staged, applied, pending and conflicting proposals through the existing proposal states.

The V97 migration adds four private tables: `processing_jobs` (job, scope, owner, state, lease, fence, counters, identity and publication plan), `processing_job_observations` (manifest), `processing_job_nodes` (sanitized map and reduce checkpoints, up to 50,000 per job) and `processing_job_effects` (confirmed effects). `auto_improve_runs` is not reused as progress, and neither is the bootstrap table.

Admission allows at most 128 unfinished jobs server-wide and 16 per workspace/project/owner bucket. Queued, running, reducing, publishing, paused and budget-exceeded jobs all count. The writer checks these limits in the insert transaction, after looking for an equivalent job, so an existing job can still be joined at capacity. A refusal leaves no manifest behind and returns HTTP 503 (an MCP internal error); it does not discard accepted work.

The job carries the context fingerprint: pipeline version (`chunk-v1`), the configured identity of the backends and fallback order, hashes of the map, reduce and final prompts and schemas, derived budgets, chunking and consolidation limits, `per_user_slots` and `require_approval`. The options (instructions and, in auto-improve, the review configuration with eval) and the identity bucket are part of the equivalence key. If the fingerprint computed on resume differs from the stored one, the job is invalidated and not resumed. Map and reduce nodes do not depend on auxiliary pages; page context is read at the final call and revalidated as described in 5.3. A remote change to the model behind the same alias cannot be detected.

Each claim has a 120-second lease, renewed periodically, and a fence token. A guard reserves each generation attempt atomically after local admission, immediately before the terminal send. This includes strict-to-tolerant fallback, another backend, and a Codex 401 resend. Queue saturation or expiry without a send spends no call; an uncertain send stays counted. A logical call has up to 3 provider attempts. Five consecutive failed claims end a transiently failing job; a completed node or final checkpoint resets that count. External eval follows its own limits and does not count as an LLM call.

A final result already checkpointed can finish publication with no calls left. A failed final request is fingerprinted durably; if shrinking leaves the same request, the job fails explicitly instead of sending that payload again.

Page, evidence, proposal and staging writes made through a handle with a fence pass through connection-local TEMP triggers on the writer, which abort a write from a holder that lost ownership, even with a response still in memory. The triggers are not part of the persisted schema, so that another Store or process cannot clear a writer's armed ownership. Manual and scheduler runs find the same compatible job instead of duplicating calls.

Synchronous and asynchronous manual requests derive the same owner bucket and authorization viewer from the authenticated request. Both are part of job equivalence; attribution alone never supplies authorization. A request cannot join another operator's job or one with a weaker viewer. SessionEnd consolidation and scheduled auto-improvement use explicit server-owned jobs instead of guessing credential provenance from a stored username. They can therefore run separately from a DB user's manual job, with additional calls and a possible publication conflict; they do not inherit or weaken that user's authorization.

The worker removes finished jobs, or jobs idle for more than 7 days, every hour, cascading over manifests, nodes and effects and preserving any job with an active lease. The state is derived and private, not a second wiki: a reindex can discard it without affecting published pages. Backup, restore and index rebuild were not reviewed in this document.

## 7. Call admission

With the flag on, the chat provider is wrapped in `AdmittedLlmProvider` and shared by all the server's workers. A fair FIFO queue admits one call at a time, and at most 32 calls wait. Each job call reaches the queue separately, so a long job re-enters at the back of the queue after each call and does not block a short call that arrived during execution. Reductions, finals and retries go through the same queue.

The wait has its own 300-second limit and ends in `CapacityBusy`, a local saturation signal that does not count as a provider failure. Once admitted, the call runs under the provider deadline only. Canceling a waiting call frees its place in the queue. The status accumulates `wait_ms` and `exec_ms` separately for calls that returned; a call lost in a crash or cancellation has no recorded duration. Without an admission wrapper, the measured time counts as execution.

Rerank calls never wait: if there is no immediate admission, they fail fast and search keeps the previous order. No 20-second latency is promised when a long call is already running, and the local queue does not preempt the GPU.

The control only coordinates provider clients inside this server. A direct `llm-test`, another application or another Ollama process stays outside it. Embeddings do not go through this chat queue.

## 8. Asynchronous interfaces

- Admin: `POST /admin/processing-jobs` starts or joins the compatible job and replies 202 with the status; `GET /admin/processing-jobs/{job_id}?workspace=&project=` returns the status. Both sit behind the root-only admin gate and the non-creating scope resolution.
- MCP: `memory_processing_job_start` (requires write access to the project) and `memory_processing_job_status`.
- CLI: `ai-memory processing-jobs start --session-id <id> --kind <kind>` and `ai-memory processing-jobs status --job-id <id>`, with `--workspace`, `--project` and `--json`.
- `kind` accepts `consolidate`, `consolidate_multi` and `auto_improve`.

Start returns the ID, state and progress immediately. Repeating the start identifies the compatible job without repeating the execution. The status carries kind, state, `chunks_total`, `chunks_done`, `calls_used`, `max_calls`, `wait_ms` and `exec_ms`, a redacted reason for pause, failure or invalidation, the final result, and creation, update and completion timestamps. `wait_ms` and `exec_ms` add up, in milliseconds, the wait in the provider's admission queue and the provider execution of the calls that returned (success or failure), recorded under the claim before the result or error checkpoint and unrelated to the `calls_used` reservation. A call in flight when the process crashes is not measured, so the totals can fall below the real time. A provider without an admission queue counts all the time as execution. Prompts, raw responses, checkpoints, owner buckets and claim tokens never leave the server.

The current synchronous contracts do not swap the final report for a job ID. With the flag on, a compatible synchronous entry waits on the same durable job, and its client is still subject to the connection timeout; long jobs should use the new interface.

MCP discovery: 25 tools with the flag off and no jobs; 27 with the flag on; 26 with the flag off and retained jobs, listing only `memory_processing_job_status`. The start tool never appears with the flag off and is also refused on direct dispatch. The CLI shows the disabled state and does not report a false success. A job ID from another project or owner is treated as not found, indistinguishable from a nonexistent ID. No DB-user credential can access `/admin/*`.

The TypeScript SDK exposes `jobs.start`, `jobs.status` and `jobs.discover`, with types, coverage classification, per-variant snapshots (default, with chunking, with retained jobs) and docs. The same routing snippets and server instructions describe the two tools.

## 9. Alternatives considered

### Running summary across chunks

It needs fewer steps, but each call depends on the previous summary. An early omission propagates and resuming is tied to the chain. It was not chosen because of the coverage guarantee and per-source auditing.

### Map/reduce with checkpoints

The adopted choice. It allows planning coverage, resuming completed nodes and separating source reading from publication. It requires temporary storage and validation of losses and conflicts between levels.

### Final write per chunk

Rejected. It exposes partial conclusions, can repeat targets and applies limits/eval several times. It also makes it harder to recover a partially published job.

### Only raising the Ollama timeout or parallelism

It may relieve the current installation, but it does not implement coverage by chunks, resume or publication control. It is not an installation requirement of the feature.

## 10. Phases and acceptance criteria

The criteria below guided the implementation. Section 13 separates the deterministic and runtime evidence from the validation limits described in section 12.

### Phase 1: contracts and flag

Internal types, configuration, errors, states, and start and progress contracts. Off/on compatibility, budget policy, and state after the flag is turned off.

**Acceptance:** absent and `false` configuration keep the same results, calls and effects as the current flow; `true` is only made available in the complete delivery. Invalid budgets fail at load time with a useful message. The template and config reference carry the keys.

### Phase 2: manifest, planner and checkpoints

Scoped batch reads, body coverage, splitting of large observations, persistence, ownership, resume, invalidation and cleanup, without creating a general workflow framework or a new crate.

**Acceptance:** rebuilding the covered ranges recovers exactly the stored bodies of the snapshot; no UTF-8 character disappears, even with observations larger than a chunk and events with equal timestamps. A read with a foreign project is refused, while a legitimate one passes. Two workers do not publish the same job. A purge/move during a call prevents later publication. A restart does not redo completed nodes and does not reset the consumed budget, including when it happens after the send and before the checkpoint. Changes to auxiliary pages or policies invalidate the dependent nodes.

### Phase 3: consolidation

Map/reduce and an adapter for the single-page and multi-page outputs. Integration with SessionEnd and `memory_consolidate`, preserving zero-LLM mode and dry-runs. Sampling and truncation stay out of the new path and remain in the legacy one.

**Acceptance:** fixtures with relevant information at the start, middle and end go through all the chunks; a later correction must not leave an outdated conclusion in the result. An intermediate failure does not publish a partial summary. An excessively large reduction, truncated output or lack of progress does not loop and is not treated as success. The publication path preserves rollback, supersession and index consistency. A crash between the write commit and the checkpoint does not publish the same generation again.

### Phase 4: auto-improve

The same manifest and node contracts, with its own candidate extraction, combination and final staging. Covers manual CLI/admin/MCP calls and the scheduler. The experience consumer does not get chunking in this delivery.

**Acceptance:** two chunks with candidates for the same page do not cause duplicate proposals or multiply the per-run limits. Eval uses the correct base version and runs once per final evaluated proposal, not per chunk. A restart between staging and approval reuses the IDs and respects the current states and manual approval, including if the crash happens between the staging commit and its checkpoint. Concurrent manual and scheduler runs do not duplicate the compatible job.

### Phase 5: admission and tracking

Shared admission, durable worker and asynchronous surfaces in CLI, MCP, admin and SDK. No SQLite transaction or chunk execution permit stays held during a long network wait.

**Acceptance:** a long job does not put all its chunks ahead of a short call; a saturated queue is bounded and observable. Cancellation by timeout removes the corresponding wait. A foreign token or scope cannot read progress or results; admin stays root-only. With the flag off, no new job is admitted, and jobs already persisted stay in a recoverable state.

### Phase 6: runtime proof and documentation

Deterministic fixtures and a controlled test provider for failure, queueing and resume; then the real server path with a local provider, a large history and a concurrent short call. Measure the size of each payload, coverage, total calls, tokens when reported, duration, wait and result quality, comparing the legacy and new flows on the same set of sessions.

**Acceptance:** demonstrate resumption after interrupting the server, no publication of intermediates, and progress of the short call between parts. An interrupted final publication preserves confirmed effects and does not duplicate them on resume. Do not claim a fixed latency or fidelity based only on chunk size. Publish the observed limits and cost, and evaluate the 8,192-token and 128-call values before settling on them.

## 11. Delivery surfaces

- `ai-memory-core`: `ChunkingSettings`, job types and IDs (`processing_job.rs`).
- `ai-memory-consolidate`: `ProcessingService` and the `processing/` module (planner, digest, map/reduce state machine, budgets, final step, publication), with its own JSON prompts.
- `ai-memory-store`: V97 migration, `processing_jobs.rs` (leases, call reservation, idempotency, cleanup), integration with purge, move, merge and observation pruning.
- `ai-memory-llm`: `AdmittedLlmProvider`, installed in `serve.rs` only with the flag on.
- `ai-memory-cli` and `ai-memory-mcp`: configuration, off/on dispatch, worker, admin routes, `processing-jobs` on the CLI and the MCP tools.
- `companions/ai-memory-ts`: `jobs.start`, `jobs.status`, `jobs.discover`, types, coverage and snapshots.
- Docs: `ARCHITECTURE.md`, `auto-improvement-loop.md`, `config-properties.md`, `security-boundaries.md`, `design-client-sdk.md` and the SDK documentation. The CHANGELOG entry follows the repository rule, with no invented ticket reference.

The feature is additive and targets the next feature-version branch, with no automatic bump or tag. Runtime changes stay separate from the local installation used in the diagnosis.

## 12. Risks and pending verification

Chunked mode increases the number of calls and the total cost. Reading everything does not guarantee the model retains everything. Repeated context and reduction levels can raise cost and introduce synthesis errors. The 12-item digest limit also compresses fact density in rich chunks, and the fidelity tests have to expose that loss.

Smaller chunks reduce occupancy per call, but they do not make the GPU preemptible and do not coordinate other processes. Output of many thousands of tokens also occupies the model; limiting only the input is not enough.

Cost, latency and fidelity have not yet been measured with a real model, and models have not been compared to choose the 8,192-token and 128-call values. The tests with a controlled provider verify limits, coverage and recovery, not synthesis quality or Ollama performance. Backup and restore of a job in progress had no dedicated smoke test; the private state stays in the same SQLite database covered by the existing operations.

## 13. Observed verification

Local verification ran on macOS arm64 without changing the installed configuration, restarting the operator's server or calling their Ollama instance.

- `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --all -- --check` passed.
- The focused tests passed: 451 in the LLM crate, 51 in processing, 23 for job persistence and 9 for sidecars. With the engine off, the scheduler skipped the session with a paused job and reviewed the control session.
- The SDK passed `npm run check` and 58 integration tests against the real binary. The snapshots came from three servers: 25 tools by default, 27 with chunking and 26 after turning it off with retained jobs.
- The server smoke passed all 115 checks, using temporary directories and a controlled HTTP provider: flag off, UTF-8 coverage, map/reduce, two SIGKILL crashes, resume, pause and reactivation, authorization, a concurrent short call, multi-page consolidation and staging of a non-empty proposal with manual approval.
- In the coverage session, 302 observations were stored. The non-empty manifest bodies were rebuilt byte for byte from the map entries; a 15,065-byte body spanned two calls. There were 13 map calls, one reduce call and one final call. The largest payload, including schema, came to 6,193 tokens by the budget's own estimate, below 8,192. These numbers do not measure the tokens or latency of a real model.
- The two crashes did not repeat nodes that were already confirmed and did not produce two publications. The short call entered before the long job's next chunk; the provider received one request at a time.
- Mutations of the owner guards and of the sidecar's post-write confirmation made the adversarial tests fail; with the guards restored, they passed. A partial installation of the guards was also reproduced and now ends the writer before it accepts commands.

The correction review added checks for strict final-review envelopes, viewer-aware equivalence, bounded job admission, captured full-page bases, exact-budget recovery, unchanged final resends, consecutive transient failures and decisions made before staging recovery. A second independent source review found no remaining confirmed blockers after its follow-up fixes.

The corrected server also ran against a tolerant HTTP provider that received no `response_format` schema. All three operations completed over more than 256 observations, including an oversized UTF-8 body. Consolidation used 25 sends (21 map, 3 reduce, 1 final), including one deliberately malformed map answer; multi-page consolidation and auto-improve each used 23 (19 map, 3 reduce, 1 final). Each job's durable attempt count matched the HTTP traffic. The malformed map was not counted as completed coverage, start/middle/end markers reached the final input, no intermediate facts were published, repeated active starts joined the same job, and the flag-off control refused new work. This correction smoke did not repeat the earlier crash/concurrency scenarios; auto-improve used an explicitly empty final proposal set.

Removing the new viewer and queue guards made four adversarial tests fail; restoring them passed all 28 job tests. Removing the full-page base check made its stale-update test fail, and restoring it passed both stale-base and live-base/legacy controls. Replacing the strict final-review envelope with the legacy decoder made `{}` complete incorrectly, which the malformed-final regression caught.

The final corrected tree passed `cargo test --workspace --all-targets --no-fail-fast`: 7,438 tests across 15 suites, with 18 intentionally ignored by the project. Workspace doctests, formatting, clippy with warnings denied, `npm run check` and all 58 SDK integration tests also passed. This includes the restored malformed-final regression, the exact-budget crash gap and resumed manual approval/rejection cases. The fixtures were isolated by removing an inherited `AI_MEMORY_DATA_DIR` only from test processes and using the real Node executable on the PATH; installed services and configuration were unchanged.
