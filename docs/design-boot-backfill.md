# Design: boot-time backfill of pre-hook local history

**Status: implemented on `release/2.3`.** This is the design pass for the "pre-load
existing session history when the store is new" idea; it adds a default-on capture
behavior and touches the trust boundary (retroactive untrusted text enters the store).

**As-built note.** Two deviations from the approved plan, both verified by a live smoke:

1. **Ingest path is B, not A.** The plan recommended path A (reuse the managed-workstream
   begin → link → finish endpoints, for the incremental cursor). A live smoke proved A
   populates the *workstream continuity ledger*, not the `sessions`/`observations`/`pages`
   pipeline — `observation` count stayed zero after import, so the history was not
   recall-able via `memory_query`. The implementation therefore **replays each transcript
   through `/hook/batch`** (the same ingress live capture uses, as the companion importer
   does): `export_transcript` normalizes the native transcript, then each event is posted
   as a session-start / `user-prompt` / backfill-extension observation, attributed to the
   original harness + native session id. The re-run smoke confirmed
   `sessions 0→1, observations 0→5, pages 0→1`, by-agent `claude-code: 1`, and a correct
   no-op on the second run.
2. **No new lease table (V65 dropped).** "Import exactly once" is held by the emptiness
   gate (only bootstrap a project with zero captured sessions) plus a per-checkout local
   sentinel (`<data_dir>/backfill-state/<hash(cwd)>`) that stops the SessionStart trigger
   re-spawning. Live hook capture (install-time forward) and backfill (before-install
   history) do not overlap, so there is no double-capture to reconcile.

`backfill --dry-run` only plans the import; it neither creates nor overwrites
the local sentinel, so it does not suppress a later automatic attempt.

