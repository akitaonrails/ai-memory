# Auto-improvement

## The 60-second version (read this first)

When an LLM provider is configured, the server periodically reviews
newly completed sessions and proposes small wiki edits — a new gotcha,
a rule promotion, a patch to a stale concept page. Every proposal is
validated (schema, confidence floor, size caps, eval gate) and staged
into `_pending/auto-improve/` with a human-readable sidecar, then
**auto-approved by default** through the normal wiki write path.

What that means for you:

- **Solo, zero-LLM install**: this never runs. Nothing to decide.
- **Solo with an LLM**: the default is fine — proposals are small,
  bounded, audited (every one lands in the pending-writes trail with
  evidence quotes and a confidence score), and reversible via wiki
  history. Run `ai-memory pending-writes list` for a week if you want to
  build trust — that (and SQLite) is the source of truth for a proposal's
  status. The `_pending/auto-improve/` sidecar files are a human-readable
  snapshot frozen at staging time; they do not track a proposal to
  approved/applied/rejected.
- **Shared / team server**: set `[auto_improve] require_approval =
  true`. On a server several people trust, an LLM should not
  auto-apply edits nobody reviewed — proposals then wait in
  `_pending/` until a human approves each one.
- **Cost**: one bounded LLM call per completed session that passes the
  preflight filters (minimum observations/duration); prompt and output
  sizes are capped in `[auto_improve]`.

A typical staged proposal sidecar looks like: the target path, the
operation (create or patch), a confidence like `0.86`, the rationale,
bounded evidence quotes from the session, and the exact body or edits
— enough to approve or reject without opening anything else.

The rest of this page is the original design research plus
implementation notes, kept for depth.

---

> Status: research plus implemented production notes. The server schedules
> auto-improvement for newly completed sessions in every project when an LLM provider is
> configured. Manual CLI/admin/MCP auto-improvement remains available for
> targeted runs and catch-up. Both paths record validated proposals in the
> pending-writes audit trail and auto-approve them through the normal wiki write
> path by default. Admins can set `[auto_improve] require_approval = true` for
> manual review.

## Executive Summary

An ai-memory equivalent of Hermes Agent's self-improvement loop is worth
shipping as a default-available, review-gated staging path. The current wiki
already captures useful durable knowledge: decisions, gotchas, concepts, rules,
notes, and session summaries. The missing piece is not more capture. It is a
careful reviewer that can identify durable lessons and apply small wiki patches
through the existing review/audit path without mutating the active agent context
or silently promoting weak session residue into rules.

The safe product shape is:

1. Keep automatic observation capture and session consolidation as they are.
2. Add a background review pass that creates pending wiki edits for newly
   completed sessions when an LLM provider is configured.
3. Record proposals and apply them through the approval/audit path by default,
   with manual approval available as an admin opt-in.
4. Keep a separate slow maintenance pass for deduplication, stale-page review,
   and lifecycle cleanup.

Do not copy Hermes' agent-local skill system directly. ai-memory's durable unit
is the project wiki page, not a `SKILL.md` package. The analogous targets are
`gotchas/`, `decisions/`, `concepts/`, `procedures/`, `_rules/`, small
`_slots/` state pages, and pending review pages under `_pending/`. The managed
ai-memory Agent Skills installed with routing are a narrow prompt-packaging
exception: static files that teach agents when to call MCP tools, not durable
memory pages or auto-improvement outputs.

## Hermes Findings

Hermes has two distinct learning loops.

### Post-Turn Review

The immediate loop runs after a successful turn when cadence counters trip.
Memory review is based on user turns. Skill review is based on tool-call
iterations. The observed defaults are 10 user turns for memory review and 10
tool iterations for skill review.

Important implementation properties:

| Property | Hermes behavior | Lesson for ai-memory |
|---|---|---|
| Active context | The review runs after the response is delivered. | Never compete with the user's active task. |
| Prompt mutation | Mid-session writes update disk but do not mutate the cached active prompt. | Background learning must not rewrite the current agent context. |
| Runtime inheritance | The fork inherits provider, model, auth, cached system prompt, session id, and toolset config. | Avoid model/cache drift when spawning auxiliary review work. |
| Tool restriction | The fork keeps the parent tool schema for prefix-cache parity, then enforces a runtime whitelist for memory and skill tools. | Safety should be enforced mechanically at dispatch/write time, not only by prompt text. |
| External memory plugins | The fork is created with `skip_memory=True` so it does not prefetch/sync external providers. | Do not let the review harness pollute independent memory systems. |
| Dangerous approvals | Background review installs an auto-deny approval callback. | A daemon review must never block on an interactive prompt. |
| Compression | Review compression is disabled. | The review should not race the parent session's lifecycle. |
| Visibility | Successful review actions are summarized to the user as self-improvement review output. | Autonomous memory changes need explicit provenance. |

