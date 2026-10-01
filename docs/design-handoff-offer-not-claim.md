# Design proposal: offer, don't claim, at session start (#959)

**Status: accepted design (maintainer review folded in, 2026-10-01). Option 1
is implemented on `release/2.6`; options 2 and 3 remain follow-up work.** The
maintainer marked #959 design-first: it touches the single-claim contract (invariant #16 — a handoff
is claimed exactly once by two independent `state='open'` guards) in several
places at once, so this is the design pass requested before any code lands.

## Problem

A pending handoff is claimed by whichever session starts next in the
project — any harness, any purpose, interactive or not. The claim is
single-use and cannot be undone. In practice the baton rarely reaches the
session it was meant for:

- an unrelated next session silently consumes it;
- continuing the same work in a different harness or TUI loses it to
  whatever started first;
- non-interactive launches (`opencode run`, scripted probes, `ai-memory run`)
  claim it too.

Observed on 2.4.1 (`433a19f3`): a Claude Code session left an automatic
handoff at 13:53:04Z; an interactive Codex session opened at 14:17:07Z for an
unrelated task consumed it, and a later `memory_handoff_cancel` on it
returned `cancelled: false, state: accepted` — already gone. A follow-up
report narrowed part of the original complaint to a self-inflicted cause
(calling `memory_handoff_begin` at the end of every session created a
*manual*, project-wide handoff, which `is_handoff_candidate` always prefers
over a directory-matched automatic one — working as documented, just not as
expected), but the remaining ask stood: the automatic claim at session start
has no opt-out, and `to_agent` is stored but never used to target delivery.

## Baseline before implementation

- **Claim site**: `fetch_and_accept_handoff_at` in `crates/ai-memory-hooks/src/router.rs`
  (~line 1355). Every `SessionStart` (and every `opencode run` /
  `ai-memory run` launch) calls this. It resolves the project, looks up
  `latest_open_handoff` (owner-filtered — `OwnerFilter::User`/`Unattributed`,
  so a shared server does not leak one operator's baton to another's
  session), renders it to markdown, then — inside the same admission-chain
  pass that notifies webhooks — claims it. A refused or timed-out admission
  chain cancels only the claim, leaving the handoff open; the content is
  still served either way (comment at ~line 1440 explains why: "the
  session-start claim is how most handoffs are consumed, so a webhook must
  be able to see it").
- **Selection**: `is_handoff_candidate` / `prefer_handoff` in
  `crates/ai-memory-store/src/reader.rs` (~line 10301, ~10363). A manual
  handoff (`from_session_id.is_none()`, always true for one created via
  `memory_handoff_begin`) is project-wide and always wins over an automatic
  one. An automatic (`SessionEnd`) handoff is scoped by cwd path-boundary.
  Owner is checked first, before the manual short-circuit, so cross-operator
  mixing is already excluded — this proposal only touches same-operator,
  same-project delivery. `startup_handoff` (`reader.rs`) also already skips a
  baton whose source session is still live (`LIVE_BATON_QUIET_PERIOD`, 10
  minutes, `router.rs`), so a session started next to a running one does not
  take its handoff.
- **Expiry**: handoffs already have an `expired` state. `SessionEnd` expires
  the same cwd's older automatic batons (`expire_same_cwd_auto_handoffs`,
  `ops.rs`) and a post-claim sweep expires superseded ones, so automatic
  batons do not pile up; only manual ones stay open until accepted or
  cancelled.
- **`to_agent`**: a column exists on the handoff row (`ai-memory-store`), is
  threaded through `row_to_agent_message`-adjacent plumbing, and is written
  — always as `None`. `memory_handoff_begin`'s args
  (`crates/ai-memory-mcp/src/server.rs` ~line 1188, `HandoffBeginArgs`) do not
  accept it at all. Selection never reads it. It is dead weight today.
- **Per-execution opt-out**: none exists for handoff delivery specifically.
  The closest precedent is `--no-autowire` / `AI_MEMORY_RUN_AUTOWIRE=false`
  (`crates/ai-memory-cli/src/cli.rs` ~line 312) for a different concern
  (harness hook/MCP autowiring on `ai-memory run`), and the generated
  OpenCode plugin's `fetchHandoff` call
  (`crates/ai-memory-cli/src/commands/install_hooks.rs`, four call sites:
  ~3464, ~4139/4249, ~4879/4915) has no guard of any kind — every
  `experimental.chat.system.transform` fetches and risks consuming the
  handoff.
