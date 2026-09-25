# Security & isolation boundaries — inventory and adversarial-test map

ai-memory is single-tenant wiki data with optional multi-user attribution, run
by parallel harnesses and shared teams. A handful of guards keep one project,
workspace, operator, or untrusted input from crossing into another. Each guard
is only as good as a test that **actively tries to break it** — a happy-path or
single-tenant test cannot see an isolation defect, so a regression that removes
the guard would pass CI silently.

This file is the source of truth for which boundaries exist, where each is
enforced, and the adversarial test that would fail if the guard were removed.
**Keep it current** (see the protocol at the end) — it is referenced by the
AGENTS.md "security-boundary tests" rule.

An adversarial test = *attempt the violation and assert refusal, plus a
legitimate control case* (so a blanket deny is not mistaken for a working
guard). Coverage verdicts: **STRONG** = a test would fail if the guard were
deleted; **PARTIAL** = only some paths of the guard are probed; **FUTURE** =
boundary not yet built.

## Boundary map

| # | Boundary | Enforcing code | Adversarial test(s) | Coverage |
|---|----------|----------------|---------------------|----------|
| 1 | Per-project isolation (3-tuple) | `ai-memory-store/src/scope.rs` `ScopeResolver::resolve_read_args`/`resolve_write_args`, no-create `lookup_existing_scope`; reader queries filter by `(workspace_id, project_id)` | `store` `scope.rs` read-resolution table tests; `tests/suite/multi_session.rs` | STRONG |
| 2 | Workspace isolation | same as #1; same-named project → distinct ids per workspace | `scope.rs` cross-workspace resolution rows; `multi_scope` dedup/validate | STRONG |
| 3 | Multi-user auth ladder | `ai-memory-core/src/actor.rs` `AuthLevel::authorize`; `ai-memory-mcp/src/auth.rs` middleware; `admin.rs` `require_root_for_multiuser_admin` / `require_root` | `admin.rs` `multiuser_admin_routes_reject_db_user_tier` / `…reject_anonymous` / `create_user_as_user_tier_returns_403`; `auth.rs` unknown-bearer 401; `actor.rs` `skip_admission_chain_rejects_db_users` | STRONG |
| 4 | Handoff single-claim / no-steal | `ai-memory-store/src/ops.rs` `accept_handoff_in_transaction` (metadata `state='open'` guard + atomic CAS) | `multi_session.rs` `a_second_accept_cannot_steal_an_accepted_handoff`; `handoff_ownership.rs` `another_operator_cannot_claim_the_handoff` | STRONG |
| 4b | Handoff owner-scoped recovery (`any_owner` admin gate) | `ai-memory-mcp/src/server.rs` `require_admin_capability` on `memory_handoff_accept`/`_cancel` `any_owner` | `handoff_admission.rs` — cancel gate + **accept gate** (adversarial: non-admin `any_owner` accept refused) | STRONG |
| 5a | Pages shared: `author_id` is never a read filter (invariant #16) | `ai-memory-store/src/reader.rs` `search_pages`/`page_body_by_ids` — `author_id` is an attribution JOIN only, never a WHERE term | `multi_session.rs` — a page with a **non-null** `author_id` (operator A) is readable by operator B in the same project | STRONG |
| 5b | Page supersession (loser stays reachable) | `ai-memory-store/src/ops.rs` `upsert_page_in_tx` — demote `is_latest=0` (never delete) + `supersedes` chain | `multi_session.rs` `concurrent_writes_to_one_path_supersede_rather_than_destroy`; `retrieval_superseded.rs` | STRONG |
| 6 | Active-project pointer (PerActor, no clobber) | `ai-memory-core/src/active_project.rs` `set_for`/`lookup_for` (fail-closed on `Mismatch`) | `active_project.rs` `parallel_harnesses_of_one_user_keep_separate_pointers`, `two_operators_never_read_each_others_pointer`, `a_session_mismatch_fails_closed_once_anything_has_been_keyed` | STRONG |
| 7 | Sanitizer trust boundary (invariant #6) | `ai-memory-core/src/sanitize.rs` `Sanitized<T>` (private field, only `sanitize()` ctor); `WriterHandle::insert_observation` requires `Sanitized`; `ops` crate-private | Structural (compile-time) + `store/src/lib.rs` `insert_observation_boundary_scrubs_before_disk` + sanitizer scrub unit tests | STRONG (structural) |
| 8a | Messaging: recipient-only visibility | `ai-memory-store/src/ops.rs` pop target-select + `reader.rs` `list_messages` (`to_*` predicate) | `agent_messages.rs` `a_message_is_only_visible_to_its_recipient`; `agent_messages_tools.rs` non-recipient cannot pop | STRONG |
| 8b | Messaging: cancel-own-only | `ai-memory-store/src/ops.rs` `cancel_messages` (AND-gated on `from_*` sender coordinate) | `agent_messages.rs` — whole-outbox **and** a foreign **specific-id** cancel refused | STRONG |
| 8c | Messaging: pop-exactly-once | `ai-memory-store/src/ops.rs` `pop_message_in_transaction` atomic CAS `WHERE state='pending'` | `agent_messages.rs` — sequential **and** `tokio::join!` concurrent double-pop yields exactly one `Some` | STRONG |
| 8d | Messaging: inferred-scope read is diagnosed (#854) | `scope.rs` `is_inferred`; `server.rs` `inferred_scope_hint` on empty pop/list | `agent_messages_briefing.rs` `no_scope_pop_that_misses_the_mail_is_diagnosed_not_a_silent_null` | STRONG |
| 9 | Scope resolution fail-closed | `ai-memory-store/src/scope.rs` no-create `lookup_existing_*`; create only via `create_explicit_scope` | `scope.rs` no-auto-create + `unscoped_write_with_unresolvable_coordinate_errors` | STRONG |
| 10 | Destructive-op live-process refusal + confirm flags (invariant #9) | `ai-memory-cli/src/commands/process_guard.rs` `sibling_processes` + confirm flags in `reset`/`restore`/`reindex`/`uninstall --purge-data`/`purge_project` | `admin_purge.rs` confirm→400; `removal.rs` — injected live-sibling makes each destructive command bail before touching the data dir | STRONG |
| 10b | Session purge is scope+owner-bound (no cross-session/project over-delete) | `ai-memory-store/src/ops.rs` `purge_session` — selection scoped to `(workspace_id, project_id)` and keyed on this session's own `summary_page_id` **or** `path='sessions/<sid>.md'` + `json_extract(frontmatter_json,'$.session_id')=<sid>` (frontmatter owner, not the recursive latest-chain); `in_scope==0 → NotFound` fail-closed; whole op in one transaction | `ops.rs` `purge_session_leaves_a_sibling_session_in_the_same_project_intact`, `…refuses_a_session_from_another_project_and_deletes_nothing`, `…refuses_a_session_from_another_workspace`, `…does_not_delete_an_identically_pathed_page_in_another_project`, `…removes_older_summary_versions_without_deleting_prior_manual_page` (#862) | STRONG |
| 10c | Session-time repair is scope-bound (no cross-project timestamp rewrite) | `ai-memory-store/src/ops.rs` `repair_session_times` — every candidate is read back and updated with `WHERE id=? AND workspace_id=? AND project_id=?`; a row outside that 3-tuple is reported `NotFound` and untouched, same code path as an id that does not exist at all; `ai-memory-mcp/src/admin.rs` `handle_repair_session_times` passes the caller's `(workspace, project)` straight through, never the candidate's own | `ops.rs` `repair_session_times_does_not_touch_a_session_of_another_project` (control: sibling session in-scope repaired in the same batch); `admin_repair_session_times.rs` `cross_project_session_is_not_found_and_untouched`; both bite-checked by dropping the `project_id` filter | STRONG |
| 11a | Hook backpressure (202/429) + bounded fan-out (invariant #5) | `ai-memory-hooks/src/router.rs` semaphore→429, 202 immediately, `MAX_HOOK_BATCH_ITEMS`, bounded LRU limiter | `router.rs` `handle_hook_returns_429_when_ingest_saturated`, `ingest_rate_limiter_is_bounded` | STRONG |
| 11b | Capture exclusions drop before storage | `ai-memory-hooks` `capture_policy.rs` `inspect`→`Drop` (before semaphore/spawn) | `capture_policy.rs` per-agent `…honors_exclusions` tests | STRONG |
| 11c | Capture hook ≤200ms budget (invariant #5) | `hooks/_lib.sh` capture path `curl --max-time 0.2` (context-fetch 1.0s and background drain 2.0s are separate, larger-budget paths) | none (shell-script timeout; hard to unit-test) — watch on any capture-path change | WATCH |
| 12 | Network/auth posture | `config.rs` loopback `DEFAULT_BIND`; `serve.rs` `validate_http_exposure`, `require_allowed_host`; `auth.rs` `require_bearer` | `serve.rs` host-guard (missing→400 / forged→403), non-loopback-requires-token; `auth.rs` wrong-token 401 | STRONG |
| 13 | Managed-run transcript attribution (concurrent launches in one checkout, invariant #16) | `ai-memory-store/src/workstream.rs` `link_native_session` stamps `native_session_linked_at` on its own run only, both `finish` updates drop the stamp when the session changes, `run_status` reports it; `ai-memory-cli/src/commands/run.rs` `resolve_native_session_after_run` takes a linked session only when `ai-memory-workstream` `native_session_in_checkout` holds it for this checkout (OpenCode by recorded directory), and never falls back to a link it set aside | `multi_session.rs` `a_session_linked_by_one_managed_run_is_not_another_runs`; `run.rs` `a_session_linked_during_the_run_wins_over_discovery` (concurrent newer session, another checkout's link refused, no fallback to it, unlinked control); `transcript.rs` `native_session_in_checkout_checks_the_opencode_directory`; `store/src/lib.rs` `managed_run_status_reports_a_link_made_during_the_run` | PARTIAL: a run whose child links nothing still falls back to discovering the newest session in the checkout |
| 14 | Client-supplied event time is bounded and self-scoped (#919) | `ai-memory-hooks/src/payload.rs` `HookEnvelope::occurred_at_micros` — a `/hook` caller's optional `occurred_at` must be `> 0` and `<= now + 5min`; it only sets the caller's *own* admitted session's `started_at`/`ended_at`/`created_at`, never another session/project/user, and the ingest dedup `seen_at`/TTL stays on `now` | `payload.rs` `occurred_at_micros_rejects_a_far_future_timestamp`, `…_rejects_non_positive_values`, `…_rejects_garbage_strings` | MEDIUM — self-scoped numeric bound; no cross-tenant surface (a client already controls its own content) |
| — | Per-project authorization (#708) | proposal only — `docs/design-per-project-authz.md` (`authorize_project` choke point + unscoped-read/raw-id bypass classes) | none yet — the design's "Verification plan" tests (authz matrix, unscoped-read-leak, raw-id-authz, ship-inert) land WITH the code | FUTURE |

## Keeping this current (the standing protocol)

1. **Touch a guard, add/extend its adversarial test.** Any change to code in the
   "Enforcing code" column — or that adds a new read/write/admin/hook entry
   point past one of these guards — must add or extend an adversarial test that
   would fail if the guard were removed, and update this file's row.
2. **New boundary → new row + tests before merge.** Adding an isolation
   dimension (a new tenancy axis, a new capability, a new cross-scope surface)
   means a new row here and its adversarial tests in the same change.
3. **A raw-id or unscoped entry point is guilty until tested.** Any handler that
   takes a bare `session_id`/`run_id`/`page_id`/message id, or fans out across
   projects (`global=true`, global `recent`, search), bypasses scope resolution
   by construction — it must resolve→authorize (or filter-before-`LIMIT`) and
   carry an adversarial test proving a foreign id/scope is refused.
4. **Prove the test bites.** An adversarial test that still passes when the guard
   is deleted is not a guard test. Confirm fail-without-guard / pass-with-guard.
5. Unit tests exercise one session/tenant at a time and cannot see these
   defects — the guards live at integration level (`multi_session.rs`,
   `handoff_ownership.rs`, `agent_messages.rs`, `active_project` pointer tests,
   the MCP permission suites). Put boundary tests there.