The post-turn prompts are intentionally aggressive about capturing reusable
procedures, user corrections, and non-trivial techniques. They also include
negative filters: do not encode transient setup failures, negative claims that a
tool is broken, one-off task narratives, or failures that resolved before the
conversation ended.

### Write Approval

Hermes has an optional write-approval gate for persistent memory and skills.
The default is off, preserving existing behavior. When enabled:

| Case | Behavior |
|---|---|
| Foreground memory, interactive CLI | Prompt inline when possible. |
| Foreground memory, no prompt channel | Stage to pending storage. |
| Background memory | Stage to pending storage. |
| Hermes skill writes | Always stage, because skill files can be large. |
| User denies inline memory write | Block, do not stage. |
| Prompt machinery fails | Stage rather than silently dropping the write. |

Validation runs before staging, so invalid writes are rejected immediately
instead of being queued for approval and failing later.

This maps strongly to ai-memory. Wiki edits are closer to Hermes skills than to
small memory entries: they can be large, durable, and project-shaping. Staging
should be the default for autonomous ai-memory learning writes.

### Curator

Hermes' slower curator is a maintenance loop for agent-created skills. It is
triggered by inactivity rather than a cron daemon. Defaults observed in code and
docs:

| Setting | Default |
|---|---:|
| `interval_hours` | 168 hours, 7 days |
| `min_idle_hours` | 2 hours |
| `stale_after_days` | 30 days |
| `archive_after_days` | 90 days |

First-run behavior is deliberately conservative. A fresh install seeds
`last_run_at` and defers the first real pass by a full interval. Users can run a
manual report first.

Important curator properties:

| Property | Hermes behavior | Lesson for ai-memory |
|---|---|---|
| Managed scope | Primarily agent-created skills, tracked in `.usage.json`. | Separate user-authored pages from autonomous pages. |
| Destructive limit | Archive is the maximum automatic destructive action. No auto-delete. | Prefer supersession or soft deletion. |
| Pinned objects | Pinned skills bypass automatic transitions. | Pinned pages and invariant slots must be protected. |
| Reports | Writes machine-readable `run.json` and human `REPORT.md`. | Every maintenance run should leave an audit artifact. |
| Backups | Takes snapshots before mutating runs and supports rollback. | Wiki git commits help, but approval reports should still be explicit. |
| Report mode | Produces report-only output. | Non-destructive reports should be first-class for maintenance. |
| Consolidation | Merges narrow skills into umbrellas with structured summary. | ai-memory should consolidate duplicate/narrow pages separately from fresh lesson capture. |

## Live ai-memory Wiki Findings

The deployed homelab wiki was sampled on 2026-06-15. At the time of sampling it
contained 1 workspace, 38 projects, and 204 latest pages.

Page distribution by path prefix:

| Prefix | Count | Assessment |
|---|---:|---|
| `sessions/` | 57 | Useful episodic history, but also the main noise source. |
| `gotchas/` | 40 | High signal; concrete failure modes and fixes. |
| `decisions/` | 37 | High signal; durable rationale. |
| `concepts/` | 36 | High signal; architecture and domain knowledge. |
| `notes/` | 17 | Mixed; includes useful facts and smoke markers. |
| `_rules/` | 8 | High signal when concise and current. |
| `bootstrap.md` | 7 | Useful seed summary. |
| `_slots/` | 2 | Useful for current state, but stale risk is real. |

Representative high-signal pages:

| Page | Why it is useful |
|---|---|
| `ai-memory/gotchas/cli-is-always-http-client.md` | Captures a durable architectural rule, why it exists, exceptions, and prior-art failure modes. |
| `ai-memory/concepts/karpathy-wiki-pattern.md` | Explains the conceptual model behind the product. |
| `.config/notes/marvin-server-nfs-drop-rootcause.md` | Concrete root cause and fix with enough detail to prevent rediscovery. |
| `nes-to-sms/gotchas/vram-budget.md` | Domain-specific constraint that will matter across future work. |
| `akitaonrails-hugo/decisions/blog-content-sourcing.md` | Short decision with rationale and implementation guidance. |
| `llm-coding-benchmark/gotchas/hallucinated-apis.md` | Detailed, verified gotcha with examples and corrections. |

