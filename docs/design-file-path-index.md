# Design proposal: opt-in file path index (#1843)

**Status: open design proposal, not implemented, with no assigned release.**
Issue [#1843](https://github.com/akitaonrails/ai-memory/issues/1843) remains
open. Any implementation must rebase the migration number and branch
assumptions below against the then-current tree.

The maintainer set the order in
[#1843](https://github.com/akitaonrails/ai-memory/issues/1843#issuecomment-6508464269):
specify the opt-in path index, then design delivery separately, then measure
before expanding. This note covers only the index. Section 10 lists the
decisions for the maintainer.

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

Checked against `main` at `ac699b56`. References name the file and the symbol.

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
version (3.2), so a side table would only add a join. Because the path lives on
the observation row, every lifecycle operation in 3.5 applies to it with no new
code.

### 3.2 Where the path comes from

Only the two front doors that implement capture policy v1 produce a path: the
native client and the generated TS integrations. They add a reserved top-level
field, versioned like `_ai_memory_assistant`:

```json
"_ai_memory_file": { "version": 1, "path": "src/lib.rs" }
```

**When the field is sent.** All three must hold:

- the disposition is Keep, under an active or an inactive policy;
- the family is `file`;
- the call names exactly one file: a direct path (extraction state
  `extracted`, `path_count` 1), or an `apply_patch` / `ApplyPatch` whose
  envelope has exactly one file header, read with the existing `patch_paths`.
  A `Move to` header names two files and is skipped.

Drop and MetadataOnly never carry the field, so an ignored path never leaves
the machine through it. Under an inactive policy the kept payload already
carries the raw path, so the field exposes nothing new; it only carries the
normalized form the server cannot compute (3.3). An `apply_patch` only reaches
this point under an inactive policy (section 2); section 10, question 6, asks
whether that is acceptable.

The client never reads server config, so the field goes out on every such
event even when the server key is off. The cost is one short JSON object per
kept file-tool post, which the server removes on arrival (3.4).

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
2. Strip the `checkout_root` prefix. A path outside the checkout is not indexed.
3. Use `/` as the separator; keep case as given.
4. Refuse: an empty result, a leading `/` or drive letter, any `..` segment, more
   than 512 characters (the bound `HANDOFF_FILE_MAX_CHARS` uses for stored
   paths).

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

- `[ingest] index_file_paths = true` (default `false`; 3.7);
- the field parses with `deny_unknown_fields` and the current version;
- the server's own inspection of the event yields Keep, family `file` and
  exactly one file. Under an inactive policy the client attaches no claim, and
  `inspect_capture_envelope` returns such an event without inspecting it, so
  the server runs `capture_inspector(...).inspect` itself, as
  `inspect_capture_envelope` already does for a claim: with the claimed policy
  state, or `Inactive` when no claim is attached. For an `apply_patch`, the
  single file is checked with `patch_paths`. This runs only when the key is on
  and the field is present;
- the path passes the step 4 checks;
- `scrub` leaves the path unchanged, checked on the path with a leading `/`
  prepended, because the credential-directory patterns need a separator before
  the directory (`.ssh/config` at the checkout root would otherwise pass).

The path enters the store through the typed boundary: `NewObservation` gains
`file_path: Option<String>`, and `Sanitized::<NewObservation>::new` runs the
checks above and clears the field when one fails. A failed check drops the field and keeps the
observation exactly as today.

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

### 3.6 Read authorization

One internal store function, with no MCP tool, HTTP route or CLI command in
this step:

```rust
observations_for_file(scope: (WorkspaceId, ProjectId), owner: &OwnerFilter,
                      file_path: &str, limit: usize) -> Vec<ObservationRef>
```

- `ObservationRef` is a new type: the observation id and its `created_at`.
- Scope comes from the no-create lookup in `ScopeResolver`; no `global`,
  `scopes` or `[recall] default_global` broadening.
- The owner filter reuses `sessions_hidden_from`: another operator's session is
  hidden, shared (`NULL` owner) sessions are visible.
- `limit` is bounded; the result carries ids and timestamps, not bodies.

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
  post.
- **6 (sanitizer boundary):** the field is validated and scrub-checked inside
  `Sanitized::<NewObservation>::new`, so it reaches the store only through
  `Sanitized<NewObservation>`.
- **16 (pages shared, batons owned):** the index belongs to observations, which
  are owned through their session; pages are untouched.

## 5. Trust and security

New rows for `docs/security-boundaries.md`. Each test fails with its guard
removed and passes with it:

| Guard | Enforcing code | Adversarial test |
|---|---|---|
| A path never crosses projects | `observations_for_file` scope predicate | Same `file_path` in two projects; a query from one never returns the other's rows. Control: same project returns them. |
| A path never crosses operators | `sessions_hidden_from` in `observations_for_file` | Operator B's session is hidden from A; owner and root see it; a shared session is visible to both. |
| An ignored path is never stored | client Drop and MetadataOnly carry no field | Marker ignores `secret/`; a Read there stores no row and no `file_path`. |
| A forged or malformed field is not stored | server checks in 3.4 | Absolute, `..`, over-long, credential-directory, wrong-version and non-file-family fields are dropped, as is a field on a patch that names two files; the observation is kept and the field never reaches storage. |
| Off means off | 3.4 key check | With the key off, a valid field stores nothing. |

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
- New client with an older server: the field is ignored. Older client with a
  newer server: no field, no path.
- No MCP, HTTP, SDK or routing-snippet change.

## 7. Tests

**Unit**

- Rust: normalization (relative, absolute, Windows spelling, worktree), each
  refusal in 3.3, the scrub check including `.ssh/config` at the root. A
  symlinked cwd is not resolved: a cwd whose lexical form is outside the
  checkout yields no field.
- Rust, `apply_patch`: one `Add File`, `Update File` or `Delete File` header
  yields the field; two files, a `Move to` header, an empty or malformed patch,
  and an active policy yield none. The same table for Droid `ApplyPatch`.
- Generated TS: the Node checks `render_shared.rs` already runs for
  `ts_capture_policy_v1`, extended to the field under active and inactive
  policies, and to its absence on Drop and MetadataOnly.

**Integration**

- `crates/ai-memory-store/tests/suite/multi_session.rs`, `project_authz.rs`: the
  project and operator rows in section 5.
- Hooks router tests, on `handle_hook` and the batch route: forged and
  malformed fields, key off, no field from a legacy-shaped payload, a field on
  an event without a claim that the server's inspection rejects, the
  observation kept in every refusal; a Codex-shaped `apply_patch` PostToolUse
  with one file stores its path, one with two files stores none.
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

**Measurement** (before any delivery design): on a real store with the key on,
report the share of live file-tool events that got a path (backfill imports
never carry the field, so they stay out of the denominator), the number of paths
touched in two or more sessions, and the stored bytes per thousand
observations. Delivery is designed only if the share reaches the threshold in
section 10, question 7.

## 8. Pull request plan

1. This note, against `main` because it is docs only (as #1319).
2. Index, against the standing feature branch (currently `release/3.4`):
   migration, both front doors, server acceptance, `observations_for_file`,
   config key and docs, the section 5 rows and the section 7 tests, CHANGELOG.
   Off by default.
3. Measurement on a real store, published with the PR or right after it.
4. Delivery: a separate design note, starting with PostToolUse in one harness,
   once per path per session, untrusted-data framing, no global broadening.

## 9. Alternatives

- **Text matching only (option A in #1843):** no storage change, but the
  tokenizer misses absolute paths in tool output and coverage without LLM
  consolidation is low. Rejected by the maintainer as a basis for delivery.
- **A side table:** fits several paths per call (a multi-file `apply_patch`), at
  the cost of a join and its own lifecycle. Deferred until multi-file calls are
  in scope.
- **The path inside `_ai_memory_capture`:** breaks older servers (3.2).
- **A protocol version bump:** same effect on older servers.
- **A hash instead of the path:** a repository path has little entropy, so the
  hash hides little, and it makes the index unreadable for diagnosis.
- **Server-side normalization:** the server sees the cwd, not the checkout
  root, so worktrees would not share keys.

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
   test-only callers, because #1843 asks for isolation to be proven before
   delivery. Deferring it moves the two isolation rows of section 5 to the
   delivery PR.
6. **Single-file `apply_patch` only under an inactive policy.** Default: accept
   the asymmetry. Codex writes go through `apply_patch`, so leaving it out
   drops every Codex write from the index.
7. **Measurement threshold.** The share of file-tool events with a path below
   which delivery is not designed. No default.