- **A working non-consuming precedent already exists**, for a different
  resource: the inbox notice (V64), `render_inbox_notice` in `router.rs`
  (~line 1572), wired in at ~line 1557. It is appended to the same
  session-start context as the handoff, additive and non-destructive —
  `memory_message_pop` is what actually consumes a message, the notice just
  says a count is waiting. Its own doc comment is explicit about why:
  **"this carries ONLY a static integer count — never any message-controlled
  text (no subject, no sender string) — so a hostile message cannot inject
  text into the on-start context."** This is the precedent the maintainer
  pointed at ("inject a non-consuming notice... like the existing inbox
  notice"), and it comes with a security constraint this proposal has to
  reconcile, not just imitate the shape of (see Open questions, below).

## Proposed model

Three independent, additive pieces. Options 2 and 3 stand alone; option 1 is
the one that actually fixes the reported problem and is where most of the
design risk lives.

### 1. Offer, don't claim (config-gated, default unchanged)

New `[handoff]` config section (same shape as `AutoImproveSettings` and its
siblings in `crates/ai-memory-cli/src/config.rs`):

```toml
[handoff]
claim_on_session_start = true   # default: unchanged behavior
```

**Where the switch lives.** The initial implementation uses a server-wide
`[handoff]` key, which applies to every operator on a shared server. A
per-project `.ai-memory.toml` override remains a possible follow-up for teams
that need different behavior per repository.

When `true` (default — **no behavior change for existing installs**),
`fetch_and_accept_handoff_at` claims exactly as it does today.

When `false`, the same function stops short of the admission-chain claim and
instead renders a **notice**, following the inbox-notice pattern:
non-consuming, appended to the same session-start context, and claimed later
only through `memory_handoff_accept` (which already exists and already does
almost this exact lookup — this proposal does not add a second tool).

```
📬 ai-memory: a pending handoff `<id>` from `<from_agent>`, left `<age>` ago.
   To pick it up, call `memory_handoff_accept` with handoff_id `<id>`.
```

The notice must name the exact `handoff_id` and tell the agent to accept that
id. "Accept the latest" could claim a different handoff that arrived after the
notice was rendered; an exact-id accept just loses the race cleanly when the
notice is stale (both `state='open'` guards still decide).

**Managed-run ledger.** Today the managed-run context claim commits in the
same transaction as the handoff claim. In offer mode the ledger/context claim
still happens at session start — only the single-use handoff slot becomes an
offer — so a managed run does not lose its continuity packet.

The brief and managed-run context (`managed_md`) are unaffected either way —
only the single-use handoff slot's own claim becomes conditional.

### 2. Per-execution opt-out (additive, no schema change)

`AI_MEMORY_HANDOFF=off`, honored by every front door that can fetch a
handoff:

- the native `ai-memory hook` path (same place `capture_policy` already
  reads its own env knobs);
- the POSIX/PowerShell hook bundles (`hooks/<agent>/session-start.sh` and
  `.ps1` — mirrors the native check so a non-native install gets the same
  opt-out);
- the four generated TypeScript `fetchHandoff` call sites in
  `install_hooks.rs`, which today call it unconditionally;
- `ai-memory run --no-handoff` as the CLI-flag spelling, for scripted
  launches that would rather pass a flag than set an env var.

This covers automation and probes without touching `claim_on_session_start`,
and composes with it: an opted-out execution sees **no** claim and **no**
notice (see Open questions — this is the one place option 1 and option 2
interact).

### 3. Make `to_agent` load-bearing

- `memory_handoff_begin` gains an optional `to_agent` argument
  (`HandoffBeginArgs`, next to the existing `cwd`/`publish` fields).
- `is_handoff_candidate` gains one more condition: a handoff with
  `to_agent = Some(x)` is not a candidate for a session whose own agent kind
  is not `x`. Order relative to the existing owner check and the
  manual-beats-automatic short-circuit matters — see Open questions.
- Selection (`prefer_handoff`) is otherwise unchanged: `to_agent` filters the
  candidate set, it does not introduce a new ranking dimension.
- An explicit `memory_handoff_accept` with an exact `handoff_id` ignores
  `to_agent`: targeting only shapes automatic delivery and the notice, it never
  stops a user from deliberately picking a baton up in another harness.

## Open questions for review

1. **Does the notice carry a summary excerpt, or only metadata?** The issue
   asks for "first line of summary" in the notice text. The inbox notice's
   own security rule is stricter than that: *no* message-controlled text,
   specifically to block a hostile handoff summary from injecting
   instructions into a session that never asked to see it (a non-consuming
   notice is, by construction, something the receiving agent cannot choose
   to skip the way it can choose not to call `memory_handoff_accept`). A
   handoff's summary is written by whatever agent or operator ended the
   prior session — same trust level as a cross-project message. **Leaning
   toward metadata-only** (id, `from_agent`, age — no summary text), matching
   the inbox notice's own bar exactly, with the full summary available (as
   untrusted content to weigh, same framing the inbox notice uses for
   messages) only after an explicit `memory_handoff_accept`. This is a
   stricter proposal than the issue's literal wording and should be
   confirmed, not assumed.
