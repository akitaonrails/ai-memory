## Human-facing command output. Keep flags, paths and identifiers verbatim.

cli-audit-contamination-no-structural-contamination-found-no-session-mis-bucketed-by-cwd-no-project = No structural contamination found (no session mis-bucketed by cwd, no project name collisions).
cli-audit-contamination-contamination-audit-session-s-mis-bucketed-project-s-with-a-colliding-name = Contamination audit: { $value0 } session(s) mis-bucketed, { $value1 } project(s) with a colliding name.
cli-audit-contamination-session-landed-in-but-its-cwd-resolves-to-project = { "  " }[{ $value0 }] session { $value1 } landed in { $value2 }/{ $value3 } but its cwd ({ $value4 }) resolves to project { $value5 }
cli-audit-contamination-project-page-s-session-s-last-activity-collides-by-with-add-a-ai-memory-toml = { "  " }[{ $value0 }] project { $value1 } ({ $value2 }/{ $value3 }): { $value4 } page(s), { $value5 } session(s), last activity { $value6 }; collides by { $value7 } with { $value8 }. Add a `.ai-memory.toml` marker or run `merge-project`.
cli-audit-contamination-in = { "  " }[{ $value0 }] { $value1 } { $value2 } in { $value3 }/{ $value4 }
cli-auth-github-token-client-id-issuer-do-not-apply-to-auth-login-openai-oauth = --github-token / --client-id / --issuer do not apply to `auth login openai-oauth`
cli-auth-issuer-applies-only-to-auth-login-oidc-device = --issuer applies only to `auth login oidc-device`
cli-auth-github-token-applies-only-to-auth-login-copilot = --github-token applies only to `auth login copilot`
cli-auth-open-this-url-complete = Open this URL: { $value0 }
cli-auth-enter-code = Enter code: { $value0 }
cli-auth-waiting-for-authorization = Waiting for authorization...
cli-auth-oidc-device-logged-in = oidc-device: logged in
cli-auth-issuer-issuer = issuer: { $value0 }
cli-auth-token-file = token file: { $value0 }
cli-auth-oidc-device-logged-out = oidc-device: logged out
cli-auth-openai-oauth-token-response-did-not-include-refresh-token = openai-oauth token response did not include refresh_token
cli-auth-openai-oauth-logged-in = openai-oauth: logged in
cli-auth-account-account-id = account: { $value0 }
cli-auth-openai-oauth-logged-out = openai-oauth: logged out
cli-auth-copilot-logged-in = copilot: logged in
cli-auth-copilot-logged-out = copilot: logged out
cli-auth-expires-in = expires in: { $value0 }
cli-auth-openai-oauth-not-logged-in = openai-oauth: not logged in
cli-auth-copilot-cached-token-valid-no-github-token-stored-for-refresh = copilot: cached token valid (no GitHub token stored for refresh)
cli-auth-copilot-not-logged-in-cached-token-expired-and-no-refreshable-github-token-s = copilot: not logged in (cached token expired and no refreshable GitHub token stored)
cli-auth-github-token-expires-in = github token expires in: { $value0 }
cli-auth-cached-copilot-token-expires-in = cached copilot token expires in: { $value0 }
cli-auth-api-base-api-base-url = api base: { $value0 }
cli-auth-copilot-not-logged-in = copilot: not logged in
cli-auth-oidc-device-not-logged-in = oidc-device: not logged in
cli-auto-improve-report-machine-readable = { "\u000A" }--- machine-readable ---
cli-backfill-server-profile-name-was-refused = server profile `{ $value0 }` was refused ({ $value1 })
cli-backfill-ai-memory-backfill-of-session-failed-error = ai-memory: backfill of { $value0 } session { $value1 } failed: { $value2 }
cli-backfill-backfill-failed-to-import-of-selected-session-s = backfill failed to import { $value0 } of { $value1 } selected session(s)
cli-backup-wrote-backup-to = ✓ wrote backup to { $value0 } ({ $value1 })
cli-bootstrap-note-no-git-found-at-bootstrapping-from-readme-docs-rules-only-git-commit-hi = note: no .git found at { $value0 }; bootstrapping from README/docs/rules only (git-commit history skipped). Pass --repo-path or `git init` to include commits.
cli-bootstrap-machine-readable-report = { "\u000A" }--- machine-readable ---{ "\u000A" }{ $value0 }
cli-checkpoints-no-wiki-checkpoints-found = no wiki checkpoints found
cli-commit-committed = committed: { $value0 }
cli-commit-nothing-to-commit = nothing to commit{ $value0 }
cli-compact-compact-rewrites-the-whole-database-under-an-exclusive-lock-every-write-bloc = compact rewrites the whole database under an exclusive lock: every write blocks until it finishes, and it needs free disk space of roughly the database's own size.{ "\u000A" }{ "\u000A" }Check `ai-memory status` for the reclaimable figure first, then:{ "\u000A" }{ "\u000A" }  ai-memory compact --confirm
cli-compact-compacted-reclaimed = Compacted: { $value0 } → { $value1 } ({ $value2 } reclaimed).
cli-compact-nothing-to-reclaim-the-database-had-no-meaningful-free-page-backlog-this-is = Nothing to reclaim — the database had no meaningful free-page backlog. This is the normal result for a store that has not just had a large deletion.
cli-continue-session-no-managed-checkout-is-linked-for-this-server-yet-launch-one-with-ai-memory = no managed checkout is linked for this server yet; launch one with `ai-memory show` or `ai-memory run <harness>` first
cli-continue-session-skipping-client-registry-has-an-invalid-linked-at-timestamp = skipping { $value0 }: client registry has an invalid linked_at timestamp
cli-continue-session-skipping-label = skipping { $value0 }: { $value1 }
cli-continue-session-continuing-in = continuing { $value0 } in { $value1 }
cli-continue-session-no-linked-checkout-is-still-usable-skipped-pick-one-explicitly-with-ai-memor = no linked checkout is still usable ({ $value0 } skipped); pick one explicitly with `ai-memory show`
cli-continue-session-checkout-now-resolves-to = checkout now resolves to { $value0 }/{ $value1 }
cli-curator-choose-either-dry-run-or-stage-not-both = choose either --dry-run or --stage, not both
cli-delete-page-path-was-passed-empty-specify-the-wiki-page-path-to-delete = --path was passed empty; specify the wiki page path to delete
cli-delete-page-under = { $value0 } { $value1 } under { $value2 }/{ $value3 }
cli-doctor-resolving-the-current-working-directory = resolving the current working directory
cli-doctor-invalid-home-route-map-doctor-did-not-query-a-fallback-server-scope = invalid home route map; doctor did not query a fallback server scope
cli-embed-dry-run-would-embed-would-page-s-skipped-already-up-to-date = dry-run: would embed { $value0 } page(s), { $value1 } already up-to-date
cli-export-okf-exported-okf-bundle-to-size-bytes = exported OKF bundle to { $value0 } ({ $value1 } bytes)
cli-finalize-session-ai-memory-no-home-directory-session-is-finalized-without-its-prompts = ai-memory: no home directory; session { $value0 } is finalized without its prompts
cli-finalize-session-ai-memory-could-not-replay-the-prompts-of-session-error-finalizing-without-t = ai-memory: could not replay the prompts of session { $value0 } ({ $value1 }); finalizing without them
cli-handoffs-no-open-handoffs-for-workspace-project = No open handoffs for { $value0 }/{ $value1 }.
cli-init-generated-a-root-bearer-token-in-owner-only-the-server-requires-it-on-every = Generated a root bearer token in { $value0 } (owner-only). The server requires it on every route but /healthz; hooks, the CLI and `ai-memory mcp-bridge` read it from there.
cli-init-generating-auth-token-pepper-e = generating auth token_pepper: { $value0 }
cli-llm-test-model = --- model: { $value0 } ---
cli-llm-test-usage-in-out = --- usage: in={ $value0 } out={ $value1 } ---
cli-llm-test-server-provider-provider-model-latency-ms = --- server provider: { $value0 } / { $value1 } ({ $value2 } ms) ---
cli-mcp-bridge-stored-bridge-credential-is-blank-repair-or-remove-it-instead-of-falling-bac = stored bridge credential { $value0 } is blank; repair or remove it instead of falling back to root
cli-merge-project-exactly-one-of-from-or-from-project-id-is-required = exactly one of --from or --from-project-id is required
cli-merge-project-exactly-one-of-into-or-into-project-id-is-required = exactly one of --into or --into-project-id is required
cli-merge-project-verb-page-versions-sessions-observations-handoffs-workstreams-page-conflicts = { $value0 } { $value1 } -> { $value2 }: { $value3 } page versions, { $value4 } sessions, { $value5 } observations, { $value6 } handoffs, { $value7 } workstreams, { $value8 } page conflicts.
cli-merge-project-re-run-the-same-command-with-confirm-to-apply-this-merge = Re-run the same command with --confirm to apply this merge.
cli-merge-project-warning-merge-committed-but-scope-manifest-refresh-failed-warning = warning: merge committed but scope manifest refresh failed: { $value0 }
cli-message-reading-message-body-from-stdin-e = reading message body from stdin: { $value0 }
cli-message-sent-message-from-from-workspace-from-project-to = Sent message { $value0 } from { $value1 }/{ $value2 } to { $value3 }/{ $value4 }.
cli-message-no-pending-mailbox-messages-for-workspace-project = No pending { $value0 } messages for { $value1 }/{ $value2 }.
cli-message-pending-mailbox-messages-for-workspace-project-oldest-first = Pending { $value0 } messages for { $value1 }/{ $value2 } (oldest first):
cli-message-id = { "    " }id: { $value0 }
cli-message-no-pending-messages-in-workspace-project-s-inbox = No pending messages in { $value0 }/{ $value1 }'s inbox.
cli-message-message-cancel-requires-exactly-one-of-id-message-id-or-all = message cancel requires exactly one of --id <MESSAGE_ID> or --all
cli-message-cancelled-message-in-workspace-project = Cancelled { $value0 } { $value1 ->
        [one] message
       *[other] messages
    } in { $value2 }/{ $value3 }.