Representative low-signal pages:

| Page | Why it should not be promoted |
|---|---|
| `.config/sessions/0b9f6071-...md` | Three-second no-activity session. |
| `.config/sessions/1f8ffad8-...md` | Single `echo claude-bash-ok` test with no captured output. |
| `.config/sessions/cf81e9c3-...md` | Repeated bash smoke attempts; useful as diagnostics history only. |
| `.config/sessions/914f9f80-...md` | User prompt was only `config`; no substantive work. |
| `sabadell/sessions/8feda9e6-...md` | Heuristic session-end page with one observation. |
| `ai-memory/notes/livetest-v011-release.md` | Valid release smoke marker, but not a general lesson. |

The current system is already creating the right durable page families. The
auto-improvement opportunity is therefore selective promotion and cleanup, not a
new memory substrate.

## Recommendation

Build auto-improvement as a default-available scheduled review path that records
proposal provenance before writing target wiki pages. Manual CLI/admin/MCP runs
use the same pipeline for targeted review or historical catch-up. Scheduling and
approval are separate: the scheduler decides when to review newly completed
sessions, and the approval policy decides whether validated proposals are applied
immediately or left pending for humans.

The shipped feature is an audit-first learning reviewer:

1. Scheduled auto-improvement is enabled when an LLM provider is configured and
   `[auto_improve.scheduler] enabled = true`. Manual runs do not affect the
   scheduler.
2. Reads stored observations/session pages, recent pages, and relevant existing
   wiki pages. Capture exclusions are upstream of storage: excluded content
   cannot be recovered by `include_raw_fallback` because it was never stored.
3. Produces a structured proposal containing small page creates or updates.
4. Stores the proposal in a pending-review queue with evidence and diffs.
5. Applies approved proposals through `Wiki::apply_batch`, admission webhooks,
    auth capabilities, audit logging, and the single writer actor.

Auto-approval is the default, but it still records staged proposals and applies
them through the same approval path. Admins who want a human queue set
`[auto_improve] require_approval = true`; admins who want no automatic review set
`[auto_improve.scheduler] enabled = false`.

High-impact targets can also be guarded by an operator-supplied executable eval
gate. `[auto_improve.eval]` defaults to disabled; when enabled, proposals whose
paths match the configured prefixes (default `_rules` and `procedures`) are sent
to the configured command after LLM review validation and before staging or
auto-approval. The command receives JSON on stdin with proposal metadata plus
before/after bodies and must return JSON like
`{ "score_before": 0.72, "score_after": 0.76, "passed": true }`. Command errors,
timeouts, invalid JSON, `passed = false`, missing `passed`, or score deltas below
`min_delta` reject only that targeted proposal. Non-targeted proposals bypass the
gate. If all proposals fail eval, the run is still staged with zero proposals and
the rejected candidates so the rejection buffer can remember the failed attempt.
Hooks never run the eval command. See
[`auto-improve-eval-gates.md`](auto-improve-eval-gates.md) for the full stdin /
stdout contract and example scorer scripts.

## Proposed Page Targets

| Target | Use for | Notes |
|---|---|---|
| `gotchas/<topic>.md` | Reproducible pitfalls, root causes, tool quirks, failed approaches with a durable fix. | Require evidence and a correction or mitigation. |
| `decisions/<topic>.md` | Choices that changed architecture, workflow, dependencies, deployment, or policy. | Include decision, rationale, consequences. |
| `concepts/<topic>.md` | Stable domain or project architecture knowledge. | Prefer synthesis over task chronology. |
| `procedures/<topic>.md` | Reusable workflows, operating procedures, and repeated multi-step patterns. | Use when the value is the sequence, not only the root cause or rationale. |
| `_rules/<topic>.md` | Explicit always/never instructions for future agents. | Should also trigger the existing lint hint to update `AGENTS.md` or `CLAUDE.md`. |
| `_slots/current-focus.md` | Mutable short-term project state. | Treat as state, not durable truth; overwrite rather than append. |
| `notes/<topic>.md` | Useful facts that do not fit the above. | Avoid using notes as a dumping ground. |
| `_pending/auto-improve/<id>.md` | Human-reviewable staged proposals and diffs. | This is proposal storage, not approved durable knowledge. |

Do not create new session pages from the auto-improvement loop. Session pages
already come from session-end consolidation.

## Negative Filters

The review prompt should explicitly reject these as durable learning:

