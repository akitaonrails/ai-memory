# Design proposal: opt-in file path index (#1843)

**Status: open design proposal, not implemented, with no assigned release.**
Issue [#1843](https://github.com/akitaonrails/ai-memory/issues/1843) remains
open, and the index alone does not close it. Any implementation must rebase the
migration number and branch assumptions below against the then-current tree.

The maintainer set the order in
[#1843](https://github.com/akitaonrails/ai-memory/issues/1843#issuecomment-6508464269):
specify the opt-in path index, then design delivery separately, then measure
before expanding. After auditing the first version of this note against
`release/3.4`, the maintainer asked for two more things before the index lands
([#1843](https://github.com/akitaonrails/ai-memory/issues/1843#issuecomment-6550103798)):
measurement provenance (section 3.8) and a readable target for a path hit
(section 3.6). This note covers the index and its measurement; delivery gets
its own note. Section 10 lists the decisions for the maintainer.

## 1. Problem

A file-tool observation records that a file tool ran, but not which file. The
body is `tool_family`, `outcome`, `tool_call_id` and the bounded output, and the
table has no path column. So on the zero-LLM path the store cannot answer "what
do we know about `src/lib.rs`" except by matching incidental text. Text matching
is unreliable for paths (section 2), and the maintainer ruled it out as a basis
for delivery.

Non-goals: delivery into any harness, calls that name more than one file
(including a multi-file `apply_patch`), shell commands, legacy shell and
PowerShell hook bundles, any new MCP tool or HTTP route (section 6).

## 2. Current state

Checked against `release/3.4` at `30fd62ad`, where the implementation lands.
References name the file and the symbol.

| Part | Where | Fact |
|---|---|---|
| Table | `crates/ai-memory-store/migrations/V01__init.sql`, `V10__observation_extension_events.sql` | `observations` keys every row by `session_id`, `workspace_id` and `project_id`, all `ON DELETE CASCADE`. No path column. V10 added two nullable columns (`extension`, `source_event`) and a partial index `WHERE extension IS NOT NULL AND source_event IS NOT NULL` scoped by `(workspace_id, project_id, ...)`. The highest migration is V96. |
| Body | `crates/ai-memory-hooks/src/payload.rs` `safe_tool_body`, `closed_tool_agent`; `capture_policy.rs` `tool_observation_metadata` | Closed schema for the agents in `closed_tool_agent`. `tool_observation_metadata` keeps family, call id and MCP names; it never reads a path field. |
| FTS | `crates/ai-memory-store/migrations/V12__fts_remove_diacritics.sql` (`observations_fts`) | The tokenizer keeps `/`, `_` and `-` inside tokens. An absolute path in tool output (`/repo/src/lib.rs`) does not match a query for `src/lib.rs`. The CJK index added in V87 covers only unspaced Han, Kana and Hangul. |
| Client policy | `crates/ai-memory-cli/src/commands/hook.rs` `run_with_payload_in_home` | The native client applies `[capture] ignore_paths` before the spool: Drop sends nothing, MetadataOnly rebuilds the body, Keep under an active or invalid policy attaches `_ai_memory_capture`. Keep under an **inactive** policy (no marker, or no `ignore_paths`, the common case) attaches nothing. |
| TS policy | `crates/ai-memory-cli/src/commands/render_shared.rs` `ts_capture_policy_v1` | The generated integrations (OpenCode, OMP, Pi, OpenClaw) implement the same table and return early on an inactive policy, also without a claim. The template is generated from `capture_policy.rs` so the two front doors cannot drift. |
| `apply_patch` | `capture_policy.rs` `extract`, `patch_paths` | Codex `apply_patch` and Droid `ApplyPatch` carry no direct path field. `patch_paths` reads the `Add File`, `Update File`, `Delete File` and `Move to` headers, and feeds only the cross-checkout lookup. A file call without a direct path is `MissingOrMalformed`, so under an active policy it becomes metadata-only, whatever the paths. |
| Protocol | `capture_policy.rs` `CaptureProtocol` | Parses through a `deny_unknown_fields` wire struct with a fixed version. |
| Reserved fields | `crates/ai-memory-hooks/src/assistant_capture.rs` `apply_assistant_backstop`; `router.rs` (`_ai_memory_usage`) | Precedent for a separate top-level field: `_ai_memory_assistant` is versioned, parsed with `deny_unknown_fields`, and always removed from the raw payload before storage, whether or not its gates accept it. |
| Server check | `crates/ai-memory-hooks/src/router.rs` `inspect_capture_envelope` | An unparsable claim is treated as invalid; for a file tool that rewrites the event to metadata-only. A parsed claim is re-inspected against the payload; the server never receives the client's patterns. |
| Checkout root | `crates/ai-memory-cli/src/marker.rs` `checkout_root` | The nearest ancestor holding `.git` (a worktree's `.git` file counts). `ignore_paths` patterns root at the marker directory, not the checkout root. |
| Normalization | `capture_policy.rs` `normalize_segments`, `marker.rs` `absolute_normalized` | Lexical; `.` and `..` folded; symlinks never resolved. |
| Sanitizer | `crates/ai-memory-core/src/sanitize.rs` `scrub` | Credential directories (`.ssh`, `.aws`, `.kube`, `.config/gcloud`, `.gnupg`) are redacted as `credential_path`; the patterns need a separator before the directory. |
| Ingest config | `docs/config-properties.md` (`[ingest]`) | Holds the server-side capture filters, among them `record_tool_names`, off by default. |
| Ownership | `crates/ai-memory-core/src/actor.rs` `OwnerFilter`; `crates/ai-memory-store/src/reader.rs` `sessions_hidden_from` | Observations are owned through `sessions.actor_user` (V40). The observation stream hides other operators' sessions with `sessions_hidden_from`. `session_observations_scoped` takes no owner and relies on its callers checking the session first. |
| Session reads | `crates/ai-memory-mcp/src/server.rs` `memory_read_page`, `memory_read_session_observations` | `memory_read_page` takes a page `path` or a `query`, never an id, and applies no owner filter (pages are shared, invariant 16). `memory_read_session_observations` takes a `session_id`, admits it through `session_summary_scoped` under `OwnerFilter::for_actor_context`, and bounds the rows and each body. |
| Session page | `crates/ai-memory-hooks/src/synth.rs` `synthesize_session_page`; `reader.rs` `latest_page_ids_by_paths` | The rule-based `sessions/<id>.md` page is written without an LLM at a substantive SessionEnd, at a checkpoint and at idle auto-end. An open session that has not checkpointed has none. `latest_page_ids_by_paths` resolves a batch of paths to page ids only, so it cannot say which path, and so which session, a page belongs to. |
| Ingest metrics | `crates/ai-memory-core/src/ingest_metrics.rs` `IngestMetrics`, `IngestMetricsSnapshot`; `crates/ai-memory-mcp/src/metrics.rs` `render_prometheus` | A fixed set of `AtomicU64` scalars, never a map, reset when the process restarts. Read by `/admin/status`, `/admin/metrics` and the web health panel, all root-only on a multi-user server and content-free (`docs/security-boundaries.md` rows 3d and 3e). `render_prometheus` names each field explicitly under a compile-time family and the closed labels `result`, `outcome`, `kind`; the test `snapshot_is_content_free` pins the key list. No counter has a tool-family dimension, nothing records why a file-tool event has no path, and the snapshot has no process start time. |
| Admission order | `router.rs` `admit_hook`, `HookProcessingOutcome::record` | `inspect_capture_envelope` runs first, before the subagent drop, the ingest drop list, the semaphore and the per-source rate limiter (429). Storage happens later, in the spawned `process_envelope`, which records the final outcome (`stored`, `replayed`, `failed`, ...). |
| Delivery and latency | `crates/ai-memory-cli/src/commands/hook.rs`, `hook_spool.rs`; `install_hooks.rs` `postHook`; `router.rs` `handle_hook`; `crates/ai-memory-core/src/latency.rs` | The native client writes each event to a local spool, and a detached `hook-drain` delivers it through `POST /hook/batch`. Two drains run inside the hook the agent waits on: `session-start` drains the backlog within `start_drain_budget` before fetching its handoff, and Zero, whose sandbox kills detached children, drains in-process within a short budget. The generated TS integrations queue each event in process (`postHook` returns `void`), a background flush posts it to `POST /hook`, and the spool takes it when delivery fails. `hook_latency` times `POST /hook` admission only, server-wide, in buckets 2 to 2.5 times apart (`LATENCY_BUCKET_BOUNDS_US`). |
| Backfill | `crates/ai-memory-cli/src/commands/backfill.rs` `BACKFILL_EXTENSION`; `payload.rs` `HookQuery` | Backfill posts to `/hook/batch` with `extension=ai-memory-backfill` in the query, and its tool events carry a title and message, not the harness's tool payload. The native client and the generated TS integrations send no `extension`. |

## 3. Proposal

### 3.1 Storage

One nullable column and one partial index, in a single additive migration:

```sql
ALTER TABLE observations ADD COLUMN file_path TEXT;
CREATE INDEX idx_observations_file_path
    ON observations(workspace_id, project_id, file_path, created_at)
    WHERE file_path IS NOT NULL;
```

The column follows the V10 precedent. A row has at most one path in this
version (section 3.2), so a side table would only add a join. Because the path
lives on the observation row, every lifecycle operation in section 3.5 applies
to it with no new
code.

### 3.2 Where the path comes from

Only the two front doors that implement capture policy v1 produce a path: the
native client and the generated TS integrations. They add a reserved top-level
field, versioned like `_ai_memory_assistant`:

```json
"_ai_memory_file": { "version": 1, "path": "src/lib.rs" }
```

**When the field is sent.** All four must hold:

- the event is a PostToolUse (a PreToolUse never carries the field);
- the disposition is Keep, under an active or an inactive policy;
- the family is `file`;
- the call names exactly one file: a direct path (extraction state
  `extracted`, `path_count` 1), or an `apply_patch` / `ApplyPatch` whose
  envelope has exactly one file header, read with the existing `patch_paths`.
  A `Move to` header names two files and is skipped.

Drop and MetadataOnly never carry the field, so an ignored path never leaves
the machine through it. Under an inactive policy the kept payload already
carries the raw path, so the field exposes nothing new; it only carries the
normalized form the server cannot compute (section 3.3). An `apply_patch` only
reaches this point under an inactive policy (section 2); section 10, question
6, asks whether that is acceptable.

**When the client refuses the path.** If the event qualifies but the client
cannot produce a key (section 3.3), it sends the field without a path:

```json
"_ai_memory_file": { "version": 1, "skipped": "outside_checkout" }
```

`skipped` says why: `outside_checkout`, `no_checkout` (no `.git` ancestor) or
`refused` (a step 4 refusal). It never carries a path or part of one. Exactly
one of `path` and `skipped` is present, and the keys keep
`deny_unknown_fields`. The reason set is additive: `skipped` is a short token
(`[a-z_]`, at most 32 characters), and the server counts a well-formed value it
does not know as `other` instead of rejecting the field. A newer client can add
a reason without a version bump, and the counters stay fixed (section 3.8).

The client never reads server config, so the field goes out on every such
event even when the server key is off. The cost is one short JSON object per
kept file-tool post, which the server removes on arrival (section 3.4).

**Why a separate field.** `CaptureProtocol` parses with `deny_unknown_fields`,
so a new key inside `_ai_memory_capture` would make an older server treat every
claim as invalid and rewrite every file event to metadata-only. An unknown
top-level field is ignored by older servers, and older clients never send it.

**Who never sends it.** Legacy shell and PowerShell bundles, manual Docker
script bundles and older clients, so they never index.

### 3.3 Normalization

The client computes the path, because only the client knows the checkout root
of the event:

1. Make the tool path absolute against the event's cwd with the existing
   lexical helpers (`absolute_normalized` in the CLI crate,
   `normalize_segments` in the hooks crate). Both are private today; the
   implementation widens the visibility it needs and adds no second
   normalizer.
2. Strip the `checkout_root` prefix. A path outside the checkout is not indexed;
   the client sends `skipped: outside_checkout`, or `no_checkout` when no
   checkout root exists.
3. Use `/` as the separator; keep case as given.
4. Refuse: an empty result, a leading `/` or drive letter, any `..` segment, more
   than 512 characters (the bound `HANDOFF_FILE_MAX_CHARS` uses for stored
   paths). The client sends `skipped: refused`.

Relative to the checkout root rather than the marker directory, so two worktrees
or two clones of the same repository produce the same key.

On the client, step 1 already folds `..`, so the `..` refusal matters on the
server: it applies to the string the server receives. The server repeats every
refusal in step 4 and drops only the field, never the event. It cannot repeat
step 2, because it never learns the checkout root; a path from outside the
checkout reaches it only as absolute or with `..`, which step 4 already
refuses.

### 3.4 Server acceptance

The server always removes `_ai_memory_file` from the raw payload, as
`apply_assistant_backstop` does with `_ai_memory_assistant`, on both routes
that build an envelope: `handle_hook` and the batch route. It stores
`file_path` only when all of these hold:

- `[ingest] index_file_paths = true` (default `false`; section 3.7);
- the field parses with `deny_unknown_fields` and the current version, and
  carries `path`;
- the server's own inspection of the event yields Keep, family `file` and
  exactly one file. Under an inactive policy the client attaches no claim, and
  `inspect_capture_envelope` returns such an event without inspecting it, so
  the server runs `capture_inspector(...).inspect` itself, as
  `inspect_capture_envelope` already does for a claim: with the claimed policy
  state, or `Inactive` when no claim is attached. For an `apply_patch`, the
  single file is checked with `patch_paths`;
- the path passes the step 4 checks;
- `scrub` leaves the path unchanged, checked on the path with a leading `/`
  prepended, because the credential-directory patterns need a separator before
  the directory (`.ssh/config` at the checkout root would otherwise pass).

The path enters the store through the typed boundary: `NewObservation` gains
`file_path: Option<String>`, and `Sanitized::<NewObservation>::new` runs the
checks above and clears the field when one fails. A failed check drops the
field and keeps the observation exactly as today.

**Classification.** With the key on, the server inspects every PostToolUse it
admits, not only those that carry the field, and carries the result on the
envelope to the point where the delivery's outcome is recorded
(`HookProcessingOutcome::record`). A counter of section 3.8 moves there, once,
and only for a delivery that ends `stored`. A 429 the client retries, a replayed
delivery, and an event the subagent rule, the drop list or authorization
discards move none. Backfill (`extension=ai-memory-backfill`) is skipped before
inspection. With the key off nothing new is inspected or counted.

The inspection runs in admission, before the 202. The agent waits on it only
through the inline drains named in section 2: `session-start` and Zero. Section
10, question 8, offers a cheaper variant.

### 3.5 Lifecycle

The column inherits the observation's lifecycle (`docs/lifecycle-ops.md`):

| Operation | Effect on `file_path` |
|---|---|
| `observation_retention_days` prune (`prune_consolidated_observations`) | deleted with the row |
| purge-project, purge-session, delete-workspace | deleted by the existing cascade; purge tombstones also stop spool replay from recreating it |
| move-session, move-project, merge-project | moves with the row (the existing `UPDATE observations` re-stamps it) |
| reset, restore, backup, `uninstall --purge-data` | follows `db/` |
| `reindex` | not rebuilt; `reindex` never reconstructs observations |
| turning the key off | stops new writes; existing values stay until their rows go (section 10, question 3) |

### 3.6 Read authorization and result mapping

One internal store function, with no MCP tool, HTTP route or CLI command in
this step:

```rust
observations_for_file(scope: (WorkspaceId, ProjectId), owner: &OwnerFilter,
                      file_path: &str, limit: usize) -> Vec<FileSessionRef>
```

- `FileSessionRef` is a new type with one entry per session, newest first:
  - `session_id`;
  - `page: Option<PagePath>`, the session's `sessions/<id>.md`;
  - `hits`, the count of that session's observations with this path;
  - `last_seen`, the newest `created_at` among them;
  - `last_offset`, that observation's position in the session's capture
    order, which is the `offset` `memory_read_session_observations` takes.
- Scope comes from the no-create lookup in `ScopeResolver`; no `global`,
  `scopes` or `[recall] default_global` broadening.
- `owner` is the filter the read tools apply to the same caller
  (`OwnerFilter::for_actor_context`), so no entry points at a session those
  tools would refuse. It is applied in SQL, as a join on `sessions.actor_user`
  with the rule of `OwnerFilter::admits` (the rule `sessions_hidden_from`
  applies after a fetch), before `limit`: a hidden session neither appears nor
  takes a slot. Shared (`NULL` owner) sessions are visible.
- `page` is found by path in the same query; an expired page counts as none,
  and so does a session with no page yet. `latest_page_ids_by_paths` does not
  fit: it returns ids only and cannot attribute a page to a session.
- `limit` is bounded; the result carries no bodies.

**Why sessions, not observations.** No tool takes an observation id, and a
file-tool observation may hold only an outcome. A session is a unit an agent
can already read, through an existing tool with existing bounds:

| Entry | Readable through | Owner check | Bounds |
|---|---|---|---|
| `page` set | `memory_read_page(path)` | done by `observations_for_file` | `max_chars` |
| `page` is `None` | `memory_read_session_observations(session_id, offset: last_offset)` | `session_summary_scoped` | `limit` and `body_max_chars` |

This is stricter than the observation stream, which lets a session page rank
whatever its owner (`docs/security-boundaries.md` row 5a). Here the hit itself,
"this session touched this file", comes from the observation, so it follows the
observation's owner. Delivery picks which read to make and how much to show;
this step guarantees only that every entry points at a target the caller may
read.

It lives in `reader.rs`, next to the private `sessions_hidden_from`. It exists
to prove isolation now, as #1843 asks, and is the function a later delivery
design would call. Until then its only callers are tests. AGENTS.md allows new
surface without a shipped caller when there is an explicit requirement, and
the #1843 sequence is that requirement; section 10, question 5, lets the
maintainer defer it instead.

### 3.7 Opt-in

A server key in `[ingest]`, off by default like `record_tool_names`. The
operator owns the store, so the operator decides whether paths are kept. The
RFC pointed at the marker's `[recall]` table, but for delivery, which spends
tokens per repository; keeping data is a decision about the store. A
repository keeps opting out of capture with `ignore_paths`, which applies
before the field exists. The key lands in `config.default.toml` and
`docs/config-properties.md` in the implementation PR.

### 3.8 Measurement

`file_path IS NULL` cannot say why a row has no path, so the server counts the
reason when it records the delivery (section 3.4). New scalars in
`IngestMetrics`, one per outcome, plus `started_ms` (the process start time) in
the snapshot. They reach the same readers as the existing counters:
`/admin/status`, the web health panel, and `/admin/metrics`, where
`render_prometheus` renders them as one compile-time family,
`ai_memory_hook_file_paths_total`, under the existing `outcome` label. All are
root-only on a multi-user server, server-wide, with no scope, path or
client-supplied label. Row 3d of `docs/security-boundaries.md` is updated in
the same PR.

With the key on, each PostToolUse delivery that ends `stored` and whose call the
server's inspection classifies as family `file` increments exactly one
counter; the first matching row wins:

| Order | Counter | Condition |
|---|---|---|
| 1 | `file_path_invalid_claim` | the claim does not parse, or the server's inspection disagrees with it |
| 2 | `file_path_metadata_only` | a valid claim whose disposition is MetadataOnly: a file call with no readable path under an active policy, an output-spool read, a match that could not be proven, or a broken marker (`PolicyState::Invalid`) |
| 3 | `file_path_no_file` | the call names no file the server can read (an empty or malformed patch) |
| 4 | `file_path_multi_file` | the call names more than one file |
| 5 | `file_path_field_absent` | no `_ai_memory_file` field (an older or unsupported client) |
| 6 | `file_path_unknown_version` | a field version this server does not read (a newer client) |
| 7 | `file_path_skipped_outside_checkout`, `_no_checkout`, `_refused`, `_other` | the field carries `skipped` |
| 8 | `file_path_server_rejected` | a malformed field, `path` and `skipped` together or neither, or a failed check in section 3.4 |
| 9 | `file_path_indexed` | the path is stored |

An `ignore_paths` match never reaches the table: the client drops the event
(section 2), so no counter can see it, and none needs to.

**Coverage.** Two ratios, because two different things can be low. The
numerator of both is `indexed`.

```text
current_client = indexed / (invalid_claim + metadata_only + no_file
                             + multi_file + skipped_* + server_rejected
                             + indexed)
population     = indexed / (every file_path_* counter)
```

- `current_client` leaves out `field_absent` and `unknown_version`, the events
  from clients that do not speak this field version. It measures the design:
  how much of what a current client could index actually gets indexed. The
  go/no-go gates on it. An older client's multi-file or empty call still lands
  in it, because without a field the server cannot tell the client's version;
  that can only lower the ratio.
- `population` adds them back. It measures how far the upgrade has reached,
  which depends on rollout rather than design, so it is reported and not gated.

Outside both by construction: backfill (skipped before inspection, and its tool
events carry no tool payload), Drop on the client, PreToolUse events,
deliveries that do not end `stored`, events while the key is off, and families
other than `file`. The report shows every counter, so a low ratio names its
cause.

**Sample.** At least 400 events in the `current_client` denominator, which puts
the 95% confidence interval of the ratio within ±5 percentage points
(1.96 · √(0.25 / 400) ≈ 0.049). Events of one session are correlated, so the
window also needs at least 30 sessions, with both front doors (native and TS)
present. The report gives the interval, not only the point estimate.

**Window.** A window is the difference between two snapshots with the same
`started_ms`. The key is read once at startup (invariant 1), so turning it on
or off takes a restart, which starts a new `started_ms`: a segment never mixes
key on and key off. A restart closes a segment, and a window may add segments.
The report publishes the snapshots, never a path.

**Prior-session hits.** The share of indexed events whose path an earlier,
different session in the same project had already indexed. This is how often
delivery would have had something to show. It comes from SQL on the operator's
own store, never from a metric, and is reported to size the delivery design,
not gated.

**Latency.** Measured on the client, where the agent waits, new client against
the previous release:

- native client: the wall time of the hook command for a file-tool
  PostToolUse, which adds the normalization, and for `session-start`, whose
  inline drain carries the server's inspection;
- TS integrations: the wall time of the synchronous `postHook` handler for a
  file-tool PostToolUse; the network post is queued and does not block the
  agent.

Each measure takes at least 200 samples per client version, from a scripted
benchmark that replays fixed payloads (section 7). With 200 samples the 95%
interval of an observed p95 spans roughly the 92nd to the 98.5th percentile;
with 30 it spans the 87th to the 100th, which is noise. The bound is absolute,
not relative: these hooks take a few milliseconds, where 10% sits inside the
noise, and invariant 5 already sets a 200 ms hook budget. Following the usual
regression-detection practice of requiring both significance and magnitude,
a measure fails when its p95 grows by more than 10 ms (5% of that budget) and
the bootstrap 95% interval of the difference excludes zero.

`hook_latency` is not used: it is server-wide, and its buckets are 2 to 2.5
times apart. The server's cost is reported, not gated: `shed_saturated` and
`shed_rate_limited` from `/admin/status`. Latency of delivery itself (a
fail-open hook with a budget) belongs to the delivery note.

The go/no-go threshold is section 10, question 7.

## 4. Invariants

Numbered as in the cross-cutting invariants of AGENTS.md.

- **1 (one config read):** the key is read by `Config::load` on the server; the
  clients read nothing new.
- **2 and 3 (single writer, same transaction):** `file_path` is written by the
  existing observation insert, in the same statement.
- **4 (typed identity):** `file_path` is an attribute of the observation, not
  part of its identity; the row keeps its `(workspace_id, project_id)` and no
  new domain row is created.
- **5 (bounded hooks):** no new synchronous call; the field rides the existing
  post. The client adds only the normalization before its write. The server's
  inspection runs in admission, before the 202 on `/hook`, and is
  extraction-only for an event without a claim (section 3.4).
- **6 (sanitizer boundary):** the field is validated and scrub-checked inside
  `Sanitized::<NewObservation>::new`, so it reaches the store only through
  `Sanitized<NewObservation>`.
- **16 (pages shared, batons owned):** the index belongs to observations, which
  are owned through their session; a path hit returns only sessions the owner
  filter admits. Pages are untouched and stay shared.

Not numbered there, but kept: the counters are fixed scalars with no path, no
scope and no client-supplied label (rows 3d and 3e of
`docs/security-boundaries.md`); `skipped` never carries a path, and an unknown
value folds into `other`.

## 5. Trust and security

New rows for `docs/security-boundaries.md`. Each test fails with its guard
removed and passes with it:

| Guard | Enforcing code | Adversarial test |
|---|---|---|
| A path never crosses projects | `observations_for_file` scope predicate | Same `file_path` in two projects; a query from one never returns the other's rows. Control: same project returns them. |
| A path never crosses operators | owner join in `observations_for_file` (the `OwnerFilter::admits` rule) | Operator B's session is hidden from A; B and `OwnerFilter::Any` see it; a shared session is visible to both. |
| An ignored path is never stored | client Drop and MetadataOnly carry no field | Marker ignores `secret/`; a Read there stores no row and no `file_path`. |
| A forged or malformed field is not stored | server checks in section 3.4 | Absolute, `..`, over-long, credential-directory, wrong-version and non-file-family fields are dropped, as is a field on a patch that names two files; the observation is kept and the field never reaches storage. |
| Off means off | section 3.4 key check | With the key off, a valid field stores nothing and no `file_path_*` counter moves. |
| A hidden session takes no slot | owner join applied before `limit` in `observations_for_file` | With `limit = 1`, operator B's newer session and A's older one touch the same path; A gets its own session, not an empty result. Control: `OwnerFilter::Any` sees both. |
| Counters carry no path | fixed scalars in `IngestMetrics`; one compile-time family under the `outcome` label in `render_prometheus`; `skipped` parsed into a closed set | `metrics_count_queries_and_hooks_without_leaking_any_content` extended with a file-tool event carrying a planted path and a planted unknown `skipped` value: neither appears in `/admin/metrics`, `/admin/status` or the health panel, and every label key stays in the closed set. `snapshot_is_content_free` lists the new keys. Control: the `file_path_*` counters move by exactly what was sent. |

A client can still lie about a path inside its own project, as it can about any
captured text today. The server cannot verify that a path was not ignored,
because it never receives the patterns; this is the same trust the capture
policy already places in the client.

## 6. Compatibility

- Key off (the default): existing columns are unchanged and `file_path` stays
  `NULL`; the field is removed and ignored.
- Migration is additive: one nullable column, existing rows read `NULL`, and
  no current query changes. Like every migration it is one-way: an older binary
  refuses the upgraded store (`StoreError::DataSchemaAhead`), so a downgrade
  needs a `restore` from the pre-upgrade backup.
- New client with an older server: the field, in either form, is ignored.
  Older client with a newer server: no field, no path, counted as
  `field_absent`. A newer client's unknown `skipped` reason reaches a current
  server as `other`.
- No MCP, HTTP, SDK or routing-snippet change.

## 7. Tests

**Unit**

- Rust: normalization (relative, absolute, Windows spelling, worktree), each
  refusal in section 3.3, the scrub check including `.ssh/config` at the root. A
  symlinked cwd is not resolved: a cwd whose lexical form is outside the
  checkout yields `skipped: outside_checkout` and no path.
- Rust, `apply_patch`: one `Add File`, `Update File` or `Delete File` header
  yields the field; two files, a `Move to` header, an empty or malformed patch,
  and an active policy yield none. The same table for Droid `ApplyPatch`.
- Generated TS: the Node checks `render_shared.rs` already runs for
  `ts_capture_policy_v1`, extended to the field under active and inactive
  policies, and to its absence on Drop and MetadataOnly.
- Both front doors: each refusal in section 3.3 emits its `skipped` value and
  no path.
- Rust, field parsing: `path` and `skipped` together, neither, a malformed
  `skipped` token and a wrong version are rejected; a well-formed unknown
  `skipped` value parses as `other`.

**Integration**

- `crates/ai-memory-store/tests/suite/multi_session.rs`, `project_authz.rs`: the
  project and operator rows in section 5.
- Hooks router tests, on `handle_hook` and the batch route: forged and
  malformed fields, key off, no field from a legacy-shaped payload, a field on
  an event without a claim that the server's inspection rejects, the
  observation kept in every refusal; a Codex-shaped `apply_patch` PostToolUse
  with one file stores its path, one with two files stores none.
- Hooks router tests, counters, on both routes: one event per row of the
  section 3.8 table moves exactly that counter. None moves for a backfill-shaped
  batch, a PreToolUse, a non-file tool event, or any event while the key is off.
  A delivery shed with 429 and then retried, and a replayed delivery, count
  once.
- `multi_session.rs`: `observations_for_file` returns one `FileSessionRef` per
  session, with `page` set for a session that has `sessions/<id>.md` and `None`
  for an open one or an expired page, and a `last_offset` that
  `memory_read_session_observations` resolves to the hit; plus the
  hidden-session row of section 5.
- `observation_prune.rs`, `session_move.rs`: prune, purge and move take the path
  with the row.
- Recovery: the migration applied to a store with existing observations, and a
  failed observation insert that leaves no `file_path` behind (AGENTS.md: new
  disk+SQL mutations need recovery and rollback tests).
- `crates/ai-memory-cli/tests/suite/external_capture_ts.rs`: the generated
  plugin, run in Node against a real server, stores the path.

**Live harness (manual, recorded in the implementation PR's test plan).** The
existing smoke tests fire no tool event: `tests/e2e/claude_plugin_smoke.py`
runs without credentials, `codex_plugin_smoke.py` points the provider at a
closed port, and `handoff_smoke.sh` records why OpenCode is not automated.

- Claude Code: `claude -p` with credentials in a fixture checkout reads and
  edits one file each; the SQLite rows carry the expected `file_path`.
- Codex: `codex exec` edits one file through `apply_patch`; the row carries
  `file_path`. A second run edits two files in one patch; no row carries a
  path. Codex reads go through the shell and stay out of scope.
- OpenCode: the same through `opencode run`.
- These runs are a sanity check for latency, not its sample: they produce too
  few events.

**Latency benchmark (scripted, recorded in the implementation PR).** Replays
fixed file-tool PostToolUse payloads at least 200 times into the native hook
command and into the generated `postHook`, and runs `session-start` against a
fixed spool backlog at least 200 times, each with the new client and with the
previous release, on the same machine. It reports p50, p95 and the bootstrap
interval of each p95 difference (section 3.8).

**Measurement** (before any delivery design): on a real store with the key on,
publish the counter snapshots, both coverage ratios with their intervals, the
prior-session hit share, the latency benchmark and the stored bytes per
thousand observations (section 3.8). Delivery is designed only if they meet
section 10, question 7.

## 8. Pull request plan

1. This note, against `main` because it is docs only (as #1319): first version
   in #1883, measurement and result mapping in a second docs PR.
2. Index, against the standing feature branch (currently `release/3.4`):
   migration, both front doors including `skipped`, server acceptance and the
   section 3.8 counters, `observations_for_file`, config key and docs, the
   section 5 rows and the section 7 tests, CHANGELOG. Off by default.
3. Measurement on a real store, published with the PR or right after it.
4. Delivery: a separate design note, starting with PostToolUse in one harness,
   once per path per session, untrusted-data framing, no global broadening,
   fail-open with a latency budget.

#1843 stays open through step 4; the index alone does not close it.

## 9. Alternatives

- **Text matching only (option A in #1843):** no storage change, but the
  tokenizer misses absolute paths in tool output and coverage without LLM
  consolidation is low. Rejected by the maintainer as a basis for delivery.
- **A side table:** fits several paths per call (a multi-file `apply_patch`), at
  the cost of a join and its own lifecycle. Deferred until multi-file calls are
  in scope.
- **The path inside `_ai_memory_capture`:** breaks older servers (section 3.2).
- **A protocol version bump:** same effect on older servers.
- **A hash instead of the path:** a repository path has little entropy, so the
  hash hides little, and it makes the index unreadable for diagnosis.
- **Server-side normalization:** the server sees the cwd, not the checkout
  root, so worktrees would not share keys.
- **Observation ids as the result:** no tool reads by observation id, and a
  file-tool body may hold only an outcome (section 3.6).
- **Persisted daily counters (as `client_activity`, V46):** survive a restart,
  at the cost of a migration and a buffered writer path. Process counters with
  `started_ms` cover a window of days.
- **A status column per row:** measurable per project with SQL, but it stores a
  value on every file-tool row to answer a question asked once.

## 10. Decisions for the maintainer

Each question carries the default this note assumes.

1. **Key name and section.** Default: `[ingest] index_file_paths`, next to the
   other server-side capture filters.
2. **Reads and writes, or writes only.** Default: both, because the later
   delivery is about a file the agent opens.
3. **Turning the key off.** Default: keep existing values until their rows go;
   retention and purge already remove them, and a clearing sweep would be new
   code.
4. **Path length bound.** Default: reuse 512. `HANDOFF_FILE_MAX_CHARS` is
   private to `ai-memory-mcp`, so the hooks crate needs its own constant or a
   shared one in `ai-memory-core`.
5. **`observations_for_file` in step 2 or with delivery.** Default: step 2, with
   test-only callers and the `FileSessionRef` result of section 3.6, because
   #1843 asks for isolation to be proven before delivery. Deferring it moves
   the three isolation rows of section 5 to the delivery PR.
6. **Single-file `apply_patch` only under an inactive policy.** Default: accept
   the asymmetry. Codex writes go through `apply_patch`, so leaving it out
   drops every Codex write from the index.
7. **Go/no-go threshold.** Default, as an initial proposal to recalibrate with
   the maintainer after the first measurement: go when, over a window of at
   least 7 days (one work week) that also meets the sample of section 3.8, both
   hold:
   - `current_client` coverage is at least 80%;
   - no client latency measure of section 3.8 fails its bound.

   Otherwise no-go, and the report names the counter or the measure that fell
   short. `population` coverage and prior-session hits are reported beside it.
   No external standard exists for this coverage; 80% means that at most one
   kept file event in five loses its path to the causes the counters list, and
   the counters show which cause to fix. The sample sizes and the latency rule
   come from section 3.8.
8. **Inspecting every tool event with the key on.** Default: yes, so
   `field_absent` and `multi_file` exist: older clients show up in `population`
   coverage, and calls the client skips without a field show up in
   `current_client`. The cheaper variant inspects only events that carry the
   field, and then neither ratio can see what never sends it.
