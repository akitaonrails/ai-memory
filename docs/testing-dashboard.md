# Dashboard regression tests

The dashboard is built and distributed with Cargo alone. Browser testing uses
Node 22+ and Chromium separately. It adds no JavaScript dependency to the Rust
server, its assets, the Docker image or the native packages.

## Run the checks

Run the repository's normal gates from `AGENTS.md`, including the full Rust
suite. The browser suite needs a built executable and an installed Chromium:

```bash
cargo build --locked --bin ai-memory
# Pinned test browser; installation follows Playwright's browser installer.
npx --yes playwright@1.63.0 install --with-deps --only-shell chromium
node tests/e2e/web_dashboard_browser.mjs
node tests/e2e/web_dashboard_mutations.mjs
# Focused edit flow, including its three browser faults.
node tests/e2e/web_dashboard_mutations.mjs --case=message-editing
python3 scripts/check-dashboard-core-mutations.py
# Focused mailbox isolation probe (the CI gate runs every case).
python3 scripts/check-dashboard-core-mutations.py --case expose-restricted-mailbox-projects
```

Use `AI_MEMORY_BIN` for another built executable and `AI_MEMORY_BROWSER` for an
existing Chromium executable. On Linux, the runner also finds Chromium in the
Playwright cache. An absent executable, a missing browser, a failed setup or a
JavaScript exception fails the command. The Linux CI job installs the pinned
browser and runs both mutation gates, without `continue-on-error` or retries.
The [browser installation reference](https://playwright.dev/docs/browsers)
describes the test browser's system dependencies and headless shell.

The browser runner starts its own Rust server, uses synthetic projects and a
fake root token, and drives Chromium through its native debugging protocol. It
does not modify the DOM or contact a running production server. Messages are sent
over HTTP and claimed through the native CLI. Every run keeps logs and a JSON
report in the printed artifact directory, and failed browser cases also keep a
screenshot.

Run the Rust mutation command alone, with no other build running and no editor
writing the selected source files. It temporarily changes clean tracked source
files, stores the exact originals and restores them in `finally`. A concurrent
edit is refused instead of overwritten. Each probe needs a passing baseline and
a specific failed test assertion, so compiler errors and timeouts do not count.
Logs and originals stay under `target/dashboard-core-mutations-*`.

## Behaviors protected

Six deliberate faults were confirmed to fail their intended assertions: source
scope, source ownership, contradiction visibility, admin-setting disclosure,
exact model filtering and the UTC lower bound. Compiler errors do not count.
Reproduce them with the matching cases in
`scripts/check-dashboard-core-mutations.py`.

The contextual investigation additions are covered by the browser cases
`contextual-investigation`, `feature-states` and `knowledge-evidence`, which run
against the real Rust server. They check combined server filters beyond fifty
responses, reload, empty results, native association labels, effective zero-LLM
states, source navigation, on-page recall and mobile overflow. Rust regressions
in `contextual.rs`, `live_token_usage.rs`, `session_usage.rs` and the hooks
router also cover foreign source IDs, private contradictions, source ownership,
root controls, redacted admin settings, transaction rollback, native cursor
continuity, malformed gaps and sanitization before truncation. Tests require historical data with missing context to remain unknown, without
guessed coordinates.

| Behavior | Automated regression guard |
| --- | --- |
| Only the selected tab is visible/accessibility-exposed; keyboard, history, reload and old section links work | `web_dashboard_browser.mjs`: `tabs`; Rust `operations_project_tabs_have_one_default_panel_and_matching_controls` |
| Metric cards lead to the project ordering/filter or scoped detail view that explains the count | Rust `dashboard_metric_links_sort_filter_and_reject_unknown_views`, project-page smoke assertions, and `operations_agent_totals_and_session_drilldown_do_not_multiply_tokens` |
| Recipient filtering includes workspace and project, clears stale selection, disables empty projects and recovers | Browser `recipients`; Rust `operations_inbox_filters_initial_agents_by_both_project_coordinates` |
| Inbox queue counts name each workspace/project, links and selectors open the matching mailbox, empty states do not claim new mail, reading leaves messages queued, collection reduces counts | Browser `mailbox-context`; Rust `operations_inbox_queue_context_hides_foreign_projects_and_preserves_mail` also refuses foreign/partial scopes and hides restricted project names with legitimate read/root controls |
| Sidebar notifications appear for actual queued mail, refresh without reload, link to the right workspace/project, disappear after collection and clear on authorization loss | Browser `notifications`; Rust `operations_notifications_are_read_only_and_hide_restricted_projects` covers access-filtered totals, bounded links, zero state, no caching and non-consumption |
| Pending notification links and the default received mailbox find older messages behind collected history; explicit all/claimed filters retain history | Rust `operations_pending_mail_is_filtered_before_history_pagination`; server state predicate runs before LIMIT/OFFSET |
| Resumed substantive work reopens scoped API/detail reads, leaves completed listings, refuses a foreign project and closes after another SessionEnd | Browser `resumed-session`; store `session_activity` tests preserve the persisted watermark and owner/project controls |
| Legacy browser reads cannot send; invalid tokens are refused and cleared; authorized sends reach the chosen harness once | Browser `delivery`, against real HTTP auth and native CLI claims; Rust `operations_message_write_obeys_root_user_and_project_grants`, `operations_message_requires_write_access_to_both_project_coordinates`, store `addressed_messages_are_claimed_once_only_by_the_selected_harness` |
| Editing holds mail outside the native claim queue; save preserves the edited text and id, discard preserves the original, held edits survive navigation/reload, and stale edits refuse collected mail | Browser `message-editing`; its `skip-message-edit-hold`, `lose-saved-message-text` and `save-discarded-message-text` faults must fail the same case |
| Every edit/save/release requires write access to both mailbox and sender, including after a grant changes; foreign workspaces/projects and invalid lifecycle states cannot be edited | Rust `operations_edit_needs_write_access_to_the_sender_project_too`, `a_message_being_edited_leaves_the_queue_until_it_is_saved`, `collected_or_cancelled_mail_cannot_be_taken_for_editing`; seven core mutation probes remove these guards |
| Sanitization, admission rejection, foreign origin and unknown recipients | Rust `operations_browser_messages_use_the_configured_sanitizer_and_admission`, `operations_browser_send_refuses_cross_origin_unknown_recipient_and_empty_body`; browser `delivery` checks escaped message text |
| Usage reaches the server before session-end, repeated reports do not add calls, refresh preserves active tab/filter/focus | Browser `live-usage`; hooks `live_token_calls_update_open_sessions_without_an_end_event` |
| More than 256 responses and a transcript larger than 4 MiB recover across successive hook processes, without advancing on enqueue failure or duplicating responses | Browser `capture-backlog` uses native hooks, spool drain, the real server and dashboard; CLI `native_hook_recovers_a_response_backlog_and_does_not_advance_on_spool_failure`, `live_token_cursor_replays_until_commit_then_recovers_every_response`, `live_token_cursor_crosses_byte_windows_and_retries_partial_usage` |
| Search and recent ordering include small responses beyond the largest fifty; pagination and reload keep query state | Browser `call-history`; store `token_call_queries_search_and_order_before_pagination_with_scope_and_owner_guards`; web `operations_response_search_and_pagination_cover_the_full_project_and_session` |
| Session usage updates without reload and preserves filter focus | Browser `session-live-usage` |
| The native hook actually reads Claude/Codex transcripts for each supported event | CLI `native_hook_reports_live_usage_for_every_supported_event_and_harness` uses the built executable and inspects the delivered spool payload |
| Streaming/replayed counters cannot decrease the displayed response or disagree with session totals | Store `token_calls_keep_partial_reports_monotonic_in_rows_and_aggregates` |
| Reused response ids stay separate across sessions, agents and projects | Store `token_calls_group_by_session_and_agent_without_merging_foreign_responses`; web ownership/aggregate tests |
| Codex cached input is excluded from fresh input | CLI `live_token_codex_reads_modern_usage_without_counting_cached_input_twice`; migration `v90_normalizes_codex_cache_once_and_preserves_claude` |
| Private/malformed response ids are refused; private model labels are scrubbed before the 64-character bound | Hooks `live_token_calls_scrub_private_labels_before_truncating` checks a private value crossing the limit, with a public-label control |
| Light/dark selection persists; system mode follows OS changes; palettes differ | Browser `themes` |
| Navigation remains visible with usable targets and no page overflow at 320, 390 and 768 px | Browser `responsive` |
| Bundled guide HTML/Markdown/JSON, search, release selection, mount prefixes, traversal refusal | `tests/suite/documentation.rs`; browser `documentation` uses the real authenticated server |
| Changing a release loads its notes without a button, keeps links/history/reload consistent and works with manual submission without JavaScript | Browser `documentation` |

## Prove the guards detect faults

The browser campaign first runs every case against the original application. It
then changes one served HTML response at a time through Chromium's network
interception, while the tested controller still runs in Chromium. A mutant
counts only if the response was changed and the intended behavioral case failed.

| Deliberate fault | Required failing case |
| --- | --- |
| Reset the response cursor on every hook | Cursor backlog regression |
| Advance the cursor before enqueue succeeds | Native-hook backlog/enqueue-failure regression |
| Ignore response search before selecting the page | Full-history response query regression |
| Remove response-owner filtering | Full-history query ownership regression with root control |
| Disable periodic refresh on the session page | `session-live-usage` |
| Drop server query fields from response controls | `call-history` |
| Display every tab panel | `tabs` |
| Ignore recipient workspace | `recipients` |
| Ignore recipient project | `recipients` |
| Retain the write credential | `delivery` |
| Enable duplicate sending while a request is pending | `delivery` |
| Enable sending to an empty project after a request completes | `delivery` |
| Disable automatic usage refresh | `live-usage` |
| Stop saving the selected theme | `themes` |
| Capture browser navigation shortcuts in the tab controller | `tabs` |
| Disable automatic release submission | `documentation` |
| Restore a stale release selection on browser history navigation | `documentation` |
| Show a fabricated new-message alert with no queued mail | `mailbox-context` |
| Ignore the workspace when selecting a mailbox | `mailbox-context` |
| Expose restricted projects in the mailbox overview | Mailbox context isolation regression |
| Stop loading sidebar notifications | `notifications` |
| Ignore a notification's workspace | `notifications` |
| Expose restricted notification totals or project names | Notification isolation regression |
| Apply history pagination before its state filter | Pending-mail pagination regression |
| Treat resumed substantive session work as ended | Store `resumed_summary_is_open_and_preserves_the_persisted_end` |
| Capture usage only on session-end | Native-hook event/harness integration test |
| Count Codex cache as fresh input | Codex transcript parser regression |
| Overwrite a response with lower replayed counters | Response/session consistency regression |
| Remove harness checks from the claim lookup and atomic update | Directed harness claim regression |
| Open the editor without holding the message on the server | `message-editing` |
| Send different text on save or overwrite the original on discard | `message-editing` |
| Accept read-only access to the mailbox or sender during editing | Edit authorization regression, including grant changes on held mail |
| Remove mailbox coordinates from hold or save/release | Foreign project/workspace edit regression |
| Hold collected/cancelled mail or save/release unheld mail | Edit lifecycle regressions |
| Let claim selection, metadata and atomic update accept held mail | Held-mail claim regression |

Browser results are summarized in `target/web-dashboard-mutations.json`. The
source anchors used to inject a fault need updating when the implementation is
reorganized, and a fault that was not injected fails the campaign. Assertions
stay on the behavior above, not on a particular function body.

The probes cover the deliberate faults listed above. Rust tests also run the
existing authorization, isolation, concurrency and recovery suites. Browser
coverage is limited to Chromium on Linux. This gate does not check Firefox,
Safari or pixel-perfect appearance. Behavior-preserving refactors should pass
the tests.