| Filter | Why |
|---|---|
| No-activity sessions | They add retrieval noise. |
| Single-command smoke tests | Usually operational evidence, not reusable knowledge. |
| Release markers | Keep as notes if needed, but do not promote to rules/gotchas. |
| Transient missing binaries, credentials, or setup state | These become stale false constraints. |
| Broad negative tool claims | `tool X is broken` hardens into future refusals after the tool is fixed. |
| One-off task narratives | Session pages already preserve chronology. |
| Resolved transient failures | Capture the retry or fix pattern, not the temporary failure. |
| User-visible status only | Use handoff or `_slots/current-focus.md`, not durable semantic pages. |

## Safety Invariants

Any implementation should preserve these invariants:

1. Scheduled and manual CLI/admin/MCP runs record proposals and auto-approve by
   default.
2. Scheduling and approval remain separate: disabling the scheduler does not
   require manual approvals, and requiring approvals does not stop scheduling.
3. Automatic SessionEnd triggering is off by default.
4. Never mutate the active session prompt or already-prepended handoff context.
5. Never run inside hook latency. Hooks remain fire-and-forget and bounded.
6. Never bypass workspace/project isolation. Use `ScopeResolver` or its explicit
   helpers for every read and write path.
7. Never bypass auth. Use `AuthLevel::authorize(Capability::...)` for all admin
   and write surfaces.
8. Never write wiki files directly from a handler or background worker. Use
   `Wiki::write_page`, `Wiki::apply_batch`, or existing destructive helpers.
9. Never auto-delete semantic pages. Use supersession, pending proposals, or the
   existing retention sweep for episodic pages.
10. Never rewrite pinned pages or invariant slots unless the proposal cites a
   direct contradiction and is explicitly approved.
11. Include source evidence for every proposed edit.
12. Attribute autonomous proposals to a distinct `auto_improve` actor so audit
    logs, admission webhooks, and review screens can distinguish machine-suggested
    changes from user/root writes.
13. Bound model cost, input size, output size, and number of proposed page
    mutations per run.
14. Write a machine-readable audit row for every run and human-readable proposal
    sidecars whenever proposals exist.