The automatic path runs detached and silent (like Claude Code's own auto-memory); the
`📼 imported N session(s)` summary is shown on a **manual** `ai-memory backfill` only.
Surfacing the count in the next session's on-start context is a possible follow-up
(§ "Notice delivery" describes the delivery path it would reuse).

## Problem

Capture is forward-only. The lifecycle hooks record events *from the moment they are
installed*. If you have already been working in a project for weeks — a long Claude or
Codex session on disk — and only now install ai-memory, none of that prior context is
in memory. You get a "cold start": the tool that exists to give cross-session continuity
begins with amnesia about the very session you are resuming.

The `ai-memory doctor` command (shipped alongside this proposal) makes the related gap
visible — "this harness ran here but captured nothing." This proposal fills a specific
instance of it: **when a project's store is brand new (empty) and the harness is resuming
a session that already has local transcript history, import that history once, so the
first consolidated pages reflect what you actually did rather than nothing.**

## What already exists (do not rebuild)

The hard part — read a native transcript, sanitize it, get it into the store — is built
twice already. This proposal is a *trigger and guards* around existing ingest, not a new
importer.

- **`ai-memory-workstream` transcript adapters** read every supported harness's native
  session store read-only, keyed by cwd, with per-harness path/id encoding in one place
  (`export_transcript`, `list_native_sessions`, `discover_native_session`). `doctor`
  already reuses `list_native_sessions`; boot-backfill reuses `export_transcript`.
- **`export_transcript`** returns an `ExportedTranscript { native_session_id, events:
  Vec<NewWorkstreamEvent>, source_cursor, losses }` — a bounded, sanitizable visible-event
  ledger plus an **opaque incremental cursor**. `ai-memory run` calls it after a managed
  child exits and POSTs the events in size-bounded batches (`FinishManagedRunRequest`,
  `event_batches`) to the managed-workstream finish endpoint; the server persists
  `source_cursor` so the next read is incremental. This is a working backfill engine that
  only ever runs *after* a managed launch today.
- **The companion importer** (`companions/ai-memory-importer`) has `ExternalConversation`
  — "replay a generic external conversation through ai-memory's hook API" — with exactly
  the bounding discipline this needs: `MAX_CONVERSATION_MESSAGES = 128`,
  `MAX_USER_MESSAGE_BYTES = 16 KiB`, `MAX_CONVERSATION_TOTAL_BYTES = 1 MiB`, a dedicated
  `external-import` agent kind, all through the sanitizer.
- **The sanitizer** (`ai-memory-hooks`, `Sanitized<NewObservation>`) is the only path from
  untrusted text into the store, and it is enforced by construction (invariant #6).
- **Detached background work from the hook path** already exists: the native hook spools
  events and drains them via a separate detached process (`hook_drain_process`), so hooks
  stay within their ≤200 ms budget (invariant #5). Boot-backfill borrows this pattern.

## Decision (confirmed with the operator)

- **Default on**, with a clean opt-out.
- **Full project bootstrap**: when the store is empty on first boot in a project,
  import *all* local sessions for this cwd across every supported harness, not just the
  current resumed session — hard-capped, with a one-line notice printed.

## Design

### 1. Trigger — the SessionStart boot handshake

The SessionStart hook already fires and already contacts the server (it fetches the
hot-context/handoff block). Extend that one round-trip to also learn, cheaply:

- **Is this project's store empty?** A scoped count — the same family as doctor's
  `GET /admin/sessions/by-agent` for `(workspace, project)`; sum of sessions == 0 means
  empty. Emptiness is the gate: an established project is never retro-imported (that would
  duplicate/rewrite real history and confuse consolidation). Boot-backfill is a strictly
  **one-time bootstrap**.
- **Backfill enabled?** `[capture] backfill_on_start` (default true), overridable by
  `AI_MEMORY_BACKFILL_ON_START` and a `.ai-memory.toml` marker key.

If empty AND enabled, the hook **spawns a detached `ai-memory backfill` process** and
returns immediately. The hook never blocks boot and never does the import inline
(invariant #5). If not empty, or disabled, the hook does nothing new.

### 2. Worker — `ai-memory backfill` (new thin CLI subcommand)

A detached, read-only-on-the-local-side worker. For the resolved `(workspace, project)`
and cwd:

1. Re-check emptiness server-side and take a **bootstrap lease** (a single-writer guarded
   claim, like handoff/message claim-once) so two harnesses booting at once cannot both
   import. Lease keyed by `(workspace_id, project_id)`; second claimant no-ops.
2. Enumerate local sessions for the cwd across every supported harness (`doctor`'s
   `scan_local` set), newest first.
3. For each session, up to the caps below, `export_transcript(...)` and ingest the
   events, attaching to the harness-native session id so the imported session id matches
   what forward capture would have used (Claude/Codex local id == server session id).
4. Persist each session's `source_cursor` so a later real resume continues incrementally
   from where the backfill stopped — no double-capture of the overlap.
5. Print one notice to the operator's next surface: `📼 ai-memory imported N prior local
   sessions (~M events) for this project. Opt out with AI_MEMORY_BACKFILL_ON_START=0.`

Manual control: `ai-memory backfill` can be run by hand (idempotent), with
`--all-sessions` / `--session <id>` / `--dry-run` / `--force` (re-import a non-empty
store, explicit) for operators who want it outside the boot path.

### 3. Ingest path — two options

The events from `export_transcript` are `NewWorkstreamEvent`s. Two ways to land them:

- **(A) Reuse the managed-workstream finish endpoint** (`FinishManagedRunRequest` +
  `source_cursor`, batched by `event_batches`). Richest fit: the cursor and incremental
  semantics already exist and the server already consolidates workstream ledgers. Cost: it
  is currently coupled to a managed *run* lifecycle (a `WorkstreamCheckpoint`, an
  `exit_code`); backfill has no live child, so it needs a "finish without a run" entry —
  a synthetic/adopted workstream for the pre-hook history.
- **(B) Replay as sanitized hook observations** (the importer's approach). Simpler and
  lifecycle-free; integrates with the ordinary capture/consolidation path. Cost: needs a
  `NewWorkstreamEvent → NewObservation` mapping and its own idempotency (no built-in
  cursor).

**Recommendation: (A)**, extended with a lifecycle-free "import" variant of the finish
endpoint. It preserves the incremental `source_cursor` contract end to end (critical for
step 4 — the overlap between backfilled history and the resumed live session must not be
captured twice), and reuses the server's existing workstream consolidation rather than
inventing an observation mapping. The cursor is the load-bearing reason to prefer A.

### 4. Idempotency and overlap

Three guards, all reusing existing mechanisms:

- **Emptiness gate** — only an empty store bootstraps; a populated store is never touched
  by the automatic path.
- **Bootstrap lease** — claim-once per project (handoff/message pattern) prevents
  concurrent double-import across simultaneous boots.
- **Per-session `source_cursor`** — the imported session's cursor is persisted, so when
  the operator's live resume then produces new events, forward capture continues *after*
  the cursor. No event is both backfilled and live-captured.

### 5. Caps (hard, non-negotiable)

Full-project bootstrap can be large (a single real Codex session was ~18.5k events).
Bound everything, defaults chosen to protect boot and the single-writer actor:

- `max_sessions` (e.g. 25 newest local sessions per project).
- `max_events_total` and `max_bytes_total` across the whole bootstrap.
- `max_events_per_session` (favor breadth over one giant session).
- Per-event/message byte caps inherited from the sanitizer + importer constants.
- Ingest in the existing size-bounded batches; never one unbounded POST.
- On hitting a cap: import what fits (newest-first), record the truncation in `losses`,
  print it in the notice. Never silently drop without a trace.

### 6. Security

Retroactive text is exactly as untrusted as live hook text — it is old model/tool output.

- **No sanitizer bypass.** Every imported record passes the same
  `Sanitized<NewObservation>` / workstream sanitize boundary and the same caps. The e2e
  canary secret must be scrubbed from a backfilled body just as from a live one.
- **Capture exclusions honored.** `[capture] ignore_paths` from the nearest
  `.ai-memory.toml` must drop matching file-tool events in the backfill path too, not just
  live hooks.
- **Emptiness + lease** prevent a backfill from overwriting or racing an established
  project's real history.
- **Multi-user posture.** On a shared server, default-on retro-import of a long local
  history is more sensitive than on a single-user box. Backfill attributes to the booting
  operator, imports only into an *empty* project scope, and is opt-outable per project via
  the marker. Consider gating the automatic path to the default (loopback/single-operator)
  posture and requiring explicit `ai-memory backfill` on `deployment_distinguishes_operators()`
  servers — an open question below.

### 7. Opt-out and configuration

- `[capture] backfill_on_start = true` (default) in config.
- `AI_MEMORY_BACKFILL_ON_START=0` env override (one config-read path — read in
  `Config::load`, never ad hoc).
- `.ai-memory.toml` marker key so a specific project can opt out regardless of global
  config (mirrors how `ignore_paths` is project-scoped).
- Manual `ai-memory backfill` is always available regardless of the auto setting.

### 8. Harness coverage

Exactly the set with read-only native transcript adapters (doctor's `SCANNED_HARNESSES`).
A harness whose store is unreadable/absent contributes nothing — backfill never invents
history it cannot read, same conservative stance as doctor.

## Relationship to `ai-memory doctor`

Complementary: **doctor detects** the coverage gap (a harness ran here but captured
nothing); **backfill fills** the specific first-boot instance of it. The docs should
cross-link them — doctor is the ongoing check, backfill is the one-time bootstrap.

## Testing plan

- **Pure**: cap enforcement (sessions/events/bytes, newest-first truncation), emptiness
  gate decision, enabled/disabled resolution, overlap/cursor bookkeeping.
- **Detector reuse**: backfill's local enumeration shares doctor's `scan_local`; the
  planted-transcript fixture pattern extends to "export + ingest a planted Claude session
  into an empty in-memory store, assert observations land, sanitized, once."
- **Idempotency**: run backfill twice → second run no-ops (lease + non-empty gate); a
  live resume after backfill does not double-capture the overlap (cursor).
- **Security**: a canary secret in a planted transcript is scrubbed; an `ignore_paths`
  match in a planted file-tool event is dropped.
- **Multi-session**: two concurrent boots → exactly one import (lease claim-once), the
  other no-ops — the invariant-#16 concurrency shape, proven at integration level.

## Open questions — resolved as built

These were the pre-implementation questions; the shipped feature (see the **As-built
note** at the top) resolved each:

1. **Ingest path A vs B** — resolved to **B** (replay through `/hook`). A live smoke proved
   path A populated the workstream continuity ledger, not the searchable memory pipeline
   (observation count stayed zero), so it was abandoned.
2. **Shared-server default** — shipped **on by default** with `AI_MEMORY_BACKFILL_ON_START=false`
   / `--no`-style opt-out; the emptiness gate makes it safe on any posture (it only ever
   bootstraps an empty project).
3. **Notice delivery** — the automatic path runs detached and silent (like Claude Code's
   own auto-memory); the summary shows on a manual `ai-memory backfill`. Surfacing it in
   the next session's on-start context remains a possible follow-up.
4. **Consolidation quality** — shipped **behind the hard cap** (newest 25 sessions / 50k
   events) and iterating; a dedicated eval remains a follow-up.

## Semver

Additive (new subcommand, new config key, new default-on behavior, no breaking change) →
**minor**, targeting `release/2.3`.