2. **Interaction between the opt-out and `claim_on_session_start = false`**:
   should `AI_MEMORY_HANDOFF=off` suppress the *notice* too, or only the
   claim? Proposed: suppress both — an automated probe that does not want to
   consume a handoff almost certainly does not want to spend context on a
   notice about one either, and this keeps the opt-out a single on/off
   switch instead of three states.
3. **`to_agent` filtering order**: should a `to_agent`-mismatched handoff be
   invisible to a session of the wrong kind (falls through to the next
   candidate, e.g. an untargeted automatic handoff for that cwd), or does a
   mismatch block delivery entirely until the right agent shows up? Proposed:
   invisible-and-fall-through, consistent with "candidates" already being a
   filtered set before `prefer_handoff` ranks them — a targeted handoff
   simply removes itself from another agent's candidate list rather than
   occupying the slot.
4. **Existing deployed OpenCode plugins**: regenerating the opt-out support
   requires `install-hooks --apply` same as any other hook-shape change
   (already the documented path for e.g. `--capture-assistant`). Worth an
   explicit callout in the eventual changelog since this one is silent
   otherwise — an un-regenerated plugin simply keeps claiming unconditionally
   forever, which is not a crash, just a no-op upgrade.
5. **A handoff nobody ever explicitly accepts**: with `claim_on_session_start
   = false`, does an un-accepted handoff expire on its own? Partly answered:
   automatic batons are already expired by `SessionEnd` and the post-claim
   sweep (see Expiry above), so offer mode does not accumulate them. Only
   manual handoffs stay open until accepted or cancelled; no new TTL is
   proposed for those unless review disagrees.

## Non-goals (v1)

- No change to the owner-filter / cross-operator isolation already in place.
- No new MCP tool — `memory_handoff_accept` already does the claim; this
  only changes whether session start does it automatically first.
- No change to `memory_handoff_begin`'s existing manual-beats-automatic
  precedence rule, beyond adding `to_agent` as an orthogonal filter.
- No retroactive handling for handoffs already in flight when an install
  upgrades — the config default keeps today's behavior, so nothing changes
  until an operator opts in.

## Verification plan (when approved)

- Unit: `is_handoff_candidate` with a `to_agent` mismatch excluded from
  candidates, both for an otherwise-eligible automatic and an otherwise-
  always-winning manual handoff.
- Unit: `render_handoff_notice` (new, mirroring
  `inbox_notice_is_count_only_and_empty_at_zero`) — metadata-only, `None`
  when there is no pending handoff, never contains the stored summary text
  (a direct string-search assertion against a planted hostile summary, the
  same shape of test that would catch a regression into carrying
  message-controlled text).
- Integration: `claim_on_session_start = false` leaves a handoff in
  `state = 'open'` after a `SessionStart` that would have claimed it under
  the default; a subsequent `memory_handoff_accept` call claims it
  identically to today's automatic path.
- Integration: `AI_MEMORY_HANDOFF=off` suppresses both notice and claim for
  the native hook path and at least one of the generated bundle/plugin front
  doors (regenerated via `install-hooks --apply`), reusing this project's
  existing per-agent hook-bundle test fixtures rather than adding a new
  harness.
- Regression: existing handoff-selection and owner-filter tests in
  `reader.rs` continue to pass unchanged (this proposal adds a filter stage,
  it does not alter the ones already there).