Capture policy is a client-side storage boundary, not a filter performed by the
reviewer. Reviewers can only consume what was stored; see
[Capture exclusions](marker-file.md#capture-exclusions).

## Existing User Upgrade Contract

Default-available auto-improvement must not surprise existing installs:

1. Existing project wiki folders need no migration. Older configs may still
   contain an `[auto_improve] mode = ...` key; current ai-memory ignores that
   legacy key. Operators can remove the line when convenient.
2. Session-end triggering stays off; the bounded background scheduler runs
   outside hook latency and sleeps for its configured interval after each
   non-overlapping all-project tick.
3. The scheduler initializes a first-run watermark per workspace/project and
   records a per-session claim before scheduled LLM work. Historical sessions are
   not reviewed automatically on upgrade, and failed scheduled reviews are not
   retried forever. Manual auto-improve remains the catch-up path for old or
   failed scheduled sessions.
4. Pending proposal storage must use additive, idempotent migrations that
    preserve all existing wiki files and session/observation rows.
5. Existing installed `CLAUDE.md`/`AGENTS.md` blocks remain valid. Operators pick
   up newer proactive retrieval guidance by running `ai-memory install-instructions`
   or asking an agent to refresh the ai-memory routing package. The marker-based
   replacement must remain idempotent, and the managed Agent Skill files should
   refresh from the same binary-owned assets as the slim snippet.
6. Target-page mutations must pass through proposal staging first and must keep
   approval attribution separate from the autonomous
   `auto_improve` proposal actor.

## Configuration Sketch

The exact names can change, but the shape should be explicit and conservative:

```toml
[auto_improve]
require_approval = false      # true leaves proposals pending for manual review
min_observations = 8
min_session_duration_secs = 120
min_confidence = 0.75
max_input_tokens = 24000
max_proposals_per_run = 5
max_patchable_pages = 8
max_patchable_body_chars = 8000
max_edits_per_proposal = 5
max_edit_content_chars = 4000
max_changed_chars_per_proposal = 12000
max_patch_edits_per_run = 8
max_rejection_context = 50
rejection_context_days = 180
max_final_body_chars = 32000
max_rule_page_tokens = 2000
max_procedure_page_tokens = 2000
include_raw_fallback = false
proposal_actor = "auto_improve"
pending_path = "_pending/auto-improve"

[auto_improve.scheduler]
enabled = true                # false disables background review only
interval_secs = 3600          # 0 disables background review only
max_sessions_per_tick = 1       # per project; ticks process projects sequentially
min_session_age_secs = 600
```

`[auto_improve.scheduler]` controls whether and how often the server launches
background review. `[auto_improve] require_approval` controls whether validated
proposals are applied automatically or left pending. They intentionally do not
imply each other.

`max_rejection_context` and `rejection_context_days` bound the persistent
rejection-buffer summary included in future reviewer prompts. The buffer is
scoped by `workspace_id` + `project_id` and stores human rejects, approval
conflicts/failures, and validator/model rejected candidates when they carry a
reason.

## Proposal Format

The LLM output should be structured JSON, validated before anything is staged:

```json
{
  "summary": "short human summary",
  "proposals": [
    {
      "operation": "create_or_update",
      "path": "gotchas/example.md",
      "title": "Example gotcha",
      "kind": "gotcha",
      "confidence": 0.82,
      "rationale": "why this is durable",
      "evidence": [
        {"page": "sessions/abc.md", "quote": "bounded quote"}
      ],
      "body_markdown": "# Example gotcha\n\n..."
    }
  ],
  "rejected_candidates": [
    {
      "reason": "single-command smoke test",
      "evidence": "sessions/xyz.md"
    }
  ]
}
```

Validation should reject proposals with missing evidence, wrong path prefix,
oversized bodies, attempts to mutate protected pages, unsupported operations,
or confidence below the configured threshold.

## Pending Review UX

The first production UX is explicit and audit-gated. The scheduler and
CLI/admin/MCP manual runs record validated pending proposals, then auto-approve
them by default through the wiki mutation path. With `require_approval = true`,
`pending-writes` applies or rejects them later.

| Command or route | Purpose |
|---|---|
| Background scheduler | Reviews newly completed sessions after the first-run watermark and applies or stages validated proposals according to approval policy. |
| `ai-memory auto-improve --session-id <id>` | Manually review one session and apply or stage validated proposals through the auto-improvement approval path. |
| `ai-memory auto-improve-report --workspace <w> --project <p> [--days N] [--limit N] [--stage]` | Read-only telemetry report for recent auto-improvement runs, proposal outcomes, terminal rates, and findings by default. `--stage` creates exactly one pending telemetry report page for audit/approval. |
| `memory_auto_improve` | Manually review the newest completed session with no persisted auto-improvement run, or explicitly rerun a named session, and apply or stage validated proposals through the same path. An empty run records a preflight skip so the next implicit call advances. |
| `ai-memory curator` | Rule-based, report-only maintenance review. |
| `ai-memory curator --stage` | Stage exactly one curator report page for pending-writes approval. |
| `ai-memory pending-writes list` | Show staged wiki changes. |
| `ai-memory pending-writes diff <id>` | Show markdown diff. |
| `ai-memory pending-writes approve <id>` | Apply through the normal wiki mutation path. |
| `ai-memory pending-writes reject <id>` | Discard proposal with audit trail. |

Pending proposals should be visible as markdown under `_pending/auto-improve/`
so humans can review them in the wiki/Obsidian workflow. SQLite can still hold
proposal state, approval status, evidence metadata, and audit rows, but the
review artifact itself should be inspectable and versioned like the rest of the
wiki.

On deployments that distinguish operators, manually staged proposals record
the qualified operator identity and enforce one pending proposal per target
*per operator*. Unattributed scheduler, curator, and telemetry proposals stay
in the shared bucket. A collision skips only that proposal, preserves its
siblings, and appears in the command or API response with the target and
reason; it must not become a silent partial run.

Because this is now an MCP tool surface, the standard prompt snippets, managed
Agent Skills, and regression tests assert `memory_auto_improve` appears in the
combined prompt-routing surface. Existing installed `CLAUDE.md`/`AGENTS.md`
snippets update idempotently when the operator runs
`ai-memory install-instructions` or asks an agent to refresh the ai-memory
routing package.

### Upgrade note for existing installs

Existing project wiki folders need no migration. Pending proposal storage is a
server-side database migration and sidecar directory.

Older server configs may contain a now-ignored `[auto_improve] mode = ...` key.
No data migration is required; remove the legacy line when convenient to avoid
confusion.

## Maintenance Loop Shape

Keep the curator analogue separate from the post-session learning reviewer.

The maintenance loop should handle:

1. Duplicate or near-duplicate titles.
2. Narrow pages that should be merged into a broader concept/gotcha.
3. Stale `_slots/current-focus.md` state.
4. Episodic pages that the retention formula marks cold.
5. Broken cross-references and contradiction candidates already surfaced by
   `memory_lint`.

The maintenance loop starts as report-only. `ai-memory curator --stage` stages
one normal report page under `notes/curator-<date>.md`; approving it records the
report only and does not perform the recommended maintenance actions. Later it
can stage merge or supersession proposals. It should not auto-delete semantic
pages.

## Implementation Phases

### Phase 1: Dry-Run Reviewer

Status: implemented for CLI/admin/MCP proposal staging plus default auto-approval.

Add a library-level reviewer that consumes one completed session plus existing
wiki context and returns validated proposals. The runtime stores pending
proposal rows plus sidecars first, then approves them through the wiki mutation
path unless `require_approval = true` is configured.

The implemented reviewer is designed for existing projects with large histories:
it treats the consolidated `sessions/<id>.md` page as the primary source when it
exists, then adds a bounded deterministic sample of raw observations selected
from start/end context, user prompts, high-importance events, error/fix/decision
keywords, and evenly spaced checkpoints. Validation rejects missing evidence,
unsupported paths, low confidence, oversized bodies, duplicate existing paths or
titles, and normalizes a missing H1 by prepending the proposal title before final
validation.

Tests:

1. Empty/no-activity sessions produce no proposals.
2. Single-command smoke sessions produce no proposals.
3. A session with a durable root cause produces one gotcha proposal.
4. A session with an explicit user rule produces one `_rules/` proposal.
5. Missing evidence rejects the proposal.

### Phase 2: Pending Wiki Writes

Status: implemented for CLI/admin staging, list/show/diff, approve, and reject.

Durable pending proposal storage lives under `_pending/auto-improve/` as
non-indexed sidecars plus SQLite rows, with list/diff/approve/reject commands
and audit rows. Approval applies through the existing wiki mutation boundaries
with the `auto_improve` actor preserved in proposal provenance.

Tests:

1. Pending proposals survive restart.
2. Approval writes files and index rows atomically.
3. Rejection never writes a wiki file.
4. Protected page proposals are rejected before staging.
5. Cross-workspace partial scopes fail closed.
6. Pending proposal markdown is created under `_pending/auto-improve/` and never
   indexed as approved durable knowledge.
7. Approval/audit metadata preserves `auto_improve` proposal attribution and the
   approving actor separately.

### Phase 3: Background Scheduler

Status: implemented as a server-side scheduler, not a SessionEnd hook trigger.
The scheduler is enabled by default when an LLM provider exists, sleeps after
each non-overlapping tick, and reviews newly completed sessions across every
project after each project's persisted first-run watermark. It records a
per-session claim before calling the LLM, so scheduled review is at-most-once per
completed session unless an admin reruns auto-improve manually.

Tests:

1. Disabled scheduler config does nothing while manual runs still work.
2. Enabled scheduler reviews only post-watermark sessions.
3. Saturated or failed review leaves normal session-end behavior intact and does
   not retry the same session forever.
4. Run rows include model/provider, scope, proposals, rejections, config, and
   scheduler trigger metadata.

### Phase 4: Maintenance Curator

Add a separate scheduled report that uses existing lint, access counters,
retention scoring, links, and page metadata to propose merges/supersessions.

Tests:

1. First run defers or reports before mutating behavior is possible.
2. Pinned pages and invariant slots are skipped.
3. Semantic pages are never hard-deleted.
4. Proposed merges identify source and destination pages with evidence.

## Resolved Design Choices

1. Pending proposals should be a first-class wiki structure under `_pending/`,
   with SQLite retaining state and audit metadata.
2. Procedural lessons should have a `procedures/` page family instead of being
   forced into `gotchas/` or `concepts/`.
3. Autonomous proposals should be attributed to a distinct `auto_improve` actor,
   with approval attribution tracked separately.
4. The minimum confidence threshold should be configurable and calibrated with
   applied proposals on real projects before any future unattended trigger is considered.

## Current Conclusion

Hermes validates the idea, but also shows why the boundaries matter. The useful
part is not that the agent can write memory by itself. The useful part is a
bounded, observable, reviewable loop that turns repeated work into durable
knowledge while keeping active task execution isolated.

For ai-memory, the current correct boundary is scheduled review plus pending
proposal storage under `_pending/auto-improve/`. Approval policy is separate:
default auto-approval keeps the wiki compiling forward, while
`require_approval = true` gives admins a human queue without disabling the
learning loop.