cli-mod-ai-memory-scope-workspace-project-from = ai-memory: scope { $value0 }/{ $value1 } ({ $value2 } from { $value3 })
cli-mod-project-candidate-does-not-exist = project candidate does not exist: { $value0 }
cli-mod-could-not-derive-project-name-from = could not derive project name from { $value0 }
cli-mod-ai-memory-workspace-declared-workspace-from = ai-memory: workspace { $value0 } (workspace from { $value1 })
cli-mod-the-ai-memory-wrapper-at-local-bin-ai-memory-looks-stale-it-didn-t-pass-ai-m = the `ai-memory` wrapper at ~/.local/bin/ai-memory looks stale (it didn't pass AI_MEMORY_HOST_CWD into the container). Without this, every project would land in `default/work` regardless of which host dir you ran from. Reinstall the checksum-verified wrapper from the latest GitHub Release as documented in README.md,{ "\u000A" }  (or run `ai-memory upgrade` if your existing wrapper is recent enough to know that command)
cli-mod-could-not-derive-project-name-from-cwd-pass-project-explicitly = could not derive project name from CWD ({ $value0 }); pass --project explicitly
cli-move-project-one-of-project-or-project-id-is-required = one of --project or --project-id is required
cli-move-project-move-project-moves-to-workspace-if-the-destination-has-no-same-named-project = move-project moves { $value0 }/{ $value1 } to workspace { $value2 }. If the destination has no same-named project it is a lossless true-move (re-stamp in place — sessions, observations and history preserved). If it already has one, the pages are copied in and merged, then the source is PURGED. Both are irreversible.{ "\u000A" }Re-run with --confirm to proceed:{ "\u000A" }{ "\u000A" }  { $value3 }
cli-move-project-ai-memory-warning-warning = ai-memory: warning: { $value0 }
cli-move-project-ai-memory-project-moved-but-the-client-local-checkout-link-could-not-be-upda = ai-memory: project moved, but the client-local checkout link could not be updated ({ $value0 })
cli-move-project-moved-pages-pages-re-stamped-true-move-sessions-observations-and-history-pre = Moved { $value0 }/{ $value1 } → { $value2 }/{ $value3 }: { $value4 } pages re-stamped (true move — sessions, observations and history preserved).
cli-move-project-moved-pages-pages-copied-merged-into-existing-project-tail = Moved { $value0 }/{ $value1 } → { $value2 }/{ $value3 }: { $value4 } pages copied (merged into existing project){ $value5 }.
cli-move-project-warning-skipped-count-page-s-could-not-be-read-from-the-source-and-were-skip = Warning: { $value0 } page(s) could not be read from the source and were skipped; the source was NOT purged. Fix and re-run.
cli-move-project-path-conflict-s-source-page-kept-under-a-de-duplicated-path-both-versions-pr = { $value0 } path conflict(s) — source page kept under a de-duplicated path (both versions preserved):
cli-move-session-dry-run-nothing-was-written-re-run-with-confirm-to-apply = { "\u000A" }(dry run; nothing was written){ "\u000A" }Re-run with --confirm to apply:{ "\u000A" }{ "\u000A" }  { $value0 }
cli-pending-writes-no-pending-writes = (no pending writes)
cli-pending-writes-body = { "\u000A" }{ $value0 }{ "\u000A" }{ "\u000A" }--- body ---{ "\u000A" }{ $value1 }
cli-pending-writes-approved = ✓ approved { $value0 }
cli-pending-writes-rejected = ✓ rejected { $value0 }
cli-profile-sets-profile-consume-false-so-this-project-does-not-take-the-profile = { $value0 } sets `[profile] consume = false`, so this project does not take the profile
cli-profile-workspace-project-sets-profile-consume-false-so-it-does-not-take-the-profile = { $value0 }/{ $value1 } sets `[profile] consume = false`, so it does not take the profile
cli-profile-no-profile-entries-apply-to-workspace-project-nothing-written = No profile entries apply to { $value0 }/{ $value1 }; nothing written.
cli-profile-would-write-to-block = Would write to { $value0 }:{ "\u000A" }{ "\u000A" }{ $value1 }
cli-profile-entr = { $value0 }: { $value1 } ({ $value2 } { $value3 ->
        [one] entry
       *[other] entries
    }{ $value4 })
cli-project-init-ai-memory-project-init-needs-an-interactive-terminal = `ai-memory project init` needs an interactive terminal
cli-project-registry-ai-memory-could-not-read-the-checkout-binding-error-using-guarded-project-na = ai-memory: could not read the checkout binding ({ $value0 }); using guarded project-name resolution
cli-project-registry-checkout-identity-changed-while-selecting-the-project-refusing-to-remember-a = checkout identity changed while selecting the project; refusing to remember a stale selection
cli-project-registry-checkout-is-not-a-directory = checkout is not a directory: { $value0 }
cli-project-registry-client-project-registry-exceeds-max-links-links = client project registry exceeds { $value0 } links
cli-project-registry-client-project-registry-exceeds-the-byte-limit-keeping-the-previous-registry = client project registry exceeds the byte limit; keeping the previous registry
cli-purge-project-purge-project-is-destructive-and-irreversible-re-run-with-confirm-to-proceed = purge-project is destructive and irreversible.{ "\u000A" }Re-run with --confirm to proceed:{ "\u000A" }{ "\u000A" }  { $value0 }
cli-purge-project-the-following-workstream-segment-directories-are-now-orphaned-under-data-dir = The following workstream segment directories are now orphaned under <data_dir>/raw/workstreams/ and can be removed:
cli-purge-project-warning-wiki-file-s-could-not-be-removed-from-disk-db-rows-are-gone = Warning: { $value0 } wiki file(s) could not be removed from disk (DB rows are gone).
cli-purge-project-database-compacted-freed-bytes-reclaimed = Database compacted: freed bytes reclaimed.
cli-purge-project-note-this-was-a-logical-delete-the-project-is-unreachable-through-the-api-an = Note: this was a logical delete. The project is unreachable through the API and search, but its bytes remain in the database file until it is rewritten. Re-run with --compact to reclaim them (slow), and see docs/lifecycle-ops.md for what that does and does not guarantee.
cli-purge-session-session-id-session-id-is-not-a-uuid-pass-the-full-sessions-id-value-a-prefix = `--session-id { $value0 }` is not a UUID. Pass the full `sessions.id` value; a prefix or a workstream id will not match.
cli-purge-session-purge-session-is-destructive-and-irreversible-re-run-with-confirm-to-proceed = purge-session is destructive and irreversible.{ "\u000A" }Re-run with --confirm to proceed:{ "\u000A" }{ "\u000A" }  ai-memory purge-session --workspace { $value0 } --project { $value1 } --session-id { $value2 } --confirm
cli-purge-session-wiki-pages-removed = Wiki pages removed:
cli-purge-session-warning-wiki-page-file-s-could-not-be-removed-from-disk-db-rows-are-gone = Warning: { $value0 } wiki page file(s) could not be removed from disk (DB rows are gone).
cli-purge-session-note-this-was-a-logical-delete-the-session-is-unreachable-through-the-api-an = Note: this was a logical delete. The session is unreachable through the API and search, but its bytes remain in the database file until it is rewritten. Re-run with --compact to reclaim them (slow), and see `docs/` for what that does and does not guarantee.
cli-read-page-path-was-passed-empty-specify-the-wiki-page-path-to-read = --path was passed empty; specify the wiki page path to read
cli-read-page-search-query-was-passed-empty-specify-a-query-or-path-wiki-path = search query was passed empty; specify a query or --path <wiki-path>
cli-read-page-provide-a-query-positional-or-path-wiki-path = provide a query (positional) or --path <wiki-path>
cli-reclaim-ledger-versions-no-superseded-ledger-versions-to-reclaim-nothing-written-by-the-pre-2-1-1-in = No superseded ledger versions to reclaim. Nothing written by the pre-2.1.1 indexer is left in this store.
cli-reclaim-ledger-versions-would-delete-rows-superseded-version-s-of-the-hook-event-ledger-across-paths = Would delete { $value0 } superseded version(s) of the hook event ledger across { $value1 } ledger path(s), carrying { $value2 } of page body.
cli-reclaim-ledger-versions-drop-latest-each-ledger-s-live-row-would-go-too = { "  " }--drop-latest: each ledger's live row would go too.
cli-reclaim-ledger-versions-nothing-was-changed-to-apply = Nothing was changed. To apply:
cli-reclaim-ledger-versions-deleted-rows-superseded-ledger-version-s-across-paths-ledger-path-s-carrying = Deleted { $value0 } superseded ledger version(s) across { $value1 } ledger path(s), carrying { $value2 } of page body.
cli-reclaim-ledger-versions-each-ledger-s-live-row-was-dropped-as-well-drop-latest = { "  " }Each ledger's live row was dropped as well (--drop-latest).
cli-reclaim-ledger-versions-reclaimed = Reclaimed { $value0 } ({ $value1 } → { $value2 }).
cli-reclaim-ledger-versions-the-bytes-are-free-pages-now-run-ai-memory-compact-confirm-to-return-them-to = The bytes are free pages now. Run `ai-memory compact --confirm` to return them to the filesystem.
cli-recovery-ai-memory-recovery-count-hook-event-s-were-dropped-recovery-remains-incomple = ai-memory: recovery: { $value0 } hook event(s) were dropped; recovery remains incomplete unless an exact correlated transcript replaces them
cli-reindex-refusing-to-reindex-into-a-non-empty-sqlite-store-counts-ai-memory-reindex-r = refusing to reindex into a non-empty SQLite store ({ $value0 }). `ai-memory reindex` rebuilds a clean derived DB from wiki files; it is not an in-place dirty-index repair. Stop the server, take a backup, move or remove { $value1 }, then re-run reindex. DB-only state (sessions, observations, handoffs, users, audit rows, embeddings, decay counters) is not reconstructed from markdown.
cli-reindex-reindexed-pages-across-project-s-in-workspace-s-from = reindexed { $value0 } pages across { $value1 } project(s) in { $value2 } workspace(s) from { $value3 }
cli-reindex-skipped-page-s-whose-path-differs-from-an-indexed-page-only-by-case-or-unico = skipped { $value0 } page(s) whose path differs from an indexed page only by case or Unicode normalization (they cannot coexist on macOS/Windows); the log names each pair
cli-rename-project-ai-memory-project-renamed-but-the-client-local-checkout-link-could-not-be-up = ai-memory: project renamed, but the client-local checkout link could not be updated ({ $value0 })
cli-rename-project-renamed-pages-now-under-the-new-name = Renamed { $value0 }/{ $value1 } → { $value2 }/{ $value3 } ({ $value4 } pages now under the new name).
cli-rename-workstream-to-was-passed-empty-specify-the-new-workstream-name = --to was passed empty; specify the new workstream name
cli-rename-workstream-pass-from-name-or-workstream-id-id-to-choose-the-workstream-to-rename = pass --from NAME or --workstream-id ID to choose the workstream to rename
cli-reorg-all-sessions-are-already-in-the-correct-per-cwd-project-nothing-to-do = All sessions are already in the correct per-cwd project; nothing to do.
cli-reorg-session-s-would-be-moved = { $value0 } session(s) would be moved:{ "\u000A" }
cli-reorg-table-header = { $value0 } { $value1 } cwd
cli-reorg-dry-run-omit-dry-run-to-apply = { "\u000A" }(dry-run; omit --dry-run to apply)
cli-reorg-reorg-complete = Reorg complete:
cli-reorg-sessions-moved = { "  " }sessions moved:        { $value0 }
cli-reorg-observations-updated = { "  " }observations updated:  { $value0 }
cli-reorg-pages-graveyarded = { "  " }pages graveyarded:     { $value0 }
cli-reorg-distinct-new-projects = { "  " }distinct new projects: { $value0 }
cli-repair-backfill-timestamps-ai-memory-repair-backfill-timestamps-for-workspace-project-no-local-transcri = ai-memory: repair-backfill-timestamps for { $value0 }/{ $value1 }: no local transcript carried a readable timestamp ({ $value2 } session(s) scanned locally, none captured); nothing to send.
cli-repair-backfill-timestamps-ai-memory-repair-backfill-timestamps-for-workspace-project-total-repaired-se = ai-memory: repair-backfill-timestamps for { $value0 }/{ $value1 }: { $value2 } session(s) { $value3 }, { $value4 } skipped, { $value5 } local transcript(s) had no readable timestamp.
cli-repair-backfill-timestamps-sent-session-s-across-request-s-of-up-to-max-sessions-per-request-each-one-t = { "  " }sent { $value0 } session(s) across { $value1 } request(s) of up to { $value2 } each (one transaction per request).
cli-repair-backfill-timestamps-skipped-reason-count = { "  " }skipped ({ $value0 }): { $value1 }
cli-repair-backfill-timestamps-dry-run-nothing-was-written-re-run-with-confirm-to-apply-recorded-in-audit-l = { "  " }dry run: nothing was written. Re-run with --confirm to apply (recorded in audit_log on apply, which is the reversibility backing).
cli-reset-would-remove = would remove { $value0 }
cli-reset-dry-run-pass-confirm-to-wipe = (dry-run; pass --confirm to wipe)
cli-restore-source-tarball-not-found = source tarball { $value0 } not found
cli-restore-restored = restored { $value0 } -> { $value1 }
cli-restore-restored-the-backup-s-config-toml = restored the backup's config.toml
cli-restore-kept-the-live-config-toml-the-backup-s-copy-is-at-a-config-can-run-commands = kept the live config.toml; the backup's copy is at { $value0 }. A config can run commands and redirect data: review it before adopting it, or rerun with --with-config to restore it in place.
cli-restore-page-path-was-passed-empty-specify-the-wiki-page-path-to-restore = --path was passed empty; specify the wiki page path to restore
cli-restore-page-restored-under-from-page-id = restored { $value0 } under { $value1 }/{ $value2 } from { $value3 } (page_id={ $value4 })
cli-restore-page-pre-restore-checkpoint-pre = pre-restore checkpoint: { $value0 }
cli-restore-page-restore-checkpoint-cp = restore checkpoint: { $value0 }
cli-resume-ai-memory-resume-needs-a-terminal-use-ai-memory-workstreams-to-list-a-checko = `ai-memory resume` needs a terminal; use `ai-memory workstreams` to list a checkout in scripts
cli-resume-resuming-workstream-with-harness-in-at = resuming workstream '{ $value0 }' with harness '{ $value1 }' in { $value2 } at { $value3 }
cli-resume-current-checkout-belongs-to-not-workspace = current checkout belongs to { $value0 }, not workspace '{ $value1 }'
cli-resume-no-managed-workstreams-for-the-current-checkout-launch-one-here-with-ai-memo = no managed workstreams for the current checkout ({ $value0 }); launch one here with `ai-memory run <harness>` first
cli-resume-skipping-current-checkout = skipping current checkout: { $value0 }
cli-resume-skipping-could-not-list-managed-workstreams = skipping { $value0 }: could not list managed workstreams ({ $value1 })
cli-resume-no-managed-workstreams-are-available-detail-launch-one-with-ai-memory-run-ha = no managed workstreams are available ({ $value0 }); launch one with `ai-memory run <harness>` first
cli-service-host-service-host-is-the-windows-logon-entry-s-launcher-on-this-platform-ai-memor = `service-host` is the Windows logon entry's launcher; on this platform `ai-memory setup` installs a systemd user unit or a LaunchAgent instead
cli-setup-init = { "\u000A" }== init
cli-setup-config = config: { $value0 } { $value1 }
cli-setup-service = { "\u000A" }== service
cli-setup-service-reason = service: { $value0 }
cli-setup-service-2 = service: { $value0 } { $value1 }
cli-setup-self-check = { "\u000A" }== self-check
cli-setup-auth-none-loopback-run-ai-memory-auth-enable-to-require-a-bearer-token = auth: none (loopback); run `ai-memory auth enable` to require a bearer token
cli-setup-auth-bearer-token-owner-only-file-never-printed = auth: bearer token (owner-only file; never printed)
cli-setup-problem-problem = problem: { $value0 }
cli-setup-setup-finished-with-problem-s-fix-them-and-re-run-ai-memory-setup = setup finished with { $value0 } problem(s); fix them and re-run `ai-memory setup`
cli-setup-plugin-is-enabled-leaving-the-settings-file-hooks-mcp-entry-and-skills-alone = plugin: { $value0 } is enabled; leaving the settings-file hooks, MCP entry and skills alone (run `ai-memory install-hooks --agent { $value1 } --apply` to add them anyway)
cli-setup-mcp-keeping-the-existing-session-aware-entry = mcp: keeping the existing session-aware entry
cli-setup-healthz-ok-healthz = healthz: ok ({ $value0 })
cli-setup-memory-status-ok = memory_status: ok
cli-setup-memory-status-reported-an-error = memory_status reported an error
cli-setup-agent-setup-agent-agent-opencode-requires-opencode-dialect-v1-v2-because-a-contain = setup-agent --agent opencode requires --opencode-dialect v1|v2 because a container cannot inspect the host executable
cli-setup-agent-opencode-dialect-cannot-be-combined-with-the-force-v2-opencode2-compatibilit = --opencode-dialect cannot be combined with the force-V2 opencode2 compatibility alias
cli-setup-agent-opencode-dialect-is-valid-only-with-agent-opencode-not = --opencode-dialect is valid only with --agent opencode, not { $value0 }
cli-setup-agent-cline-discovers-generated-hook-files-in-the-host-user-s-hook-directory-run-a = Cline discovers generated hook files in the host user's hook directory; run `ai-memory install-hooks --agent cline --apply` on that host
cli-setup-agent-internal-generated-integration-should-have-returned-before-staging-hooks = internal: generated integration should have returned before staging hooks
cli-setup-agent-ai-memory-setup-agent-emits-shell-powershell-hook-bundles-for-agent-sub-thes = [ai-memory] setup-agent emits shell/PowerShell hook bundles for { $value0 }; these remote/compatibility bundles do not enforce capture-policy capability v1. Install local native hooks instead when policy enforcement is required.
cli-setup-agent-extracted-copied-hook-script-s-from-to = ✓ Extracted { $value0 } hook script(s) from { $value1 } to { $value2 }
cli-setup-agent-internal-generated-integration-should-have-returned-before-emitting-staged-h = internal: generated integration should have returned before emitting staged hooks
cli-setup-agent-label-uses-a-typescript-extension-plugin-not-extracted-shell-scripts = # { $value0 } uses a TypeScript extension/plugin, not extracted shell scripts.
cli-setup-agent-capture-policy-capability-v1-requires-a-freshly-generated-integration-re-run = # Capture-policy capability v1 requires a freshly generated integration; re-run --apply after upgrades.
cli-setup-agent-legacy-shell-powershell-and-remote-only-paths-are-unsupported-compatibility = # Legacy shell/PowerShell and remote-only paths are unsupported compatibility modes.
cli-setup-agent-install-it-directly-instead = # Install it directly instead:
cli-setup-agent-server-url = { "  " }--server-url { $value0 } \
cli-setup-agent-auth-token-token = { "  " }--auth-token <token>
cli-setup-agent-server-url-2 = { "  " }--server-url { $value0 }
cli-setup-agent-add-auth-token-token-if-the-server-requires-bearer-auth = { "  " }# add --auth-token <token> if the server requires bearer auth
cli-setup-agent-mcp-tools-come-through-the-same-generated-pi-bridge-extension-no-native-mcp = MCP tools come through the same generated Pi bridge extension; no native mcp.json is written.
cli-setup-agent-also-run-ai-memory-install-mcp-client-mcp-client-to-wire-mcp-separately = Also run `ai-memory install-mcp --client { $value0 }` to wire MCP separately.
cli-show-json-only-lists-launch-options-do-not-combine-it-with-launch-arguments = --json only lists launch options; do not combine it with launch arguments
cli-show-ai-memory-show-needs-a-terminal-use-ai-memory-show-json-to-list-choices-in-s = `ai-memory show` needs a terminal; use `ai-memory show --json` to list choices in scripts
cli-show-no-supported-managed-harness-was-found-in-path-looked-for = no supported managed harness was found in PATH (looked for: { $value0 })
cli-show-ai-memory-server-project-listing-unavailable-using-client-local-links-and-sc = ai-memory: server project listing unavailable ({ $value0 }); using client-local links and scan results
cli-show-ai-memory-client-project-registry-unavailable-using-scan-results = ai-memory: client project registry unavailable ({ $value0 }); using scan results
cli-show-ai-memory-ignored-stale-client-project-link = ai-memory: ignored stale client project link { $value0 }/{ $value1 } ({ $value2 })
cli-show-ai-memory-ignored-additional-stale-client-project-links = ai-memory: ignored { $value0 } additional stale client project links
cli-show-checkout-scope-changed-after-it-was-listed-expected-found-rerun-ai-memory-sh = checkout scope changed after it was listed: expected { $value0 }/{ $value1 }, found { $value2 }/{ $value3 }; rerun `ai-memory show`
cli-uninstall-would-run-program = would run `{ $value0 } { $value1 }`
cli-uninstall-note-run-ai-memory-uninstall-only-hooks-apply-inside-every-other-checkout-th = note: run `ai-memory uninstall --only hooks --apply` inside every other checkout that used `install-hooks --scope project`; their .claude/settings.local.json keeps calling this binary and data dir.
cli-uninstall-would-purge = would purge { $value0 }
cli-uninstall-dry-run-pass-apply-to-remove = (dry-run; pass --apply to remove)
cli-uninstall-proceed-with-removal-y-n = Proceed with removal? [y/N]{ " " }
cli-uninstall-aborted = aborted.
cli-uninstall-purged = ✓ purged { $value0 }
cli-upgrade-upgraded-to-tag = ✓ upgraded to { $value0 }
cli-upgrade-no-staged-hook-scripts-found-at = → no staged hook scripts found at { $value0 }
cli-upgrade-nothing-to-refresh-install-hooks-hasn-t-been-run-with-apply-yet = { "  " }(nothing to refresh — install-hooks hasn't been run with --apply yet)
cli-upgrade-no-staged-agent-dirs-found-under = → no staged agent dirs found under { $value0 }
cli-upgrade-skipping-generated-name-integration-err = { "  " }(skipping generated { $value0 } integration — { $value1 })
cli-upgrade-refreshing-generated-name-integration = → refreshing generated { $value0 } integration
cli-upgrade-skipped-err-re-run-ai-memory-install-hooks-agent-name-apply = { "      " }(skipped — { $value0 }; re-run `ai-memory install-hooks --agent { $value1 } --apply`)
cli-upgrade-updating-the-ai-memory-claude-code-plugin = → updating the ai-memory Claude Code plugin
cli-upgrade-the-ai-memory-codex-plugin-is-installed-run-codex-plugin-marketplace-upgrade = → the ai-memory Codex plugin is installed: run `codex plugin marketplace upgrade ai-memory` and `codex plugin add { $value0 }` to update it
cli-upgrade-refreshing-staged-hook-scripts-for = → refreshing staged hook scripts for: { $value0 }
cli-upgrade-no-ai-memory-hooks-in-skipping-the-user-level-re-apply-the-staged-scripts-ar = { "    " }(no ai-memory hooks in { $value0 } — skipping the user-level re-apply; the staged scripts are refreshed, but `--scope project` files are not rewritten: re-run `ai-memory install-hooks --agent { $value1 } --scope project --apply` in each such checkout to pick up hook-command changes)
cli-upgrade-restaged = { "    " }restaged { $value0 }
cli-upgrade-skipped-err-re-run-ai-memory-install-hooks-agent-name-scope-project-apply-in = { "      " }(skipped — { $value0 }; re-run `ai-memory install-hooks --agent { $value1 } --scope project --apply` in each checkout)
cli-upgrade-skipped-err-re-run-with-the-same-server-url-auth-token-used-originally = { "      " }(skipped — { $value0 }; re-run with the same --server-url / --auth-token used originally)
cli-upgrade-note-ai-memory-server-url-looks-remote-this-upgrade-refreshed-only-the-local = note: AI_MEMORY_SERVER_URL={ $value0 } looks remote — this upgrade refreshed only the local binary and hooks; redeploy the remote server separately
cli-workstream-search-no-matching-managed-workstream-events = No matching managed-workstream events.
cli-workstream-search-event = ## Event { $value0 } | { $value1 } | { $value2 }
cli-workstream-search-source-record = Source record: { $value0 }
cli-workstream-search-provenance = Provenance: { $value0 }
cli-workstreams-warning-the-ai-memory-server-ignored-workstream-pagination-showing-its-first = warning: the ai-memory server ignored workstream pagination; showing its first { $value0 } workstreams. Upgrade the server to list them all.
cli-write-page-path-was-passed-empty-specify-the-wiki-page-path-to-write = --path was passed empty; specify the wiki page path to write
cli-write-page-wrote-page-id-under = ✓ wrote { $value0 } (page_id={ $value1 }) under { $value2 }/{ $value3 }
cli-install-hooks-scope-project-is-only-supported-for-agent-claude-code-ai-memory-writes-no-pr = `--scope project` is only supported for `--agent claude-code`; ai-memory writes no project-local hook file for { $value0 }
cli-install-hooks-ai-memory-warning-could-not-persist-the-hook-auth-token-under-e-the-token-co = [ai-memory] warning: could not persist the hook auth token under { $value0 } ({ $value1 }). The token comes from [auth].bearer_token_file and is never embedded in an agent config, so these hooks will not authenticate until the data dir is writable and `install-hooks --apply` is re-run.
cli-install-hooks-ai-memory-warning-could-not-persist-the-hook-auth-token-under-e-embedding-it = [ai-memory] warning: could not persist the hook auth token under { $value0 } ({ $value1 }); embedding it in the rendered hook config instead so capture still works. The token is then readable in that file / process argv. To keep it out (recommended), install with a writable host data dir — e.g. `AI_MEMORY_DATA_DIR=$HOME/.local/share/ai-memory` for the docker wrapper — or a native `ai-memory` binary.
cli-install-hooks-ai-memory-hooks-installing-for-user-user = [ai-memory] hooks installing for user: { $value0 }
cli-install-hooks-ai-memory-capture-policy-capability-v1-enforced-by-this-selected-integration = [ai-memory] capture-policy capability v1 enforced by this selected integration; re-run --apply to refresh existing installs.
cli-install-hooks-ai-memory-selected-shell-powershell-compatibility-path-does-not-enforce-capt = [ai-memory] selected shell/PowerShell compatibility path does not enforce capture-policy v1; use a native platform selection or generated integration.
cli-install-hooks-capture-assistant-requires-agent-claude-code-codex-or-opencode2-on-a-native = --capture-assistant requires --agent claude-code, codex, or opencode2 on a native hook platform (PosixNative/WindowsNative). The current selection uses the script fallback or a different agent, where the opt-in cannot take effect. Remove --capture-assistant or switch to a supported native install.
cli-install-hooks-no-capture-prompts-and-capture-prompts-require-agent-claude-code-other-agent = --no-capture-prompts and --capture-prompts require --agent claude-code. Other agents may use their prompt hook to deliver handoff context, so removing it could break cross-agent continuity.
cli-install-hooks-capture-mode-capture-mode = capture mode: { $value0 }
cli-install-hooks-repositories-without-a-ai-memory-toml-marker-emit-no-lifecycle-events = { "  " }repositories without a .ai-memory.toml marker emit no lifecycle events
cli-install-hooks-warning-this-install-uses-script-hooks-which-post-directly-and-cannot-enforc = { "  " }WARNING: this install uses script hooks, which POST directly and cannot enforce the mode. Allowlist is stored but NOT in force here.
cli-api-key-creating-api-credential = creating API credential
cli-api-key-listing-api-credentials = listing API credentials
cli-api-key-no-api-credentials = (no API credentials)
cli-api-key-rotating-api-credential = rotating API credential
cli-api-key-revoking-api-credential = revoking API credential
cli-api-key-revoked-api-key = ✓ revoked API key '{ $value0 }'
cli-api-key-verb-api-key-store-this-token-now-it-will-not-be-shown-again = ✓ { $value0 } API key '{ $value1 }' ({ $value2 }){ "\u000A" }{ "\u000A" }Store this token now — it will NOT be shown again.
cli-auth-issuer-is-required-for-auth-login-oidc-device = --issuer is required for `auth login oidc-device`
cli-auth-client-id-is-required-for-auth-login-oidc-device = --client-id is required for `auth login oidc-device`
cli-auth-enable-auth-already-required-by-ai-memory-auth-token-or-auth-bearer-token-nothing-c = auth: already required by AI_MEMORY_AUTH_TOKEN or [auth].bearer_token; nothing changed.
cli-auth-enable-auth-bearer-token-at-owner-only = auth: bearer token { $value0 } at { $value1 } (owner-only)
cli-auth-enable-hooks-given-the-token-through-the-hook-credential-files = hooks: given the token through the hook credential files
cli-auth-enable-hooks-already-use-the-token = hooks: already use the token
cli-auth-enable-hooks-another-credential-is-stored-for-them-and-was-kept-re-run-ai-memory-in = hooks: another credential is stored for them and was kept; re-run `ai-memory install-hooks --agent <agent> --apply` to switch them to this token
cli-auth-enable-mcp-client-re-registered-in-so-it-reads-the-token-without-holding-it = mcp { $value0 }: re-registered in { $value1 } so it reads the token without holding it
cli-auth-enable-mcp-client-targets-url-another-server-left-unchanged = mcp { $value0 }: targets { $value1 }, another server; left unchanged
cli-auth-enable-mcp-client-needs-a-manual-change-reason = mcp { $value0 }: needs a manual change: { $value1 }
cli-auth-enable-restart-the-ai-memory-server-for-it-to-require-the-token = Restart the ai-memory server for it to require the token.
cli-auto-improve-staged-auto-improve-run-for-manual-approval = Staged auto-improve run { $value0 } for manual approval
cli-auto-improve-auto-approved-auto-improve-run = Auto-approved auto-improve run { $value0 }
cli-auto-improve-approval-policy = Approval policy: { $value0 }
cli-auto-improve-page = { "  " }- { $value0 } [{ $value1 }] page { $value2 } ({ $value3 })
cli-auto-improve-report-auto-improve-telemetry-for-project = { "\u000A" }Auto-improve telemetry for { $value0 }{ "\u000A" }
cli-auto-improve-report-summary = Summary: { $value0 }
cli-auto-improve-report-terminal-denominator = Terminal denominator: { $value0 }
cli-auto-improve-report-rates-approved-1-rejected-1-conflict-1-failed-1 = Rates: approved { $value0 }%, rejected { $value1 }%, conflict { $value2 }%, failed { $value3 }%
cli-auto-improve-report-runs-with-learning-proposals = Runs: { $value0 } ({ $value1 } with learning proposals)
cli-auto-improve-report-top-targets = Top targets:
cli-auto-improve-report-findings = Findings: { $value0 }
cli-auto-improve-report-more = { "  " }... { $value0 } more
cli-backfill-locating-the-local-harness-session-stores = locating the local harness session stores
cli-backfill-ai-memory-session-skipped-skipped-oversized-transcript-record-s-the-rest-was = ai-memory: { $value0 } session { $value1 }: skipped { $value2 } oversized transcript record(s); the rest was imported
cli-backfill-ai-memory-capture-mode-is-allowlist-and-this-checkout-has-no-ai-memory-toml = ai-memory: capture mode is allowlist and this checkout has no .ai-memory.toml marker opting in (or its marker is unreadable) — nothing to backfill for { $value0 }/{ $value1 }.
cli-backfill-ai-memory-already-has-captured-sessions-nothing-to-backfill-use-force-to-imp = ai-memory: { $value0 }/{ $value1 } already has captured sessions — nothing to backfill (use --force to import anyway).
cli-backfill-ai-memory-would-import-local-session-s-into-dry-run = ai-memory: would import { $value0 } local session(s) into { $value1 }/{ $value2 } (dry run).
cli-backfill-ai-memory-no-local-session-history-found-to-backfill-for = ai-memory: no local session history found to backfill for { $value0 }/{ $value1 }.
cli-backup-requesting-backup-from-server = requesting backup from server
cli-backup-agents-locating-user-home-directory = locating user home directory
cli-backup-agents-locating-current-working-directory = locating current working directory
cli-backup-agents-no-ai-agent-assets-detected-for-backup = No AI agent assets detected for backup.
cli-backup-agents-reopening-temporary-agent-archive = reopening temporary agent archive
cli-backup-agents-syncing-temporary-agent-archive = syncing temporary agent archive
cli-backup-agents-backed-up-agent-assets-to = ✓ Backed up { $value0 } agent assets to { $value1 } ({ $value2 })
cli-backup-agents-warning-secrets-were-included-without-sanitization-archive-mode-is-0600-on-u = ⚠️  Warning: Secrets were included without sanitization. Archive mode is 0600 on Unix; protect the file on other platforms.
cli-backup-agents-found-assets-across-ai-agents-dry-run = Found { $value0 } assets across AI agents (dry-run):
cli-bootstrap-getting-cwd-for-bootstrap-no-git-repo-falling-back-to = getting CWD for bootstrap (no git repo, falling back to .)
cli-bootstrap-kind-complete-for-workspace-project = { "\u000A" }{ $value0 } complete for { $value1 }/{ $value2 }{ "\u000A" }
cli-bootstrap-sources-loaded-into-the-llm = Sources loaded into the LLM:
cli-bootstrap-git-commit-summar = { "  " }- { $value0 } git commit { $value1 ->
        [one] summary
       *[other] summaries
    }
cli-bootstrap-readme = { "  " }- README
cli-bootstrap-doc-file-under-docs = { "  " }- { $value0 } doc { $value1 ->
        [one] file
       *[other] files
    } (under docs/)
cli-bootstrap-rust-module-header = { "  " }- { $value0 } Rust module { $value1 ->
        [one] header
       *[other] headers
    }
cli-bootstrap-project-rules-file-claude-md-agents-md = { "  " }- { $value0 } project-rules { $value1 ->
        [one] file
       *[other] files
    } (CLAUDE.md / AGENTS.md / ...)
cli-bootstrap-input-tokens-estimated = { "  " }-> ~{ $value0 } input tokens estimated{ $value1 }
cli-bootstrap-sequential-llm-chunk = { "  " }-> { $value0 } sequential LLM { $value1 ->
        [one] chunk
       *[other] chunks
    }
cli-bootstrap-dry-run-no-llm-call-no-pages-written = { "\u000A" }(dry-run -- no LLM call, no pages written)
cli-bootstrap-generated-wiki-page = { "\u000A" }Generated { $value0 } wiki { $value1 ->
        [one] page
       *[other] pages
    }:
cli-bootstrap-rationale = { "\u000A" }Rationale: { $value0 }
cli-bootstrap-what-ai-memory-knows-now-only-the-sources-listed-above-not-every-file-in-you = { "\u000A" }What ai-memory knows now{ "\u000A" }  Only the sources listed above. NOT every file in your project,{ "\u000A" }  NOT every commit since project start, NOT runtime behaviour or{ "\u000A" }  test logs. As you use Claude Code (or another MCP agent) the{ "\u000A" }  lifecycle hooks will automatically capture your actual workflow,{ "\u000A" }  and consolidation will refine the wiki over time.
cli-curator-curator-dry-run-for-project = { "\u000A" }Curator dry-run for { $value0 }{ "\u000A" }
cli-delete-page-deleting-page-via-server = deleting page via server
cli-doctor-machine-identity-is-unavailable-showing-capture-coverage-only = Machine identity is unavailable; showing capture coverage only.
cli-doctor-muse-hooks-mcp-entry = { "  " }muse: { $value0 } (hooks { $value1 }, MCP entry { $value2 }){ "\u000A" }
cli-doctor-has-both-mcpservers-and-mcp-servers-so-muse-ignores-every-mcp-server-move-th = ⚠ { $value0 } has both `mcpServers` and `mcp_servers`, so Muse ignores every MCP server. Move the servers into one of the two maps.{ "\u000A" }
cli-doctor-has-no-schema-version-so-every-muse-command-fails-at-startup-add-schema-vers = ⚠ { $value0 } has no "schema_version", so every muse command fails at startup. Add "schema_version": 1.{ "\u000A" }
cli-doctor-plugin-plugin-id-for-agent = { "  " }plugin: { $value0 } { $value1 } for { $value2 } ({ $value3 }){ "\u000A" }
cli-doctor-the-agent-plugin-is-version-but-this-binary-is-run-ai-memory-upgrade-it-upda = ⚠ The { $value0 } plugin is { $value1 } but this binary is { $value2 }. Run `ai-memory upgrade` (it updates the Claude Code plugin too) or update the plugin in { $value3 }.{ "\u000A" }
cli-doctor-agent-also-has-ai-memory-settings-file-hooks-in-consequence-keep-one-remove = ⚠ { $value0 } also has ai-memory settings-file hooks in { $value1 }. { $value2 }. Keep one: remove the ai-memory entries from that file, or run `{ $value3 }`.{ "\u000A" }
cli-doctor-the-ai-memory-routing-skills-are-installed-in-as-well-as-in-the-plugin-so-ag = ⚠ The ai-memory routing skills are installed in { $value0 } as well as in the plugin, so { $value1 } loads both copies. Keep one: `ai-memory uninstall --only skills --apply` removes the installed ones.{ "\u000A" }
cli-doctor-an-ai-memory-mcp-server-is-registered-in-as-well-as-by-the-plugin-so-agent-l = ⚠ An ai-memory MCP server is registered in { $value0 } as well as by the plugin, so { $value1 } lists the memory tools twice. Keep one.{ "\u000A" }
cli-doctor-setting-is-set-in-your-claude-code-settings-the-plugin-s-hooks-do-not-run-an = ⚠ { $value0 } is set in your Claude Code settings: the plugin's hooks do not run and sessions are not captured.{ "\u000A" }
cli-doctor-codex-skips-of-the-plugin-s-hooks-untrusted-or-modified-open-codex-run-hooks = ⚠ Codex skips { $value0 } of the plugin's hooks (untrusted or modified). Open Codex, run /hooks, and trust them; until then Codex sessions are not captured.{ "\u000A" }
cli-doctor-the-server-at-did-not-answer-hook-events-are-being-spooled-locally = ⚠ The server at { $value0 } did not answer. Hook events are being spooled locally.{ "\u000A" }
cli-doctor-the-hook-spool-is-full-events-oldest-newly-captured-events-are-dropped-until = ⚠ The hook spool is full ({ $value0 } events, oldest { $value1 }): newly captured events are dropped until space is available.{ "\u000A" }
cli-doctor-hook-spool-event-s-waiting-for-the-server-oldest = { "  " }hook spool: { $value0 } event(s) waiting for the server (oldest { $value1 }).{ "\u000A" }
cli-doctor-codex-skips-ai-memory-hook-s-codex-runs-a-hook-only-after-you-trust-it-and-a = ⚠ Codex skips { $value0 } ai-memory hook(s): { $value1 }. Codex runs a hook only after you trust it, and again after its command changes. Open Codex, run /hooks, and trust them; until then Codex sessions are not captured.{ "\u000A" }
cli-doctor-project-coordinate-requested-current = { "  " }project coordinate: { $value0 } (requested { $value1 }, current { $value2 })
cli-doctor-collision-reason = { "    " }collision: { $value0 }
cli-doctor-project-coordinate-unavailable = { "  " }project coordinate: unavailable ({ $value0 })
cli-doctor-local-scope-marker = { "    " }local: scope { $value0 }, marker { $value1 }
cli-doctor-repository-source-source-style = { "    " }repository: source { $value0 }, style { $value1 }
cli-doctor-canonical-candidate-canonical = { "    " }canonical candidate: { $value0 }
cli-doctor-legacy-candidate-legacy = { "    " }legacy candidate: { $value0 }
cli-doctor-marker-diagnostic-message = { "    " }marker diagnostic: { $value0 }
cli-export-okf-requesting-okf-bundle-from-server = requesting OKF bundle from server
cli-finalize-session-no-open-sessions-matched = No open { $value0 } sessions matched { $value1 }/{ $value2 }
cli-finalize-session-finalized-session-s-for = Finalized { $value0 } { $value1 } session(s) for { $value2 }/{ $value3 }
cli-grant-no-grants = (no grants)
cli-grant-open-projects-admit-every-user-regardless-a-restricted-project-with-no-grant = Open projects admit every user regardless; a restricted project with no grants admits only root.
cli-grant-listing-grants = listing grants
cli-grant-who-already-holds-on-project-nothing-changed = { $value0 } already holds { $value1 } on { $value2 }; nothing changed.
cli-grant-who-previous-on-project = { $value0 }: { $value1 } -> { $value2 } on { $value3 }.
cli-grant-who-now-holds-on-project = { $value0 } now holds { $value1 } on { $value2 }.
cli-grant-no-longer-holds-anything-on-project = { $value0 } no longer holds anything on { $value1 }.
cli-grant-held-nothing-on-project-nothing-changed = { $value0 } held nothing on { $value1 }; nothing changed.
cli-install-instructions-would-write-into = # Would write into: { $value0 }{ "\u000A" }
cli-install-instructions-getting-cwd-for-install-instructions-target = getting CWD for install-instructions target
cli-install-instructions-note-neither-claude-md-nor-agents-md-exists-in-creating-claude-md-if-you-use = note: neither CLAUDE.md nor AGENTS.md exists in { $value0 }; creating CLAUDE.md. If you use Codex / OpenCode / Cursor / Gemini CLI / Antigravity CLI / Kimi Code / Kiro CLI, re-run with `--target AGENTS.md`.
cli-install-mcp-ai-memory-this-install-s-hooks-sign-in-with-oidc-keeping-the-auth-token-key = [ai-memory] this install's hooks sign in with OIDC; keeping the --auth-token key in the MCP entry, as before, so they keep doing so
cli-install-mcp-ai-memory-stored-the-bridge-s-key-in-the-mcp-entry-carries-no-token = [ai-memory] stored the bridge's key in { $value0 }; the MCP entry carries no token
cli-install-mcp-ai-memory-warning-already-holds-a-different-key-for-this-install-s-hooks-kee = [ai-memory] warning: { $value0 } already holds a different key for this install's hooks; keeping the --auth-token key in the MCP entry, as before
cli-install-mcp-ai-memory-warning-could-not-store-the-key-under-e-keeping-it-in-the-mcp-entr = [ai-memory] warning: could not store the key under { $value0 } ({ $value1 }); keeping it in the MCP entry, as before
cli-install-mcp-prime-agent-reads-the-token-from-prime-agent-token-env-export-it-where-you-l = { "  " }Prime Agent reads the token from ${ $value0 }; export it where you launch prime-agent.
cli-install-mcp-codex-reads-the-token-from-prime-agent-token-env-export-it-where-you-launch = { "  " }Codex reads the token from ${ $value0 }; export it where you launch codex.
cli-install-mcp-refusing-to-replace-user-owned-opencode-mcp-entry-in-the-selected-major-vers = refusing to replace user-owned OpenCode MCP entry '{ $value0 }' in the selected major-version location
cli-install-mcp-refusing-to-replace-a-user-owned-opencode-mcp-entry-in-the-selected-major-ve = refusing to replace a user-owned OpenCode MCP entry '{ $value0 }' in the selected major-version location while migrating the generated alternate entry
cli-install-mcp-ai-memory-warning-preserving-ambiguous-opencode-mcp-entry-in-the-alternate-m = [ai-memory] warning: preserving ambiguous OpenCode MCP entry '{ $value0 }' in the alternate major-version location
cli-install-mcp-mcp-is-present-but-not-an-object = `mcp` is present but not an object
cli-install-mcp-a-user-owned-opencode-mcp-entry-occupies-the-selected-major-version-location = a user-owned OpenCode MCP entry occupies the selected major-version location while a generated entry remains in the alternate location; preserved both, pass explicit --server-url and --auth-token after resolving the conflict
cli-install-mcp-conflicting-generated-opencode-mcp-entries-exist-in-the-v1-and-v2-locations = conflicting generated OpenCode MCP entries exist in the V1 and V2 locations; pass explicit --server-url and --auth-token before installing
cli-install-mcp-ai-memory-warning-preserving-user-owned-opencode-mcp-entry-in-the-selected-m = [ai-memory] warning: preserving user-owned OpenCode MCP entry in the selected major-version location; pass explicit --server-url and --auth-token rather than inferring from it
cli-install-mcp-internal-unsupported-json-mcp-client = internal: unsupported JSON MCP client
cli-install-mcp-mcpservers-is-present-but-not-an-object = `mcpServers` is present but not an object
cli-install-mcp-mcp-servers-is-present-but-not-an-object = `mcp.servers` is present but not an object
cli-install-mcp-servers-is-present-but-not-an-object = `servers` is present but not an object
cli-install-mcp-context-servers-is-present-but-not-an-object = `context_servers` is present but not an object
cli-install-skills-ai-memory-note-skipped-the-grok-first-turn-rule-is-not-grok-home-hooks-ai-me = [ai-memory] note: skipped the Grok first-turn rule: { $value0 } is not <grok home>/hooks/ai-memory.json, so it names no Grok home
cli-install-skills-ai-memory-warning-skipped-grok-first-turn-handoff-rule-err = [ai-memory] warning: skipped Grok first-turn handoff rule: { $value0 }
cli-list-projects-table-header = { $value0 }  { $value1 }  { $value2 }  { $value3 }  LAST UPDATED{ "\u000A" }
cli-llm-test-provider-is-required-unless-server-is-passed = --provider is required unless --server is passed
cli-llm-test-model-is-required-unless-server-is-passed = --model is required unless --server is passed
cli-llm-test-parsing-ai-memory-llm-headers-llm-headers = parsing AI_MEMORY_LLM_HEADERS / llm_headers
cli-llm-test-building-llm-provider = building LLM provider
cli-llm-test-prompt-is-required-unless-server-is-passed = --prompt is required unless --server is passed
cli-llm-test-calling-provider-for-structured-output = calling provider for structured output
cli-llm-test-calling-provider = calling provider
cli-message-popped-message-for-workspace-project = Popped message { $value0 } for { $value1 }/{ $value2 }.
cli-message-from-agent = { "  " }from: { $value0 }/{ $value1 } (agent: { $value2 })
cli-message-sent-by = { "  " }sent by: { $value0 }
cli-message-subject = { "  " }subject: { $value0 }
cli-message-nature = { "  " }nature: { $value0 }
cli-message-created = { "  " }created: { $value0 }
cli-message-evidence = evidence: { $value0 }
cli-mod-getting-cwd-for-project-auto-detect = getting CWD for project auto-detect
cli-move-session-verb-session-from-to = { $value0 } session { $value1 } from { $value2 } to { $value3 }:
cli-move-session-session-row = { "  " }session row:         { $value0 }
cli-move-session-warning-warning = Warning: { $value0 }
cli-move-session-would-create-project-absent-created-on-confirm = { "  " }would create project { $value0 } (absent; created on --confirm)
cli-move-session-pre-move-checkpoint-oid = Pre-move checkpoint: { $value0 }
cli-move-session-checkpoint-oid = Checkpoint: { $value0 }
cli-move-session-verb-moved-of-total-session-s-from-to = { $value0 } { $value1 } of { $value2 } session(s) from { $value3 } to { $value4 }:
cli-move-session-table-header = { $value0 } { $value1 } { $value2 } { $value3 } { $value4 } { $value5 } cwd
cli-move-session-note-rehomed-session-s-marked-re-home-already-had-their-session-row-in-the-d = Note: { $value0 } session(s) marked re-home already had their session row in the destination; only their stray rows (the counts above) were gathered.
cli-move-session-warning-warned-session-s-have-a-cwd-whose-basename-is-not-the-destination-pr = Warning: { $value0 } session(s) have a cwd whose basename is not the destination project; new sessions from those directories still resolve by basename unless a .ai-memory.toml marker pins the project.
cli-move-session-gathering-observations-out-of-scopes-into-destination = { "  " }gathering observations out of { $value0 } scopes into { $value1 }:
cli-move-session-observation-s = { "    " }{ $value0 }/{ $value1 }: { $value2 } observation(s)
cli-move-session-note-this-empties-every-scope-listed-above-not-only-the-one-named-as-the-sou = { "  " }Note: this empties every scope listed above, not only the one named as the source. Moving the session back later returns all rows to a single project — the split shown here is not restored.
cli-move-session-observations = { "  " }observations:        { $value0 }
cli-move-session-handoffs = { "  " }handoffs:            { $value0 }
cli-move-session-consolidation-jobs = { "  " }consolidation jobs:  { $value0 }
cli-move-session-auto-improve-runs = { "  " }auto-improve runs:   { $value0 }
cli-move-session-auto-improve-claims = { "  " }auto-improve claims: { $value0 }
cli-move-session-page-page-version-s-moved-retired = { "  " }page:                { $value0 } ({ $value1 } version(s) moved, { $value2 } retired)
cli-profile-profile-on-share-deployment-server = Profile: on ({ $value0 }; { $value1 } server)
cli-profile-profile-off-deployment-server = Profile: off ({ $value0 } server)
cli-profile-enabled-share = { "  " }enabled = { $value0 }, share = { $value1 }
cli-profile-scope-entr-digest-of-bytes = { "  " }scope: { $value0 }/{ $value1 } — { $value2 } { $value3 ->
        [one] entry
       *[other] entries
    }, digest { $value4 } of { $value5 } bytes
cli-profile-scope-one-private-profile-per-user-pass-user-name-to-inspect-one = { "  " }scope: one private profile per user; pass --user <name> to inspect one
cli-profile-scope-no-profile-entries-yet = { "  " }scope: no profile entries yet
cli-profile-digest-at-session-start-baseline-for-new-projects-bytes = { "  " }digest at session start: { $value0 } (baseline for new projects: { $value1 } bytes)
cli-profile-min-projects-apply-max-lines-llm = { "  " }min_projects = { $value0 }, apply_max_lines = { $value1 }, llm = { $value2 }
cli-profile-every-project-contributes = { "  " }every project contributes
cli-profile-projects-that-opted-out-profile-contribute-false = { "  " }projects that opted out ([profile] contribute = false):
cli-profile-listing-the-profile = listing the profile
cli-profile-no-profile-entries-yet = No profile entries yet.
cli-profile-forgot-the-git-history-keeps-it = Forgot { $value0 } (the git history keeps it).
cli-profile-was-not-in-the-profile-nothing-changed = { $value0 } was not in the profile; nothing changed.
cli-profile-reviewing-the-profile = reviewing the profile
cli-profile-the-profile-is-off-on-this-server = The profile is off on this server.
cli-profile-profile-share-entr = Profile ({ $value0 }): { $value1 } { $value2 ->
        [one] entry
       *[other] entries
    }
cli-profile-next-pass-will-update = Next pass will update: { $value0 }
cli-profile-not-in-the-profile-yet-needs-more-projects-or-say-it-as-a-general-rule = Not in the profile yet (needs more projects, or say it as a general rule):
cli-profile-project-more-needed-people = { "  " }{ $value0 } ({ $value1 } { $value2 ->
        [one] project
       *[other] projects
    }, { $value3 } more needed{ $value4 })
cli-profile-edited-by-hand-left-alone = Edited by hand, left alone: { $value0 }
cli-profile-forgotten-stays-out-until-you-say-it-again = Forgotten, stays out until you say it again: { $value0 }
cli-profile-fix-a-wrong-entry-ai-memory-profile-forget-path-reword-it-by-editing-the-pag = Fix a wrong entry: `ai-memory profile forget <path>`; reword it by editing the page (it then stays yours).
cli-profile-rebuilding-the-profile = rebuilding the profile
cli-profile-rebuilt-the-profile-read-project-new-candidate-entr-written-unchanged = Rebuilt the { $value0 } profile: read { $value1 } { $value2 ->
        [one] project
       *[other] projects
    }, { $value3 } new { $value4 ->
        [one] candidate
       *[other] candidates
    }, { $value5 } { $value6 ->
        [one] entry
       *[other] entries
    } written, { $value7 } unchanged.
cli-profile-llm-calls-fell-back-to-the-zero-llm-path = { "  " }LLM calls: { $value0 } ({ $value1 } fell back to the zero-LLM path)
cli-profile-left-alone-edited-by-hand = { "  " }left alone (edited by hand): { $value0 }
cli-profile-error-error = { "  " }error: { $value0 }
cli-profile-reading-the-current-directory = reading the current directory
cli-profile-reading-the-profile-entries-for-this-project = reading the profile entries for this project
cli-profile-does-not-exist-nothing-to-remove = { $value0 } does not exist; nothing to remove.
cli-profile-has-no-profile-block-nothing-to-remove = { $value0 } has no profile block; nothing to remove.
cli-profile-removed-it-held-only-the-profile-block = { $value0 }: removed (it held only the profile block)
cli-profile-profile-block-removed = { $value0 }: profile block removed ({ $value1 })
cli-project-setting-project-access = setting project access
cli-project-repo-is-now-was = { $value0 } is now { $value1 } (was { $value2 }).
cli-project-repo-was-already-nothing-changed = { $value0 } was already { $value1 }; nothing changed.
cli-project-these-users-have-written-to-repo-and-hold-no-grant-so-they-are-now-refused = These users have written to { $value0 } and hold no grant, so they are now refused:
cli-project-grant-the-ones-who-should-keep-access-ai-memory-user-grant-user-name-workspa = Grant the ones who should keep access: ai-memory user grant --user <name> --workspace { $value0 } --project { $value1 } --level write
cli-project-registry-canonicalizing-project-binding-checkout = canonicalizing project binding checkout
cli-project-registry-locking-client-project-registry = locking client project registry
cli-project-registry-serializing-client-project-registry = serializing client project registry
cli-read-page-path = path: { $value0 }{ "\u000A" }
cli-recent-page-s = { $value0 } page(s):{ "\u000A" }
cli-recovery-acquiring-the-recovery-spool-lock = acquiring the recovery spool lock
cli-recovery-restoring-previously-quarantined-hook-events = restoring previously quarantined hook events
cli-recovery-quarantining-correlated-hook-events = quarantining correlated hook events
cli-recovery-restoring-quarantined-hook-events = restoring quarantined hook events
cli-recovery-ai-memory-server-at-is-unreachable-nothing-to-recover-yet = ai-memory: server at { $value0 } is unreachable — nothing to recover yet.
cli-recovery-journal-unreadable-error = { "  " }journal: unreadable ({ $value0 })
cli-recovery-journal-entr-await-recovery-degraded-launch-es-failed-transcript-import-s = { "  " }journal: { $value0 } { $value1 ->
        [one] entry
       *[other] entries
    }{ $value2 ->
        [one] { " awaits" }
       *[other] { " await" }
    } recovery ({ $value3 } degraded launch(es), { $value4 } failed transcript import(s))
cli-recovery-spool-hook-event-s-queued-locally = { "  " }spool:   { $value0 } hook event(s) queued locally
cli-recovery-start-the-server-or-fix-ai-memory-server-url-then-run-ai-memory-recover-agai = Start the server (or fix AI_MEMORY_SERVER_URL), then run `ai-memory recover` again.
cli-recovery-ai-memory-recovering-against = ai-memory: recovering against { $value0 }…
cli-recovery-spool-another-drainer-held-the-lock-transcript-recovery-was-deferred = { "  " }spool: another drainer held the lock; transcript recovery was deferred
cli-recovery-spool-hook-event-s-delivered-remain-queued-dropped = { "  " }spool: { $value0 } hook event(s) delivered, { $value1 } remain queued, { $value2 } dropped
cli-recovery-recovery-is-incomplete-because-hook-events-remain-queued-or-were-dropped-exa = { "  " }recovery is incomplete because hook events remain queued or were dropped; exact correlated transcripts may still replace quarantined events
cli-recovery-in = { "  " }[{ $value0 }] { $value1 } { $value2 } in { $value3 }/{ $value4 } — { $value5 }
cli-recovery-events = { " " }({ $value0 } events)
cli-recovery-recovered-event-s-across-journal-entr-failed-remain = recovered { $value0 } event(s) across { $value1 } journal { $value2 ->
        [one] entry
       *[other] entries
    }; { $value3 } failed; { $value4 } remain
cli-recovery-recovery-journal-error-error = recovery journal error: { $value0 }
cli-recovery-failed-entries-stay-journaled-with-their-reason-fix-the-cause-and-re-run-ai = failed entries stay journaled with their reason; fix the cause and re-run `ai-memory recover` (re-running is safe — imports dedup)
cli-reindex-opening-store-for-reindex = opening store for reindex
cli-reindex-checking-whether-the-sqlite-store-is-clean-for-reindex = checking whether the SQLite store is clean for reindex
cli-reindex-opening-wiki = opening wiki
cli-reindex-rebuilding-index-from-wiki = rebuilding index from wiki/
cli-rename-project-one-of-from-or-project-id-is-required = one of --from or --project-id is required
cli-rename-workstream-getting-managed-workstream-checkout = getting managed workstream checkout
cli-rename-workstream-workstream-already-has-that-name = Workstream '{ $value0 }' already has that name.{ "\u000A" }
cli-rename-workstream-renamed-workstream-to-id = Renamed workstream '{ $value0 }' to '{ $value1 }'.{ "\u000A" }    id: { $value2 }{ "\u000A" }
cli-repair-backfill-timestamps-note-returned-the-maximum-scanned-sessions-for-this-checkout-older-sessions = { "  " }note: { $value0 } returned the maximum scanned sessions for this checkout; older sessions may exist and were not considered.
cli-restore-agents-backup-archive-not-found-at = backup archive not found at { $value0 }
cli-restore-agents-creating-restore-staging-directory = creating restore staging directory
cli-restore-agents-parsing-agent-backup-manifest = parsing agent backup manifest
cli-restore-agents-archive-manifest-schema-version-is-not-supported-supported-1 = archive manifest schema version { $value0 } is not supported (supported: 1..={ $value1 })
cli-restore-agents-archive-manifest-contains-duplicate-asset-path = archive manifest contains duplicate asset path { $value0 }
cli-restore-agents-archive-manifest-references-missing-asset = archive manifest references missing asset { $value0 }
cli-restore-agents-archive-asset-is-missing-its-required-sha-256-checksum = archive asset { $value0 } is missing its required SHA-256 checksum
cli-restore-agents-sha-256-integrity-mismatch-for-asset-expected-got = SHA-256 integrity mismatch for asset { $value0 }: expected { $value1 }, got { $value2 }
cli-restore-agents-archive-asset-has-unsafe-permission-bits-o = archive asset { $value0 } has unsafe permission bits { $value1 }
cli-restore-agents-archive-manifest-maps-more-than-one-asset-to = archive manifest maps more than one asset to { $value0 }
cli-restore-agents-refusing-to-replace-non-file-destination = refusing to replace non-file destination { $value0 }
cli-restore-agents-archive-contains-unmanifested-asset = archive contains unmanifested asset { $value0 }
cli-restore-agents-all-identical-count-selected-assets-are-already-identical-on-host-nothing-to = All { $value0 } selected assets are already identical on host. Nothing to restore.
cli-restore-agents-no-archive-assets-matched-the-selected-agent-and-scope-filters = No archive assets matched the selected agent and scope filters.
cli-restore-agents-restoration-complete-created-updated-identical-skipped-existing = { "\u000A" }Restoration complete: { $value0 } created, { $value1 } updated, { $value2 } identical, { $value3 } skipped (existing).
cli-restore-agents-note-this-archive-contains-sanitized-configurations-restored-mcp-configs-may = Note: this archive contains sanitized configurations. Restored MCP configs may require replacing '[REDACTED:...]' values with valid credentials.
cli-restore-agents-skip-already-exists-pass-force-to-overwrite = { "  " }[SKIP] { $value0 } (already exists, pass --force to overwrite)
cli-restore-agents-updated = { "  " }updated { $value0 }
cli-restore-agents-created = { "  " }created { $value0 }
cli-restore-agents-restore-preview-dry-run-pass-apply-to-execute = Restore preview (dry-run, pass --apply to execute):{ "\u000A" }
cli-restore-agents-asset-row = { "  " }{ $value0 } [{ $value1 }] { $value2 } ({ $value3 } bytes) -> { $value4 }
cli-restore-agents-skipped-identical-count-identical-files = { "\u000A" }  (skipped { $value0 } identical files)
cli-restore-agents-note-archive-contains-sanitized-configurations-with-redacted-credentials = { "\u000A" }  Note: archive contains sanitized configurations with redacted credentials.
cli-restore-agents-review-the-archive-source-and-listed-destinations-skills-plugins-and-instruc = { "\u000A" }Review the archive source and listed destinations: skills, plugins, and instructions are active content. Rerun with `--apply` to restore.
cli-restore-page-restoring-page-via-server = restoring page via server
cli-resume-reading-the-current-checkout = reading the current checkout
cli-search-no-results-for-query = no results for { $value0 }{ "\u000A" }
cli-search-result-s-for-query = { $value0 } result(s) for { $value1 }:{ "\u000A" }
cli-search-scoped-hit = { "  " }{ $value0 }/{ $value1 }  { $value2 }  rank={ $value3 }
cli-search-hit = { "  " }{ $value0 }  rank={ $value1 }
cli-service-host-locating-the-running-ai-memory-binary = locating the running ai-memory binary
cli-service-host-the-service-host-supervisor-stopped-unexpectedly = the service-host supervisor stopped unexpectedly
cli-setup-locating-your-home-directory = locating your home directory
cli-setup-create-the-data-dir-and-write-with-a-new-root-bearer-token-in-auth-tokens-de = { "  " }- create the data dir { $value0 } and write { $value1 } (with a new root bearer token in auth-tokens/default when the data dir is new)
cli-setup-keep-the-existing-install-at = { "  " }- keep the existing install at { $value0 } ({ $value1 })
cli-setup-install-and-start-a-at = { "  " }- install and start a { $value0 } at { $value1 }{ $value2 }
cli-setup-service-reason-2 = { "  " }- service: { $value0 }
cli-setup-wire-no-agent-none-name-one-with-agent-name = { "  " }- wire no agent: none { $value0 }; name one with --agent <name>
cli-setup-wire-these-agents = { "  " }- wire these agents:
cli-setup-number-name-leave-it-to-its-enabled-ai-memory-plugin-which-carries-the-hooks = { "      " }{ $value0 }. { $value1 }: leave it to its enabled ai-memory plugin ({ $value2 }), which carries the hooks, MCP entry and skills
cli-setup-number-name-mcp-skills-why = { "      " }{ $value0 }. { $value1 }: { $value2 } + skills ({ $value3 })
cli-setup-check-healthz-memory-status-and-doctor-read-only = { "  " }- check /healthz, memory_status and doctor (read-only)
cli-setup-also-supported-not-in-this-plan-add-with-agent-name = Also supported, not in this plan (add with --agent <name>): { $value0 }
cli-setup-nothing-was-changed-there-is-no-terminal-to-confirm-this-plan-on-re-run-ai-m = nothing was changed: there is no terminal to confirm this plan on. Re-run `ai-memory setup --yes` to apply it.
cli-setup-proceed-y-n-or-type-the-numbers-of-the-agents-to-wire-e-g-1-3 = Proceed? [y/N], or type the numbers of the agents to wire (e.g. 1 3):{ " " }
cli-setup-proceed-y-n = Proceed? [y/N]{ " " }
cli-setup-reading-the-answer = reading the answer
cli-setup-calling-memory-status = calling memory_status
cli-setup-web-binding-the-setup-page-to-127-0-0-1 = binding the setup page to 127.0.0.1
cli-setup-web-open-the-setup-page-to-choose-your-agents-and-apply-url = Open the setup page to choose your agents and apply:{ "\u000A" }{ "\u000A" }  { $value0 }{ "\u000A" }
cli-setup-web-it-answers-only-on-this-machine-over-ssh-forward-the-port-first-ssh-l-port-1 = It answers only on this machine. Over SSH, forward the port first (`ssh -L { $value0 }:127.0.0.1:{ $value1 } <host>`) and open the same address. Press Ctrl-C here to stop without changing anything.
cli-setup-web-the-setup-page-was-idle-for-15-minutes-stopping-it = The setup page was idle for 15 minutes; stopping it.
cli-setup-web-serving-the-setup-page = serving the setup page
cli-setup-web-the-setup-page-closed-nothing-was-changed = The setup page closed; nothing was changed.
cli-setup-web-setup-finished-with-problems-see-the-output-on-the-page-or-run-ai-memory-set = setup finished with problems; see the output on the page, or run `ai-memory setup` here
cli-setup-web-the-setup-page-stopped-while-setup-was-still-running = the setup page stopped while setup was still running
cli-show-canonicalizing-the-current-directory = canonicalizing the current directory
cli-show-invalid-project-name-name-use-lowercase-ascii-letters-digits-dot-dash-or-und = invalid project name { $value0 }: use lowercase ASCII letters, digits, dot, dash, or underscore, starting with a letter or digit
cli-show-project-directory-already-exists = project directory already exists: { $value0 }
cli-show-writing-the-staged-project-marker = writing the staged project marker
cli-show-project-directory-appeared-while-it-was-being-prepared = project directory appeared while it was being prepared: { $value0 }
cli-show-created = created { $value0 }
cli-status-local-spool-server-unreachable-rendered = local spool (server unreachable): { $value0 }
cli-status-local-spool-server-unreachable = local spool (server unreachable):
cli-status-pending = { "  " }pending:    { $value0 }
cli-status-oldest = { "  " }oldest:     { $value0 }
cli-status-retries = { "  " }retries:    { $value0 }
cli-status-the-spool-is-full-events-newly-captured-events-are-dropped-until-space-is-av = { "  " }The spool is full ({ $value0 } events): newly captured events are dropped until space is available. Bring the server back to stop the loss.
cli-status-event-s-are-queued-locally-and-will-be-delivered-once-the-server-is-reachabl = { "  " }{ $value0 } event(s) are queued locally and will be delivered once the server is reachable again.
cli-status-windows-logon-server-unreachable-rendered = windows logon (server unreachable): { $value0 }
cli-uninstall-warning-skipping-the-active-omp-profile-error = warning: skipping the active OMP profile: { $value0 }
cli-uninstall-getting-cwd-for-hook-removal = getting CWD for hook removal
cli-uninstall-getting-cwd-for-instruction-removal = getting CWD for instruction removal
cli-uninstall-getting-cwd-for-skill-removal = getting CWD for skill removal
cli-uninstall-nothing-to-remove-ai-memory-wiring-not-found = Nothing to remove. ai-memory wiring not found.
cli-uninstall-would-remove-from = would remove { $value0 } from { $value1 }
cli-uninstall-would-delete = would delete { $value0 } ({ $value1 })
cli-uninstall-skipped-because-it-no-longer-looks-like-an-ai-memory-generated = skipped { $value0 } because it no longer looks like an ai-memory-generated { $value1 }
cli-uninstall-deleted = ✓ deleted { $value0 }
cli-uninstall-removed-empty-directory = ✓ removed empty directory { $value0 }
cli-uninstall-stopped-and-removed-the-user-service = ✓ stopped and removed the user service { $value0 }
cli-uninstall-would-stop-and-remove-the-user-service = would stop and remove the user service { $value0 }
cli-uninstall-leaving-ai-memory-setup-did-not-write-it-or-it-was-edited-since-remove-it-yo = leaving { $value0 }: `ai-memory setup` did not write it, or it was edited since; remove it yourself if it runs ai-memory
cli-uninstall-wiring-removed-ai-memory-s-server-data-live-in-its-container-volume = Wiring removed. ai-memory's server + data live in its container/volume —
cli-uninstall-tear-those-down-manually = tear those down manually:
cli-uninstall-docker-compose-f-docker-docker-compose-yml-down-v = { "  " }docker compose -f docker/docker-compose.yml down -v
cli-uninstall-docker-volume-rm-ai-memory-data-if-you-used-the-default-volume = { "  " }docker volume rm ai-memory-data   # if you used the default volume
cli-uninstall-rm-f-bin-ai-memory-the-wrapper-script-if-installed = { "  " }rm -f bin/ai-memory               # the wrapper script, if installed
cli-uninstall-local-data-dir-was-left-intact-to-wipe-it-ai-memory-reset-confirm-or-re-run = Local data dir was left intact. To wipe it: `ai-memory reset --confirm` (or re-run with --purge-data).
cli-upgrade-already-up-to-date-current = already up to date ({ $value0 })
cli-upgrade-no-github-release-asset-for-this-os-arch = no GitHub release asset for this OS/arch
cli-upgrade-creating-extract-temp-dir = creating extract temp dir
cli-upgrade-extracting-release-archive = extracting release archive
cli-upgrade-downloading-asset-tag = → downloading { $value0 } ({ $value1 })
cli-upgrade-release-archive-checksum-mismatch-expected-expected-got-actual = release archive checksum mismatch (expected { $value0 }, got { $value1 })
cli-upgrade-checksum-ok = { "  " }✓ checksum ok
cli-upgrade-executable-has-no-parent-directory = executable has no parent directory
cli-upgrade-replacing = → replacing { $value0 }
cli-upgrade-refreshing = → refreshing { $value0 }
cli-upgrade-skipping-unknown-staged-agent-dir-name = { "  " }(skipping unknown staged agent dir `{ $value0 }`)
cli-user-creating-human-user = creating human user
cli-user-listing-users = listing users
cli-user-no-registered-users = (no registered users)
cli-user-resetting-password = resetting password
cli-user-disabling-user = disabling user
cli-user-disabled-user = ✓ disabled user '{ $value0 }'
cli-user-enabling-user = enabling user
cli-user-enabled-user = ✓ enabled user '{ $value0 }'
cli-user-patching-user = patching user
cli-user-updated-user = ✓ updated user '{ $value0 }' ({ $value1 })
cli-user-verb-user = ✓ { $value0 } user '{ $value1 }'
cli-user-name-name = { "  " }name:  { $value0 }
cli-user-email-email = { "  " }email: { $value0 }
cli-user-role-store-this-temporary-password-now-it-will-not-be-shown-again-the-user-m = { "  " }role:  { $value0 }{ "\u000A" }{ "\u000A" }Store this temporary password now — it will NOT be shown again. The user must change it on next login.
cli-workstreams-inspecting-managed-workstream-checkout = inspecting managed workstream checkout
cli-workstreams-workstream-pagination-offset-overflow = workstream pagination offset overflow
cli-workstreams-managed-workstreams-for-workspace-project = Managed workstreams for { $value0 }/{ $value1 }:{ "\u000A" }
cli-write-page-reading-body-from-stdin = reading body from stdin
cli-write-page-writing-page-via-server = writing page via server
cli-install-hooks-internal-the-muse-settings-path-is-resolved-before-apply = internal: the Muse settings path is resolved before apply
cli-install-hooks-could-not-resolve-current-dir-for-grok-project-configuration = could not resolve current dir for Grok project configuration
cli-install-hooks-grok-project-configuration-may-override-grok-home-pass-explicit-server-url-a = Grok project configuration { $value0 } may override GROK_HOME; pass explicit --server-url and --auth-token rather than inferring MCP settings
cli-presentation-doctor-capture-coverage = DOCTOR / capture coverage
cli-presentation-connection = Connection
cli-presentation-capture-coverage-for-server = Capture coverage for { $value0 }/{ $value1 } (server { $value2 })
cli-presentation-identity-server = { "  " }identity: { $value0 } ({ $value1 }, server { $value2 })
cli-presentation-identity-unavailable = { "  " }identity: unavailable
cli-presentation-auth = { "  " }auth: { $value0 }
cli-presentation-ai-memory-capture-owner-is-active-native-capture-is-suppressed-in-this-proce = { "  " }AI_MEMORY_CAPTURE_OWNER is active: native capture is suppressed in this process.{ "\u000A" }  Confirm the external producer is delivering events.
cli-presentation-capture-coverage-unavailable = { "  " }capture coverage: unavailable ({ $value0 })
cli-presentation-capture-health = Capture health
cli-presentation-recent-window-all-on-disk-sessions = { "  " }recent window: all on-disk sessions{ "\u000A" }
cli-presentation-recent-window-last-days = { "  " }recent window: last { $value0 } days{ "\u000A" }
cli-presentation-the-settings-marker-has-an-invalid-capture-section-it-fails-closed-file-and = ⚠ { $value0 }: the settings marker has an invalid `[capture]` section.{ "\u000A" }  It fails closed — file and shell tool content is reduced to metadata there, nothing leaks — but its `ignore_paths` exclusions are NOT applying while it stays invalid. Fix the TOML and re-run `ai-memory doctor` to confirm.{ "\u000A" }
cli-presentation-note-a-shell-powershell-hook-install-does-not-enforce-capture-policy-v1-excl = Note: a shell/PowerShell hook install does not enforce capture-policy v1 — "excluded" below only holds on a native hook install or a generated integration. Check the configured hook command; a dry-run `ai-memory install-hooks` reports the selected install path, not the active one.{ "\u000A" }
cli-presentation-no-local-harness-sessions-found-for-this-project-server-capture-counts-are-u = { "  " }No local harness sessions found for this project; server capture counts are unavailable.
cli-presentation-no-local-harness-sessions-found-for-this-project-and-nothing-captured-yet = { "  " }No local harness sessions found for this project and nothing captured yet.
cli-presentation-agent-coverage = Agent coverage
cli-presentation-agent-row = { "  " }{ $value0 } { $value1 } { $value2 } local ({ $value3 } recent) → { $value4 } captured
cli-presentation-no-captured-session-check-the-external-producer-and-repository-policy = { "      " }No captured session: check the external producer and repository policy.
cli-presentation-ran-here-but-nothing-was-captured-install-its-hook-ai-memory-install-hooks-a = { "      " }└ ran here but nothing was captured — install its hook:{ "\u000A" }        ai-memory install-hooks --agent { $value0 } --apply
cli-presentation-count-session-s-contain-events-from-multiple-capture-sources-backfill-can-al = { "      " }{ $value0 } session(s) contain events from multiple capture sources.{ "\u000A" }      Backfill can also mix source metadata; this does not prove duplicate capture.
cli-presentation-native-memory-file = { "      " }native memory: { $value0 } ({ $value1 } { $value2 ->
        [one] file
       *[other] files
    })
cli-presentation-capture = { "      " }capture: { $value0 }
cli-presentation-capture-verdict-unavailable-server-capture-counts-could-not-be-resolved = { "\u000A" }Capture verdict unavailable: server capture counts could not be resolved.
cli-presentation-every-harness-that-ran-in-this-project-recently-is-captured-on-the-server = { "\u000A" }✓ Every harness that ran in this project recently is captured on the server.
cli-presentation-harness-es-ran-here-recently-with-no-captured-sessions-remedy = { "\u000A" }⚠ { $value0 } harness(es) ran here recently with no captured sessions: { $value1 }.{ "\u000A" }  { $value2 }
cli-presentation-status-memory-capture = STATUS / memory & capture
cli-presentation-server = { "  " }server:       { $value0 }
cli-presentation-data-dir = { "  " }data-dir:     { $value0 }
cli-presentation-db = { "  " }db:           { $value0 }
cli-presentation-bind = { "  " }bind:         { $value0 }
cli-presentation-auth-2 = { "  " }auth:         { $value0 }
cli-presentation-memory = Memory
cli-presentation-pages-all-versions = { "  " }pages:        { $value0 } (all versions: { $value1 })
cli-presentation-sessions = { "  " }sessions:     { $value0 }
cli-presentation-observations = { "  " }observations: { $value0 }
cli-presentation-search-index = Search index
cli-presentation-fts-pages-observations = { "  " }fts:          pages { $value0 }/{ $value1 }; observations { $value2 }/{ $value3 }
cli-presentation-embeddings-rows-latest-pages-missing = { "  " }embeddings:   { $value0 } rows; { $value1 } latest pages missing
cli-presentation-unembeddable-empty-body-no-embedder-can-cover-these = { "    " }unembeddable: { $value0 } (empty body; no embedder can cover these)
cli-presentation-dim-rows = { "    " }{ $value0 }/{ $value1 } dim { $value2 }: { $value3 } rows
cli-presentation-embed-failures-unresolved-recovered-since = { "    " }embed failures: { $value0 } unresolved ({ $value1 } recovered since)
cli-presentation-storage-links = Storage & links
cli-presentation-storage-on-disk-reclaimable-pct-1 = { "  " }storage:      { $value0 } on disk, { $value1 } reclaimable ({ $value2 }%)
cli-presentation-filesystem-free = { "    " }filesystem free: { $value0 }
cli-presentation-would-return-it-blocks-writes-while-it-runs = { "    " }`{ $value0 }` would return it (blocks writes while it runs)
cli-presentation-links-latest-page-links-unresolved-stale = { "  " }links:        { $value0 } latest-page links (unresolved: { $value1 }, stale: { $value2 })
cli-presentation-typed-edges = { "    " }typed edges: { $value0 }
cli-presentation-wiki-format-migrated-pre-migration-backup-still-on-disk-path = { "  " }wiki format:  { $value0 }; pre-migration backup still on disk: { $value1 } ({ $value2 })
cli-presentation-wiki-format-migrated = { "  " }wiki format:  { $value0 }
cli-presentation-capture-delivery = Capture & delivery
cli-presentation-write-queue-queued-capacity = { "  " }write queue:  { $value0 }/{ $value1 }
cli-presentation-spool = { "  " }spool:
cli-presentation-pending = { "    " }pending:    { $value0 }
cli-presentation-oldest = { "    " }oldest:     { $value0 }
cli-presentation-retries = { "    " }retries:    { $value0 }
cli-presentation-recovery = { "  " }recovery:
cli-presentation-journal-journal-pending-pending-journal-degraded-degraded-journal-finish-fai = { "    " }journal:    { $value0 } pending ({ $value1 } degraded, { $value2 } finish-failed)
cli-presentation-error-error = { "    " }error:      { $value0 }
cli-presentation-action-run = { "    " }action:     run `{ $value0 }`
cli-presentation-ingest-server-this-process = { "  " }ingest (server, this process):
cli-presentation-accepted = { "    " }accepted:   { $value0 }
cli-presentation-dropped-capture-policy = { "    " }dropped:    { $value0 } (capture policy)
cli-presentation-outcomes-stored-replayed-resumed-ignored-end-failed = { "    " }outcomes:   { $value0 } stored, { $value1 } replayed, { $value2 } resumed, { $value3 } ignored end, { $value4 } failed
cli-presentation-drops-unauthorized-collision-invalid = { "    " }drops:      { $value0 } unauthorized, { $value1 } collision, { $value2 } invalid
cli-presentation-shed-saturated-rate-limited = { "    " }shed:       { $value0 } saturated, { $value1 } rate-limited
cli-presentation-last-write = { "    " }last write: { $value0 }
cli-presentation-capture-mode-allowlist-repositories-without-a-ai-memory-toml-marker-send-not = { "  " }capture mode: allowlist — repositories without a .ai-memory.toml marker send nothing
cli-presentation-optional-models = Optional models
cli-presentation-providers = { "  " }providers:
cli-presentation-llm = { "    " }llm:       { $value0 }
cli-presentation-embedding = { "    " }embedding: { $value0 }
cli-presentation-retry-hint = { "    " }retry:     { $value0 }
cli-presentation-fallback-chain = { "    " }fallback chain:
cli-doctor-an-authorized-write-can-promote-this-project-name-in-place = { "    " }an authorized write can promote this project name in place{ "\u000A" }
cli-list-projects-no-projects-found = No projects found.{ "\u000A" }
cli-read-page-expired-true = expired: true{ "\u000A" }
cli-recent-no-recent-pages = no recent pages{ "\u000A" }
cli-recovery-no-session-id = (no session id)
cli-setup-named-with-agent = named with --agent
cli-workstreams-no-managed-workstreams-for-this-checkout = No managed workstreams for this checkout.{ "\u000A" }
cli-workstreams-no-linked-harnesses = no linked harnesses
cli-presentation-unavailable = unavailable

cli-help-option-help = Print help
cli-help-option-version = Print version
cli-help-commands = Commands
cli-parse-unknown-argument = Unknown argument
cli-parse-unknown-command = Unknown command
cli-parse-missing-command = A command is required
cli-parse-required-argument = A required argument is missing
cli-parse-conflicting-arguments = Arguments cannot be combined
cli-parse-value-count = Incorrect number of values
cli-parse-invalid-value = Invalid argument value
cli-parse-help-hint = Run 'ai-memory --help' for more information.
cli-help-options = Options
cli-help-arguments = Arguments
cli-help-template = { "{before-help}{name} {version}\u000A{about-with-newline}\u000AUsage: {usage}\u000A\u000A{all-args}{after-help}" }

cli-http-server-response = { $method } { $path }: server returned { $status }: { $detail }
cli-http-configured-server-hint = { "\u000A" }AI_MEMORY_SERVER_URL is set to { $value0 } but nothing answered (refused or timed out). Check the server is running, the port is reachable from this host, and (if remote) any firewall + bearer-token config matches.
cli-http-default-server-hint = { "\u000A" }AI_MEMORY_SERVER_URL is NOT set; the CLI defaulted to { $value0 } and nothing answered (refused or timed out). If your server lives on another machine (e.g. a homelab), `export AI_MEMORY_SERVER_URL=http://<server>:49374` and (if auth is on) `export AI_MEMORY_AUTH_TOKEN=<token>` before re-running.
cli-http-unreachable = could not reach { $value0 }.{ $value1 }
cli-http-request-failed = HTTP request to { $value0 } failed

cli-status-client-version = ai-memory { $value0 } (client)
cli-status-major-version-mismatch = ⚠ The client is { $value0 } but the server is { $value1 }: across a major version they may not understand each other's requests. Upgrade the older side.
cli-status-provider-disabled = disabled
cli-status-provider-unknown = { $value0 } unknown (no calls yet)
cli-status-time-unknown = unknown time
cli-status-provider-ok = { $value0 } ok (last call: { $value1 })
cli-status-provider-error = { $value0 } error ({ $value1 })
cli-status-last-error = last error: { $value0 }
cli-auth-posture-token = bearer token
cli-auth-posture-loopback = none (loopback) — fix: `{ $value0 }`
cli-auth-posture-none = none — set AI_MEMORY_AUTH_TOKEN on the server

cli-table-workspace = WORKSPACE
cli-table-project = PROJECT
cli-table-project-id = PROJECT ID
cli-table-pages = PAGES

## Interactive selections and setup completion.

cli-picker-project = Project
cli-picker-new-project = + New project
cli-picker-new-project-detail = create a directory and install its agent context
cli-picker-agent-title = Agent | { $value0 }
cli-picker-resume-title = Resume workstream
cli-picker-horizontal-hint = left/right harness
cli-picker-empty = nothing to choose from
cli-picker-no-match = No matching workstreams — edit the search or press Esc to clear
cli-picker-search-title = { $title } [{ $matching }/{ $total }] | search: { $query }
cli-picker-project-name = New project name
cli-picker-name-required = type a name
cli-picker-name-policy = use lowercase letters, digits, dot, dash, or underscore
cli-picker-name-exists = { $value0 } already exists here
cli-picker-create-hint = { "  enter create | esc cancel" }
cli-picker-search-hint = { "  type to search | ↑↓ move | ←→ harness | enter select | esc clear/cancel" }
cli-picker-paged-hint = { "  " }up/down move{ $horizontal } | pgup/pgdn page | enter select | esc cancel [{ $selected }/{ $total }]
cli-picker-basic-hint = { "  " }up/down move{ $value0 } | enter select | esc cancel
cli-setup-ready-no-agent = ai-memory is set up, but no agent is wired; run `ai-memory setup --agent <name>`.
cli-setup-ready-one = ai-memory is set up. Restart { $value0 } so it loads the new hooks and MCP entry, then ask it: "What do you remember about this project?" It answers through ai-memory's tools; a new install remembers nothing yet, and every session from now on is captured.
cli-setup-ready-many = ai-memory is set up. Restart { $value0 } so they load the new hooks and MCP entry, then ask one of them: "What do you remember about this project?" It answers through ai-memory's tools; a new install remembers nothing yet, and every session from now on is captured.
cli-setup-codex-trust = ai-memory is installed, but Codex has not approved its { $count } ai-memory hook(s) yet: { $hooks }. Codex skips a hook until you trust it, so Codex sessions are not captured until then. Action required: open Codex, run /hooks, and trust them. { $closing }
cli-setup-web-sign-in = web UI: { $value0 } (the service runs with --enable-web); when the browser asks for a username and password, use any username and the contents of { $value1 } as the password
cli-api-key-confirm-rotate = Rotate API key '{ $value0 }'? Existing clients using the old secret will 401 immediately. (y/N) { " " }
cli-api-key-confirm-revoke = Revoke API key '{ $value0 }'? (y/N) { " " }
cli-user-confirm-password = Reset password for user '{ $value0 }'? Their current password stops working immediately. (y/N) { " " }
cli-user-confirm-disable = Disable human login for user '{ $value0 }'? (y/N) { " " }
cli-setup-ready-prefix = { "ai-memory is set up. " }
cli-config-load-failed = Could not load ai-memory configuration
cli-error-report = Error: { $message }
cli-error-caused-by = Caused by:
