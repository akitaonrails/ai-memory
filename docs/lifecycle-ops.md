# Lifecycle operations

Reference for the destructive / state-touching ai-memory commands.
Read this before running anything that mutates wiki + db, especially
on a homelab box where mistakes are harder to undo.

## TL;DR - safety matrix

| Command | Safe with server **running**? | Wipes data? | Reversible? | Notes |
|---|---|---|---|---|
| `purge-project --confirm` | ✅ yes | the one project's data, **plus** any observation stamped into a different project by one of this project's sessions (cascades regardless of the observation's own `project_id`), and it nulls (does not delete) the session reference on any handoff in a different project that this project's sessions authored or accepted | no | Deletes the UUID-namespaced wiki root and raw workstream segments. Refuses with `409` while a managed workstream under the project holds a live run lease — `--force` overrides. Logical delete by default; `--compact` additionally rebuilds the FTS indexes and `VACUUM`s (see below). Without `--confirm` it previews the same counts, including the cross-project ones, before refusing — see below. |
| `purge-session --session-id --confirm` | ✅ yes | the one session's data, **plus** any observation stamped into a different project by this session (cascades regardless of the observation's own `project_id`), and it nulls (does not delete) the session reference on any handoff in a different project that this session authored or accepted | no | Deletes one session by UUID: its row, its observations, the handoffs it **authored**, its `sessions/<id>.md` page and every superseded version, their embeddings, and its auto-improve runs. Strictly scoped — a session that does not belong to the named workspace/project is a `404` and nothing is deleted. Handoffs the session only *accepted* are kept: that text belongs to the session that wrote it. Logical delete by default; `--compact` additionally rebuilds the FTS indexes and `VACUUM`s (see below). Without `--confirm` it previews the same counts, including the cross-project ones, before refusing — see below. |
| `handoffs --expire-all --confirm` | ✅ yes | no (state change only) | no (but nothing is destroyed) | Marks every **open** handoff in the scope `expired` so it stops being offered to an agent. Rows, summaries and provenance are kept and stay visible in the audit log. Unlike the automatic sweep it does **not** spare manual handoffs or ones from another directory — those exemptions are exactly what a leftover backlog is made of, so honouring them would clear nothing. `--older-than-days N` keeps recent batons. Owner-scoped: never touches another user's baton. |
| `rename-project --from --to` | ✅ yes | no | yes (rename back) | Column-only update on `projects.name`. The on-disk dir is keyed by `project_id` (UUID), so the rename never moves a file. |
| `/admin/rename-workspace` | ✅ yes | no | yes (rename back) | Column-only update on `workspaces.name`; refreshes `_meta.md` scope manifests and checkpoints the wiki tree. |
| `/admin/delete-workspace` | ✅ yes | the workspace and every child project, **plus** any observation stamped into a different workspace by one of this workspace's sessions (cascades regardless of the observation's own `workspace_id`), and it nulls (does not delete) the session reference on any handoff in a different workspace that this workspace's sessions authored or accepted | no | Runs `purge_workspace` admission first, deletes SQLite rows in one cascade, removes the UUID-keyed workspace directory and managed-workstream raw segments, reports filesystem partial failures, and dispatches mirror notification after durable work. Logical delete by default; `"compact": true` additionally rebuilds the FTS indexes and `VACUUM`s (see below). `"dry_run": true` previews the same counts, including the cross-workspace ones, without deleting anything — see below. |
| `move-project --confirm` | ✅ yes | source only in the merge case (a `Reject`-policy `purge_project` webhook can still abort the source teardown leaving everything intact) | no | Fresh destination → lossless **true move** (re-stamp `workspace_id`, keep `project_id`, rename the dir): sessions/observations/handoffs + history all survive. Destination with a same-named project → **copy+purge merge**: only latest pages migrate. |
| `move-session <id> --to --confirm` | ✅ yes | no | yes (move it back) | Re-stamps one session (or every session touching `--from-project`) into another project: `sessions`, `observations`, its `handoffs`, consolidation jobs, auto-improve runs/claims and its `sessions/<id>.md` page, one transaction per session; the page file moves with it (`--pages move`, default) or is retired for regeneration. Without `--confirm` it is a real dry run (rolled back). Refuses with `409` an open session or a pending consolidation job unless `--force`. |
| `repair-backfill-timestamps --project --confirm` | ✅ yes | no | yes, if the operator kept the audit row or the CLI's before/after report (nothing else) | Corrects `sessions.started_at`/`ended_at` for sessions an older `backfill` imported before it carried the transcript's own event time. The CLI reads the operator's local transcripts read-only and posts candidate `(session_id, started_at, ended_at)` tuples to `POST /admin/repair-session-times`, which validates each one against `(workspace, project)` — a candidate outside that scope is `not_found` and untouched — only rewrites a row whose `started_at` postdates the candidate's own end (`not_flattened` otherwise, so a correctly hook-captured session or a re-run is a no-op), refuses negative/inverted/future-dated times, and never assigns an end time to a session still open. Without `--confirm` the server validates and reports inside a rolled-back transaction, so it is a real dry run; a confirmed run writes one `audit_log` row per request with the before/after times. Touches only the two timestamp columns of rows already in scope; `observations` and pages (including the session's own page frontmatter) are untouched. |
| `backup --to` | ✅ yes | no | n/a | Streams a gzipped tarball from the server's online `sqlite3 .backup` plus the wiki tree. Safe alongside the live writer. |
| `reclaim-ledger-versions` | ✅ yes | superseded pre-#660 ledger page *versions* only | no (the latest page version stays) | Dry-run by default; needs `--confirm` to delete. Online through the writer actor, safe alongside the live writer. Content-gated: only non-latest, non-decay `log.md`/`log-YYYY-MM.md` versions whose body opens with a ledger hook entry are removed; a real page that merely shares the name is untouched. `--compact` additionally rebuilds FTS and `VACUUM`s to reclaim the freed bytes. |
| `checkpoints` | ✅ yes | no | n/a | Lists recent wiki git checkpoints. Read-only. |
| `restore-page --path --from` | ✅ yes | overwrites one markdown page version | yes (restore another checkpoint) | Restores one page from wiki git history, reindexes it into SQLite, and writes a post-restore checkpoint. Does not restore DB-only state. |
| `backup-agents --to` / `restore-agents --from <tarball>` | ✅ yes (client-side; never touches the server or its data dir) | no | yes (`restore-agents --apply` keeps timestamped copies of overwritten files) | Agent-config assets, not wiki/db: MCP configs (redacted unless `--include-secrets`), hook configs, instructions, skills, plugins. `restore-agents` is a dry run by default; `--apply` writes to disk and `--force` overwrites existing or differing targets. Non-MCP assets are restored verbatim and may contain secrets — only `--apply` an archive whose source you trust. |
| `restore --from <tarball>` | ❌ **stop the server first** | overwrites the data dir | no (without prior backup) | Refuses if any sibling `ai-memory` process is alive (sysinfo guard). Stages and verifies the archive before swapping it in, so a failed restore leaves `wiki/` and `db/` as they were. |
| `reset --confirm` | ❌ **stop the server first** | yes, all data | no | Refuses if any sibling `ai-memory` process is alive (sysinfo guard). |
| `reindex` | ❌ **stop the server first** | no wiki wipe; requires a clean DB | only with prior DB backup | Rebuilds pages/links/FTS from `wiki/` using `_meta.md` manifests. Refuses if SQLite already has rows so stale DB-only state cannot survive silently. |

State-touching commands route through the HTTP admin API except `reset`,
`restore`, and `reindex`, which are direct-disk lifecycle operations that
fundamentally cannot run while another process holds the SQLite WAL writer. See
[CLAUDE.md §16](../CLAUDE.md) for the invariant.


## What "deleted" means (`purge-session`, `purge-project`, `delete-workspace`)

`purge-session` answers *"forget this conversation"*: after it runs, the
session is gone from the API, from `status` counts and from search.
It removes both earlier and later summary versions identified as belonging to
that session; hand-written versions at the same path remain, including a later
live wiki file.

```bash
ai-memory purge-session \
  --workspace default --project my-app \
  --session-id 0199f3d2-1c4e-7a10-9f3b-2b0c5d8e7a11 \
  --confirm
```

It is a **logical delete**, and the distinction matters if you are answering a
regulatory erasure request rather than tidying up:

| | default | `--compact` |
|---|---|---|
| Reachable through the API / MCP tools | no | no |
| Returned by search (FTS) | no | no |
| Live wiki Markdown file | no, best-effort | no, best-effort |
| Bytes still present in `memory.sqlite` | **yes**, in free pages | no |
| Text still in the wiki git history | **yes** | **yes** |
| Present in backups taken before the purge | **yes** | **yes** |
| Cost | one transaction | rewrites the whole database |

`--compact` rebuilds the affected FTS5 indexes — an ordinary delete leaves the
tokens inside the index segments, which is why a rebuild rather than a
`VACUUM` alone is what clears them — and then `VACUUM`s to release the freed
pages. It needs free disk space of roughly the database's own size and takes
minutes on a large store, which is why it is opt-in.

**`--compact` is not forensic erasure.** The wiki git repository stores page
content in its objects *and its commit messages*, and any backup taken before
the purge still contains everything. Removing the bytes from the live SQLite
file is worth doing on its own terms; do not describe it to a user as a
guarantee that the content is unrecoverable, because it is not.

### Preview without `--confirm`

Without `--confirm`, the CLI first asks the server for a preview
(`"dry_run": true` in the request, exactly like `purge-project`'s own field).
`dry_run` always wins over `confirm` — `{"confirm": true, "dry_run": true}`
still only previews — so a preview request can never become destructive by
accident.

The preview runs the same lookups and counts a confirmed purge uses to decide
what to delete — same 404 for a session outside the named scope — including
two cross-project counts for what purging this *session* collaterally
deletes or orphans in a *different* project through its own id
(`collateral_observations_deleted`, `collateral_handoffs_denulled` — the same
shape `purge-project`'s preview reports one level up, at project rather than
session granularity), without ever issuing the `DELETE`. It never runs the
delete and rolls it back. Because nothing is deleted, `removed_paths` in the
reply names the wiki page paths a confirmed purge *would* remove — not paths
already gone — and `files_deleted`/`files_failed` are always empty, since no
file is touched; neither the `purged_sessions` tombstone nor the `audit_log`
row is written, and neither checkpoint is taken. The reply carries
`"dry_run": true`. The CLI prints:

```
Would purge session from default/my-app: 1063 observations, 0 handoffs, 1 pages, 0 auto-improve runs.
```

(with a trailing "Plus N observations in other projects via this session" /
"Plus N handoffs ..." clause when either cross-project count is non-zero;
note the session id itself is never printed — the caller already has it, and
this command exists to make a session stop existing), then still refuses with
the existing "destructive and irreversible" message and a non-zero exit — the
preview is information layered on top of the refusal, never a substitute for
`--confirm`.

A preview also skips the blocking admission call a confirmed purge makes
before deleting anything (`admit_purge_session`): nothing was decided yet, so
there is nothing for a `Reject`-policy webhook to act on. This means a `200`
preview is not a guarantee — that same webhook only runs on the confirmed
path and can still refuse the real purge afterward.

If the server is unreachable, times out (a few seconds, auth-token refresh
included), or predates this field (a plain `400`), the CLI falls back
silently to the plain refusal with no preview line. A `404`/`403` (or any
other unexpected status) prints the server's own error before the refusal
instead, since the operator asked what would happen and the server has a
real answer.

A handoff this session only *accepted* (did not author) keeps its row and
its text either way — confirmed or previewed — and loses only its
`accepted_by_session` pointer (`ON DELETE SET NULL`) once the session row is
actually gone; it is never counted in `handoffs_deleted`. Two more places a
purged session's id is referenced are neither counted nor previewed today,
as a known follow-up: `agent_messages.from_session_id` /
`claimed_by_session` (the cross-project mailbox, V64) and another project's
`auto_improve_runs.session_id`, both `ON DELETE SET NULL` and out of scope
for this preview's two `collateral_*` fields, which only cover observations
and handoffs — the same set `purge-project`'s own preview covers, one level
up.

### The same is true of `purge-project` and `delete-workspace`

Every row above applies unchanged to `ai-memory purge-project --compact` and to
`POST /admin/delete-workspace` with `{"compact": true}`. The table is a
property of *any* SQLite delete, not of one command: rows go, bytes stay in
free pages until the file is rewritten.

This is worth saying explicitly because the help text for those two commands
used to promise "ALL its data", which read as byte-level removal they never
performed (#540). Both now describe the same boundary and both accept the same
opt-in.

One extra detail applies at project and workspace scope. A managed
workstream's `workstream_events` rows leave through the
`projects → workstreams → workstream_events` cascade rather than through a
`DELETE` against that table, and a cascade does not fire the `AFTER DELETE`
trigger that would drop the row from `workstream_events_fts`. Compaction
therefore rebuilds **all three** FTS indexes — `pages_fts`,
`observations_fts` and `workstream_events_fts` — not just the two a session
purge needs. Rebuilding only the indexes a given caller "should" have touched
is what leaves a managed agent's transcript text in the file after an operator
asked for it to be reclaimed.

Session purges hold the wiki mutation guard across the database deletion and
file cleanup. In-flight page writes and watcher reindexes finish before the
purge starts; new ones wait until cleanup completes. This also serializes the
purge with wiki project/session moves. Admission webhooks run before this guard.
File cleanup failures still leave the database purge committed and are reported
in `files_failed`; this coordination does not provide crash-atomic rollback.

The guard is taken before the purge is submitted to the writer actor, so it also
covers the wait for whatever that single queue is already draining, and — with
`compact: true` — the `VACUUM` that runs after the delete commits. A purge on a
busy server therefore holds up wiki mutations for longer than the delete itself.
The git checkpoints taken before and after the purge sit outside the guard: they
bracket it, they do not snapshot it.

### Scope containment

The session id is never authority on its own. Every statement is filtered on
`workspace_id` and `project_id` as well as `session_id`, the session must
belong to the named scope or the call is a `404` with nothing deleted, and the
derived pages are deleted by **id** rather than by path — two projects can
hold the same `sessions/<uuid>.md`, and deleting by path would take the other
one with it. `/admin/purge-session` runs the admission chain before any row is
touched, so a `failure_policy = reject` webhook can still abort the whole
operation while the data is intact. After the SQL transaction commits, the
server removes only the returned page paths under that same UUID-keyed project
root; the response keeps `removed_paths` for the logical DB purge and reports
actual cleanup in `files_deleted` / `files_failed`. If filesystem removal fails,
the DB purge is not rolled back, the call still returns 200 with `files_failed`
populated, and async `purge_session` observers receive `partial_failure: true`.


## What "project isolation" means here

Every project's data lives under an isolated, UUID-keyed root on disk:

```
<wiki_root>/
├── .git/
├── <workspace_id>/
│   ├── _meta.md                 # workspace name for rebuilds
│   └── <project_id>/
│       ├── concepts/
│       ├── decisions/
│       ├── gotchas/
│       ├── sessions/
│       ├── _rules/
│       ├── _meta.md             # project name, path, optional identity keys
│       ├── log-YYYY-MM.md      # rolling event log, one file per month
│       └── bootstrap.md
└── <other_workspace_id>/
    └── <other_project_id>/
        └── ...
```

The mutable **project name** (the human-readable `distrobox-gaming`
or `.config` you see in `/web/`) never appears in any disk path; the
stable **project_id UUID** does. SQLite's `projects.name` column maps
name → id. Two projects can have the exact same `pages.path` (e.g.
both have `decisions/0001.md`) without colliding on disk - the
namespaced layout guarantees structural isolation.

The git history is rooted at `<wiki_root>` (one repo, all projects
as subtrees). A `git log` from inside the wiki dir shows changes
across every project; per-project diffs are also possible via
`git log -- <workspace_id>/<project_id>/`.

Each workspace directory also carries `<workspace_id>/_meta.md`, and each
project directory carries `<workspace_id>/<project_id>/_meta.md`. Those small
frontmatter-only manifests store human names (plus `repo_path` for projects),
so a clean SQLite DB can be rebuilt from the UUID-keyed wiki tree alone.

## Command-by-command

### `purge-project`

```bash
ai-memory purge-project --workspace default --project my-project --confirm

# …and to reclaim the freed bytes as well (slow; rewrites the whole database):
ai-memory purge-project --workspace default --project my-project --confirm --compact
```

Like `purge-session`, this is a logical delete unless `--compact` is given —
see [What "deleted" means](#what-deleted-means-purge-session-purge-project-delete-workspace).

What happens, in order:

1. Server looks up `(workspace_id, project_id)` by name. Returns 404
   if either is missing.
2. Refuses with 409 when a managed workstream under the project still
   holds a **live** run lease (`managed_runs.state = 'active'` AND
   `lease_expires_at` in the future). `workstreams` cascades out of
   `projects` and `managed_runs` cascades out of `workstreams`, so
   purging would delete a running agent's lease row: its heartbeat
   would then fail with `409 managed run lease is not active` for the
   rest of the session and the transcript would never reach the ledger.
   A lapsed lease (crashed wrapper) does **not** block the purge.
   `--force` purges anyway.
3. Counts rows that will cascade (`pages`, `sessions`,
   `observations`, `handoffs`, `page_embeddings`, plus `workstreams`
   and `managed_runs`).
4. Single `DELETE FROM projects WHERE id = ?` - the V01 + V05
   `ON DELETE CASCADE` foreign keys propagate to every dependent
   table in one transaction.
5. Best-effort filesystem cleanup removes both the UUID-namespaced wiki root
   and every `<data_dir>/raw/workstreams/<workstream_id>/` segment directory.
6. Returns a summary: `{label, pages_deleted, sessions_deleted, …,
   workstreams_deleted, managed_runs_deleted, workstream_ids,
   files_deleted: [<project_root>, <raw_workstream_dir>, ...],
   files_failed: [...]}`.

`workstream_ids` remains in the report for auditability. Each corresponding
raw segment directory is removed on the server and appears in
`files_deleted`; a failed removal appears in `files_failed` alongside wiki
cleanup failures.

#### Preview without `--confirm`

Without `--confirm`, the CLI first asks the server for a preview
(`"dry_run": true` in the request). `dry_run` always wins over `confirm` —
`{"confirm": true, "dry_run": true}` still only previews, the same way
`reclaim-ledger-versions` treats its own `dry_run` field — so a preview
request can never become destructive by accident.

The preview runs the same lookups and counts steps 1-3 above use to decide
what a confirmed purge would delete — same 404 on an unknown scope, same
`409` on a live managed-run lease without `--force` — and returns those
counts, including the two cross-project ones (an observation deleted, or a
handoff's session reference nulled, in a project other than the one named;
see the matrix row above), without ever issuing the `DELETE` in step 4. It
does not run the delete and roll it back: on a large project that would cost
as much writer-actor time as a real purge (every hook capture queued behind
it pays for that), for no benefit over just counting. Because step 4 never
runs, step 5's filesystem cleanup never runs either (`files_deleted` /
`files_failed` are always empty), and neither the `purged_scopes` tombstone
nor the `audit_log` row from step 4's transaction is written; neither
checkpoint is taken. The reply carries `"dry_run": true`. The CLI prints:

```
Would purge default/my-project: 3 pages, 1 sessions, 1063 observations, 0 handoffs, 3 embeddings, 0 workstreams, 0 managed runs.
```

(with a trailing "Plus N observations in other projects via their sessions"
/ "Plus N handoffs ..." clause when either cross-project count is non-zero),
then still refuses with the existing "destructive and irreversible" message
and a non-zero exit — the preview is information layered on top of the
refusal, never a substitute for `--confirm`.

A preview also skips the blocking admission call a confirmed purge makes
before deleting anything (`admit_purge_project`): nothing was decided yet,
so there is nothing for a `Reject`-policy or scope-guard webhook to act on.
This means a `200` preview is not a guarantee — that same webhook only runs
on the confirmed path and can still refuse the real purge afterward.

If the server is unreachable, times out (a few seconds, auth-token refresh
included), or predates this field (a plain `400`), the CLI falls back
silently to the plain refusal with no preview line, so no existing script's
exit code or error shape changes — only a confirmed purge is ever
destructive. A `404`/`409`/`403` (or any other unexpected status) prints the
server's own error before the refusal instead, since the operator asked what
would happen and the server has a real answer.

Failure modes:

- **Workspace or project name not found** → 404, no mutation.
- **Confirmation flag omitted** → 400, no mutation.
- **Live managed run, no `--force`** → 409 naming the workstreams, no
   mutation. Finish or cancel the session, or re-run with `--force`
   (the running agent then stops being able to save its history).
- **`remove_dir_all` partial failure** (e.g. permissions) → DB
   rows are already gone but `files_failed` is populated. Re-run
   the command with the same args is idempotent; the second call
   returns 404 (project already deleted).

Why this is safe with the server running:

- The DB cascade is one transaction; the writer actor serialises
  it against any other writes.
- The on-disk delete touches only the project's UUID-keyed subdir,
  which no other project shares files with. No race with the
  watcher even mid-write - at worst the watcher emits delete
  events for files we just removed, which it ignores (no DB row
  to reindex).

### `rename-project`

```bash
ai-memory rename-project --workspace default --from old-name --to new-name
```

What happens:

1. Look up `(workspace_id, project_id)` by current name. 404 on
   miss.
2. Validate the new name: non-empty, no `/`, no leading/trailing
   whitespace. 422 on bad input.
3. `UPDATE projects SET name = ? WHERE id = ?`. UNIQUE-violation on
   the `(workspace_id, name)` index → 422 with "name taken".
4. Refresh the UUID-keyed project's `_meta.md` and checkpoint the wiki.
5. Return `{workspace, from, to, pages, checkpoint}` plus
   `manifest_warning` when the post-commit refresh failed.

Zero files move on disk because the disk path is keyed by
`project_id`, not name. The web UI URL `/web/w/<ws>/<proj-name>/…`
just resolves to the same `project_id` after the column update.
This command also does not rename a source checkout or rewrite any native agent
session locator. See [managed workstream rename
behavior](managed-workstreams.md#project-and-directory-renames) before
physically renaming a checkout that has native sessions.
After a successful CLI rename, the client also rekeys its local `show` checkout
link. A direct `/admin/rename-project` request cannot update other machines'
client registries; the next successful managed `run` from a checkout refreshes
its link.

Failure modes:

- **`to` name already exists in this workspace** → 422.
- **`to` invalid (empty, slash, whitespace)** → 422.
- **Source `from` not found** → 404.
- **Manifest refresh/checkpoint failed after commit** → 200 with
  `manifest_warning`; the SQL rename remains committed and startup manifest
  backfill can repair the disk metadata. The bootstrap, write-page,
  move-project merge, move-session create, and managed-run responses surface
  the same warning shape when their scope resolution performed the promotion.
  The warning is retained if the promoted operation later fails: MCP puts it at
  `error.data.manifest_warning`, while HTTP admin/managed-run errors keep the
  top-level `manifest_warning`. Their CLI commands print it to stderr; API
  callers receive it in JSON.

### `/admin/rename-workspace`

Renames a workspace by updating `workspaces.name`; on-disk paths remain keyed by
`workspace_id`, so no page files move. After the SQLite rename, the handler
refreshes `_meta.md` scope manifests with `Wiki::backfill_scope_manifests()` and
returns `manifests_refreshed` plus a post-rename checkpoint when the wiki tree
changed.

If manifest refresh fails after the SQLite rename has committed, the rename
still returns `200 OK` with `manifests_refreshed: 0` and a `manifest_warning`
string instead of reporting a misleading 500. The DB rename is authoritative at
that point; operators can rerun a manifest refresh or restore from the emitted
checkpoint if they need to repair `_meta.md` drift.

Failure modes:

- **Source `from` not found** → 404.
- **`to` name already exists or is invalid** → 422.
- **Manifest refresh failed after commit** → 200 with `manifest_warning` and
  committed DB rename.

### `/admin/delete-workspace`

Deletes a workspace row and all child projects/pages/sessions/managed
workstreams through the `workspace_id` cascade. The route is guarded by
`force: true` for non-empty workspaces and follows the destructive-operation
ordering used by project purges. It accepts the same opt-in reclaim as the
other two destructive commands — `{"compact": true}` — and is otherwise a
logical delete; see
[What "deleted" means](#what-deleted-means-purge-session-purge-project-delete-workspace).

1. Look up the workspace without creating missing scopes.
2. Run blocking `op=purge_workspace` admission. A reject-policy webhook aborts
   before DB rows or files are removed.
3. Take a pre-delete checkpoint if the wiki tree is dirty.
4. Delete the workspace in one writer-actor transaction, counting
   `projects_deleted`, `pages_deleted`, `sessions_deleted`,
   `observations_deleted`, `handoffs_deleted`, `embeddings_deleted`,
   `workstreams_deleted`, `managed_runs_deleted`, plus the two cross-workspace
   counts (`collateral_observations_deleted`, `collateral_handoffs_denulled`
   — the same shape `purge-project`'s preview reports one level down, at
   project rather than workspace granularity) before issuing the `DELETE`.
5. Remove `<wiki_root>/<workspace_id>` and every affected
   `<data_dir>/raw/workstreams/<workstream_id>` directory from disk. The
   response reports `workstreams_deleted`, `managed_runs_deleted`, and the
   cleaned `workstream_ids` alongside the filesystem results.
6. Dispatch non-blocking `purge_workspace` mirror notifications after durable
   work. If the DB delete committed but disk removal failed, the response
   includes `files_failed` and webhook `ctx.partial_failure: true`.
7. Take a post-delete checkpoint if the wiki tree changed.

Failure modes:

- **Workspace not found** → 404, no mutation.
- **Non-empty workspace without `force: true`** → 409, no mutation.
- **Reject-policy `purge_workspace` webhook fails** → 500, no DB/disk mutation.
- **Filesystem removal fails after SQL commit** → 200 with `files_failed`
  populated and `partial_failure: true` on async mirror notifications; manual
  cleanup of the reported path is required.

#### Preview with `"dry_run": true`

`"dry_run": true` in the request body always wins — `{"force": true,
"dry_run": true}` still only previews — so a preview request can never
become destructive by accident, the same pattern `purge-project` uses for
its own `dry_run` field. The preview runs step 4's
counts (including the two cross-workspace ones) against the same rows a
confirmed delete would remove, using the same `409` for a non-empty
workspace without `force` and the same `404` for an unknown workspace, and
returns before ever issuing the `DELETE`. Steps 2, 3, 5, 6, and 7 never run
under `dry_run`: no admission call, no wiki directory removal, no mirror
dispatch, no checkpoint — `files_deleted`/`files_failed` are always empty and
`pre_checkpoint`/`checkpoint` are always absent. The reply carries
`"dry_run": true`. There is no CLI subcommand for `delete-workspace` today,
so this preview is HTTP-only.

The two `collateral_*` fields cover only observations and handoffs, the same
set `purge-project`'s own preview covers. `agent_messages` (V64, the
cross-workspace mailbox) is affected the same cross-workspace way and is
neither counted nor previewed today, as a known follow-up: a message TO a
*different* workspace's mailbox is deleted outright if it was sent FROM the
workspace being deleted (`from_workspace_id` is `ON DELETE CASCADE`, with no
regard for the message's own `to_workspace_id`), and `from_session_id`/
`claimed_by_session` pointers on messages elsewhere are nulled, not deleted,
when they reference a session that lived in the deleted workspace (`ON DELETE
SET NULL`).

### `move-project`

```bash
ai-memory move-project --from-workspace default --project my-project \
  --to-workspace other-workspace --confirm
```

Moves a project into a **different** workspace. Unlike `rename-project`
(a same-workspace column update), this crosses the workspace boundary.
The destination decides which of two strategies runs — reported as
`moved_via` in the response:

**1. Fresh destination → `"true-move"` (lossless, the common case).**
When the destination workspace has **no** same-named project, the move is
a low-level re-stamp:

1. Resolve the source `(from_workspace, project)`. 404 on miss.
2. Reject `from_workspace == to_workspace` (use `rename-project`) → 422.
3. Get-or-create the destination **workspace** row (not a new project).
4. Take the wiki's exclusive mutation gate and run `op=move_project` admission
   webhooks with source names in `ctx.workspace` / `ctx.project` and
   destination names in `ctx.destination_workspace` /
   `ctx.destination_project`. A reject-policy webhook aborts before files or DB
   rows move.
5. While still holding that gate, check that the destination dir is still
   absent, then `fs::rename` the project dir
   `<wiki>/<from_ws>/<proj>` → `<wiki>/<to_ws>/<proj>` (atomic within one
   wiki root).
6. Re-stamp `workspace_id` across every domain table for the project in
   **one transaction**, keeping the same `project_id`
   (`projects`, `pages`, `sessions`, `observations`, `handoffs`, `audit_log`,
   auto-improvement state, SessionEnd consolidation jobs, managed
   `workstreams`, pending cross-project messages on both ends, entities, page
   feedback, purge tombstones, and the profile evidence). Native workstream
   sessions, runs, and events remain attached
   through `workstream_id`; `page_embeddings` and `links` remain attached
   through `page_id`, so none of those rows need a direct re-stamp.

After a successful CLI true move, or a completed copy-purge move, the client
rekeys its local `show` checkout link. Existing destination links win during a
merge. Direct admin API callers leave client-local registries untouched; a
later successful managed `run` repairs the relevant link.

Ordering is **rename-FIRST, SQL-commit-LAST**, so the **DB is never ahead of
disk**: a rename failure touches nothing; a crash between the two steps leaves
at most an orphan dir at the destination with the DB still wholly at the source
(recoverable), never a DB row pointing at a missing file. A SQL failure renames
the dir back, so the move is all-or-nothing unless the filesystem also refuses
the rollback, in which case the error names the manual repair. In-process page
writes/reindexes take the shared side of the same mutation gate and validate the
`(workspace_id, project_id)` pair before touching disk, so stale source writes
fail without creating orphan files after the move.

This is O(1) (one transaction + one rename), re-embeds nothing, and
**preserves everything** — sessions, observations, handoffs and the full
supersession history all travel with the project.

**Live-session guard.** The server refuses (409) to move the project the
hook router has published as the *active* project (a live session's next
observation would carry a now-stale `workspace_id`). Pass `--force` /
`force: true` to override — still safe: the move republishes the active
pointer, and the wiki pair validator plus `(workspace_id, project_id)` insert
trigger (V18) reject stale writes cleanly, so the router re-resolves instead of
corrupting or creating old-workspace files.

**2. Destination already has a same-named project → `"copy-purge"`
(merge).** Two distinct `project_id`s can't be re-stamped into one (it
would collide on `UNIQUE (workspace_id, name)`), so the source's latest
pages are copied into the existing destination project via
`Wiki::write_page` (sanitization, link re-resolution, FTS, and — on
deploy — the admission/git-mirror webhooks all fire), source embeddings
are carried over verbatim, and only then is the source purged
(`merged_into_existing: true`, `source_purged: true`).

`--force` overrides only the hook router's active-project guard. It never
deletes a live managed-workstream lease during this destructive path; finish
or cancel that run before retrying. A true move keeps the same project and
lease ids, so it does not need this additional guard.

Copy-before-purge means any copy failure aborts **before** the purge,
leaving the source intact. An unreadable source file is skipped and also
blocks the purge (`source_purged: false`) so a fixed re-run is safe
(re-running is idempotent — copied pages just supersede). A **live managed
run** under the source blocks the purge leg the same way: the move returns
409 saying how many pages were already copied, and the source stays intact
until the session ends and the move is re-run.

**Same-path conflicts (`on_conflict`).** When a source page's path already
exists in the destination with a different body, frontmatter, title, tier, or
pinned bit, the policy decides (identical pages are always a no-op supersession
at the same path):

- **`block`** (default) — abort the whole move with 409, listing the
  conflicting paths; the source is left intact. The safe default for a
  destructive op: nothing is overwritten or split silently. The operator
  resolves the conflicts or re-runs with an explicit policy.
- **`overwrite`** — the source page supersedes the destination page at the
  same path (the destination's prior version becomes history).
- **`duplicate`** — keep both: the source page lands at
  `<stem>-from-<src_workspace_slug>.md`, then `-2`, `-3`, … on
  further collisions. The `-from-` literal is the `DEDUP_FROM_TOKEN`
  constant in `crates/ai-memory-mcp/src/admin.rs`; if you ever
  change one, change the other. Wikilinks pointing at the original
  path are not rewritten, so the lossless `true-move` path remains
  the way to preserve paths and links.

Every conflict (overwrite/duplicate) is listed in the response `conflicts`
array (`path` → `moved_to`). Set the policy via `--on-conflict` on the CLI
or `"on_conflict": "block" | "overwrite" | "duplicate"` in the JSON body
for direct `/admin/move-project` callers. If resolving an existing identity-backed
destination promotes its legacy name first, the response includes
`manifest_warning` when that committed promotion's manifest refresh/checkpoint
fails; the CLI prints it.

**What does NOT migrate (merge case only):** in the `copy-purge` path the
source's `sessions`, `observations`, and `handoffs` (the raw episodic
capture log) are dropped by the purge, and the moved pages start a fresh
supersession chain (the real page history lives in the wiki's git
mirror). The `true-move` path has no such loss.

> **Operational caveat — moving the project the current session writes
> to.** Lifecycle hooks stamp a bounded observation on every supported tool-lifecycle event into the
> session's project. If you move that very project mid-session, the next
> hook re-creates the source (`scratch`-style) under the old workspace.
> Before moving a live project, point the repo's `.ai-memory.toml` at the
> **destination** workspace first, so new hook events already land there
> and the move is a clean no-contention operation.

Failure modes:

- **Missing `--confirm`** → 400.
- **`from_workspace == to_workspace`** → 422 (use `rename-project`).
- **Source project not found** → 404.
- **Destination workspace directory already exists** (true-move only)
  → 409 with `WikiError::DestinationExists` body — the destination
  has on-disk content for the same `(workspace, project)` UUID pair
  without a corresponding DB row; refuse and let the operator
  reconcile manually.
- **Block-policy same-path conflict** (copy-purge merge only) → 409
  with `{"error": "...", "conflicts": [paths...]}` listing every
  conflicting path. Re-run with `on_conflict=overwrite` or
  `on_conflict=duplicate` to proceed.
- **True-move admission or SQL re-stamp failure** → 500 and no
  committed move. If a rare rollback double-fault happens after the
  directory moved but before SQL committed, the error includes the
  exact manual repair.

### `move-session`

```bash
# One session, page and history included (dry run: no --confirm)
ai-memory move-session 0192b6a1-4c2e-7d3f-8a5b-1234567890ab --to NAS_general
# Apply
ai-memory move-session 0192b6a1-4c2e-7d3f-8a5b-1234567890ab --to NAS_general --confirm
# Every session of a stray project, into another workspace, page regenerated later
ai-memory move-session --from-project tmp --to NAS_general --to-workspace home \
  --pages regenerate --confirm
```

Moves one session, or every session touching `--from-project`, into another
project, in the same or another workspace. Use it when a session was
captured under the wrong project (a `cd` into `/tmp` before sticky routing,
a subagent started in a scratch directory, a repo cloned under a temporary
name) and its observations should live with the project they are about.
Unlike `reorg`, which re-derives every session's project from its `cwd`,
this names the destination explicitly and leaves the rest of the store
alone. Sessions already rooted in the destination only get their stray rows
re-homed: the `sessions` row stays, every row of theirs still lying in
another scope is gathered into the destination, and the report says
`session_moved: false` with the counts. The batch form therefore sees a
session through a `sessions` row in the source OR observations stamped into
it, which is what empties a phantom bucket that holds only observations of
sessions rooted elsewhere (mid-session routing before sticky).

**Request.** `POST /admin/move-session` takes either
`{"session_id": "<uuid>", "workspace"?, "project", "pages"?, "confirm"?,
"force"?, "create"?}` or the batch form `{"from_workspace"?, "from_project",
"workspace"?, "project", "pages"?, "confirm"?, "force"?, "create"?}`.
`workspace` (the destination) defaults to the source workspace;
`from_workspace` defaults to `default`. The CLI resolves `--from-project`
like every other scope argument (marker, else literal); `--to` and
`--to-workspace` stay literal.

**What moves, per session, in ONE transaction:** the `sessions` row, every
`observations` row of the session (including any that landed in another
scope), the `handoffs` it produced (`from_session_id`), its
`session_consolidation_jobs`, `auto_improve_runs` and
`auto_improve_scheduler_claims`, and its `sessions/<id>.md` page:

- `--pages move` (default): every version of the page is re-stamped into the
  destination and the file is renamed into the destination project
  directory, so the curated page and its supersession history follow the
  session. Refused with `409` when the destination already has a latest page
  (or a page file) at that path; retry with `--pages regenerate` or resolve
  the destination page first.
- `--pages regenerate`: the source versions are retired (`is_latest = 0`),
  the session's `summary_page_id` is cleared when it pointed at them, and the
  file is removed, so the next consolidation of the session writes a fresh
  page in the destination. The file has to go: a page file left on
  disk with no latest row is re-indexed as a new latest page by the next
  reconciliation pass.

The audit row (`op = move_session`) carries the operator when the request
was attributed. What does NOT move: `sessions.cwd` (historical truth; the
response carries `cwd_warning` when its basename is not the destination
project, since new sessions started there still resolve by basename unless a
`.ai-memory.toml` marker pins the project), other pages written during the
session (decisions, gotchas: pages are not tracked per session), handoffs the
session *accepted*, and `auto_improve_proposals` (they have no `session_id`;
they target pages in the scope they were staged in). `entities` and
`page_feedback` are not re-stamped either, unlike `move-project`, which
moves the whole project and re-stamps both.

**Order of operations (per session):** validate the destination (404 unless
`create`), reject a batch whose source is the destination (422; the single
form with the session's own project is a re-home), run the guards (the
open-session guard does not apply to a re-home, whose row does not move),
then, under the
wiki's exclusive mutation gate, fire `op=move_session` admission webhooks
(source names in `ctx.workspace`/`ctx.project`, destination names in
`ctx.destination_*`), move the file, and re-stamp SQLite. Disk goes first
and SQL last, as in `move-project`: a store failure renames the file back,
so the move is all-or-nothing unless the filesystem also refuses the
rollback, in which case the error names the manual repair. A `--confirm`
run takes a wiki checkpoint before and after; a batch takes one pair for
the whole run.

**Dry run by default.** Without `--confirm` the server runs the same
transaction and rolls it back, so the counts in the response are exact
(`dry_run: true`), and the CLI prints the exact command to apply. Nothing
is written and no audit row is left; with `--create` against a destination
that does not exist yet the dry run does not create it either: it reports
`would_create_project: true` with the counts of what would move (the CLI
says "would create project"), and only the `--confirm` run creates it.

**Guards (409 unless `--force`).** A session that is still open (no session
end recorded) may still receive events; a `pending`/`running` consolidation
job would write the page under the old scope; the batch form also refuses
the project the hook router has published as active, like `move-project`.
`--force` proceeds. A re-home (the session row already sits in the
destination) skips the open-session guard: the row does not move, so an
open session only has its stray rows gathered; the job guard still applies. Note that forcing an open session leaves the hook
router's per-session active pointer on the old scope until it expires;
sticky routing then keeps following the session row, which is now the
destination.

**Batch form.** Every session of the source scope moves one at a time, each
in its own transaction. The batch stops at the first refusal and the error
body reports `moved`/`total` and the sessions already moved (they stay
moved). A dry run of the batch shows the whole plan first.

Response (`MoveSessionReport`): `session_id`, `dry_run`, `session_moved`
(`false` for a re-home), `from`/`to`
(`{workspace, project}`), `summary` (`observations`, `handoffs`,
`consolidation_jobs`, `auto_improve_runs`, `auto_improve_claims`,
`page_versions_moved`, `pages_regenerated`), `page` (`moved` |
`regenerated` | `already in destination` for a re-home whose page rows all
sit in the destination | `none` when the session has no page anywhere),
`cwd`, `cwd_warning`, `would_create_project` (dry run with `create` only),
`pre_checkpoint` (only when the wiki tree had uncommitted changes before the
move), `checkpoint` (only when the move changed the tree), and
`manifest_warning` when `--create --confirm` resolves an identity-backed legacy
destination, commits its promotion, but cannot refresh/checkpoint `_meta.md`.
The batch surfaces the same warning and wraps reports in `{dry_run,
would_create_project?, from, to, total, moved, sessions: [...]}`.

Failure modes:

- **Neither `session_id` nor `from_project`, or both** → 400.
- **Session or destination not found** → 404 (pass `create` / `--create`
  to create the destination).
- **Batch source equals the destination** → 422 (the single form with the
  session's own project is a re-home, 200 with `session_moved: false`).
- **Open session (unless the row already sits in the destination), pending
  consolidation job, or active source project (batch)** → 409 without
  `force`.
- **Latest page or page file already at `sessions/<id>.md` in the
  destination** (`--pages move`) → 409; nothing moved.
- **Store failure after the file moved** → 500, file renamed back; the
  error names the manual repair if that rollback also fails.

**It drains every scope, not just the one you named.** Dependent rows are
matched by session id alone, so a session whose observations were scattered
across projects (pre-`sticky` mid-session routing, for instance) is gathered
whole. That is the point of the command — but it means a move can empty a
project you did not mention, and it is **not cleanly reversible**: moving the
session back later returns every row to a single project, and the original
per-row split is gone.

The dry run therefore names each scope it would drain, with counts:

```text
  gathering observations out of 2 scopes into default/acme-api:
    default/scratchpad: 3 observation(s)
    default/tmp: 1 observation(s)
  Note: this empties every scope listed above, not only the one named as the
  source. Moving the session back later returns all rows to a single project —
  the split shown here is not restored.
```

`POST /admin/move-session` reports the same list as `source_scopes`. Read it
before confirming.

### `reclaim-ledger-versions`

```bash
# Dry run — reports how many superseded ledger versions (and bytes) would go:
ai-memory reclaim-ledger-versions
# Apply, and rewrite the database to actually free the disk:
ai-memory reclaim-ledger-versions --confirm --compact
```

Before #660 the wiki indexer stored every rewrite of an OKF event ledger
(`log.md`, `log-YYYY-MM.md`) as a fresh page version. On a busy store those
superseded versions dominate the database (one report: ~95 % of a 45 GB store).
This online command deletes exactly that residue: a page version is removed only
when it is **not** the latest, is not a retention-decay version, its path matches
the ledger shape, **and** its own body opens with a `## [timestamp]` ledger hook
entry — so a genuine page that merely happens to be named `log-2026-09.md` is
never touched, and no live/latest page is affected. Deletion runs through the
single writer actor in one transaction; derived rows (embeddings, links,
feedback) cascade, and the kept latest version's supersession back-pointer is
nulled rather than cascade-deleted, so the surviving page stays reachable.

It is dry-run by default and requires `--confirm` to write. Like `purge-*`, the
logical delete alone does not shrink the file: pass `--compact` to rebuild the
FTS index and `VACUUM`. `--drop-latest` (also content-gated) additionally
reclaims the latest ledger version when you no longer need the in-wiki ledger at
all.

### `repair-backfill-timestamps`

```bash
# Dry run: report what would change for this project
ai-memory repair-backfill-timestamps --project my-app
# Apply
ai-memory repair-backfill-timestamps --project my-app --confirm
```

Corrects `sessions.started_at`/`ended_at` for sessions an older `backfill`
already imported. `backfill` used to date every imported session at import
time rather than from the transcript's own event times, flattening the whole
imported history onto one day; a companion change fixes new imports, and this
command repairs sessions a pre-fix `backfill` already wrote.

The CLI is the only part that touches the filesystem: it re-reads the
operator's local harness transcripts read-only (reusing `backfill`'s own
session discovery, so it looks at exactly the sessions `backfill` itself
would import for this checkout), and for each one computes the transcript's
first and last event timestamp. It resolves each transcript's session id the
same way the hook router does (`SessionId::from_native`: a UUID native id
as-is, any other native id hashed to a deterministic UUID v5), then posts the
candidate `(session_id, started_at, ended_at)` list to
`POST /admin/repair-session-times`, chunked at up to 2,000 candidates per
request (one transaction each) when the local batch is larger. The server is
the only part that validates and writes.

**Request.** `POST /admin/repair-session-times` takes `{"workspace",
"project", "sessions": [{"session_id", "started_at_us", "ended_at_us"?}, ...],
"confirm"?}`. `sessions` is capped at 2,000 entries per request so a malformed
or oversized client request cannot grow the write-actor transaction
unboundedly; over the cap is a 400 before any scope lookup.

**Validation, per candidate, against the row this same transaction reads for
it (never the caller's belief about it):**

- **Scope containment**, exactly like `purge-session`: a `session_id` that
  does not belong to `(workspace, project)` — wrong scope or nonexistent — is
  reported `skipped: {reason: "not_found"}` and left untouched. The two cases
  are never distinguished, so an id existing in a different project is not
  leaked to the caller.
- **A no-op candidate is `"unchanged"`, not counted as repaired.** Checked
  before the flattened-signature judgement below, so an exact repeat is
  always a no-op regardless of whether the row happens to look flattened.
- **The bug's own signature is the gate, not the caller's say-so.** A row is
  only rewritten when its current `started_at` sits AFTER the candidate's own
  end (or start, when the candidate carries no end) — the actual shape of
  "backfill dated this at import time". A row that already sits at or before
  that point is `"not_flattened"` and left untouched: this endpoint repairs
  the specific backfill bug, it is not a generic "set session times"
  primitive that would happily rewrite a correctly hook-captured session's
  real times. This also makes a re-run against an already-repaired session,
  or one a fixed `backfill` imported in the first place, a true no-op.
- **Sane times**: `started_at_us` and `ended_at_us` (when given) must be
  positive, and `started_at_us` must not be later than the end the row will
  actually have once the candidate lands (its own new end, or, when that
  stays unwritten, whatever end the row already had) — otherwise
  `"invalid_time"` or `"inverted_times"`.
- **Never into the future**: either time more than five minutes past "now" is
  `"future_time"`. Clock-skew margin, not a real correction target.
- **Never closes an open session**: when the session's current `ended_at` is
  `NULL`, the candidate's `ended_at_us` is silently withheld — `started_at` is
  still applied, and the report marks that session `end_kept_open: true`
  rather than skipping it outright.

**Dry run by default.** Without `confirm: true` the server validates and
computes the exact would-be write inside a transaction it then rolls back, so
the report (`repaired`, `skipped`, before/after times) is the literal outcome,
not an estimate — same pattern as `move-session`. `--confirm` applies through
the single `WriterHandle`, one transaction per request (chunk).

**What it touches.** Only `sessions.started_at`/`ended_at` of rows already in
the requested scope — never `observations`, pages, or any other table, and
never a row outside `(workspace, project)`. Session identity (`sessions.id`
and its 3-tuple scope) is never written; this is a correction of two
timestamp columns on rows that already exist, not a move or a delete.
`observations.created_at` and the `sessions/<id>.md` page's own frontmatter
timestamps still carry the old import-day times after a repair — only the
`sessions` row's two columns are corrected; regenerating the session page
(e.g. via a fresh consolidation) is what would bring its frontmatter in line.

**Reversibility.** A confirmed batch writes one `audit_log` row (`op =
"repair_session_times"`) per request with every repaired session's before/
after times in `detail`, and the CLI's human report prints the same old →
new values per session — an operator who kept either can restore the prior
values by hand; there is no automatic undo.

### `checkpoints`

```bash
ai-memory checkpoints
```

Lists recent wiki git commits, newest first. The short OID is enough for
`restore-page`, but the JSON output includes the full OID:

```bash
ai-memory checkpoints --json
```

What it is for:

- Finding the checkpoint just before a bad page write, delete, purge, move, or
  restore.
- Inspecting wiki history without shelling into the server's `wiki/.git` repo.

Startup creates a one-time `upgrade baseline: existing wiki tree before recovery
checkpoints` commit for existing data dirs whose wiki repo has zero commits.
Fresh empty installs still have no commit until there is content to save.

### `restore-page`

```bash
ai-memory restore-page --workspace default --project my-project \
  --path notes/foo.md --from <checkpoint>
```

What happens:

1. Server resolves `(workspace, project)` without auto-creating anything.
2. Server validates the page path.
3. Server checkpoints the current wiki tree first (`pre-restore-page ...`) when
   there are uncommitted changes.
4. Server reads the exact markdown blob for that project/page from git at
   `--from`, parses it, writes it back to the live wiki tree, and upserts a new
   latest page row in SQLite so search, links, and `/web` agree with disk.
5. Server writes a post-restore checkpoint (`restore-page ...`) when the live
   tree changed.

Failure modes:

- **Workspace or project name not found** → 404, no mutation.
- **Invalid page path** → 422, no mutation.
- **Checkpoint or file not found** → 500 with the git/libgit2 error; any
  pre-restore checkpoint remains as an audit breadcrumb.
- **Historical markdown is malformed or non-UTF-8** → 500, live file is not
  replaced.

What it does not recover:

- Sessions, observations, handoffs, users, audit rows, access counters, and
  embeddings. Those live only in SQLite and require a full `backup` / `restore`
  if you need to roll them back.

### `backup`

```bash
ai-memory backup --to /tmp/ai-memory-backup.tar.gz
```

What happens on the server:

1. SQLite online-backup API copies the live WAL DB to a temp file -
   guaranteed consistent snapshot without stopping the writer.
2. Server tar-gzips the snapshot + the wiki tree + `config.toml`.
3. Response body IS the gzipped tarball
   (`Content-Type: application/gzip`).

CLI writes the response body to `--to`. For a homelab user
this is the standard "snapshot before doing something dangerous"
move - `ai-memory backup` first, then proceed.

Restoring a backup follows the inverse:

```bash
# Stop the server first.
docker compose -f ~/deploy/ai-memory/docker-compose.yml down
# Restore (sysinfo refuses if the container is still running).
ai-memory restore --from /tmp/ai-memory-backup.tar.gz --data-dir /var/opt/docker/utils/ai-memory/data --force
# Start back up.
docker compose -f ~/deploy/ai-memory/docker-compose.yml up -d
```

The `--data-dir` flag points the CLI at the host-side path of the
docker volume (since `restore` runs directly on disk, not via the
HTTP admin API).

### `restore`

```bash
ai-memory restore --from <tarball> --data-dir <path> --force
```

Direct-disk operation. Refuses if any other `ai-memory` process is
alive (uses `sysinfo` to scan the process table).

Order of operations:

1. Check the data dir is empty (or the user passed `--force`).
2. Extract the tarball into a staging directory beside the live data
   (`<data_dir>/.restore-staging-<stamp>/`), validating every entry.
3. Open the staged store so pending migrations run and the SQLite
   snapshot is verified — still without touching the live data.
4. Swap: rename the live `wiki/` and `db/` (and `config.toml`, when the
   archive carries one) aside, rename the staged copies into place, then
   delete the previous data. Each move is a same-filesystem rename; if
   one fails, the moves already made are reversed.
5. Print a one-line summary.

The live data is therefore untouched until the archive has proven usable.
A restore that fails in steps 2–3 leaves `wiki/` and `db/` exactly as they
were, which matters because a restore is usually attempted when no other
copy exists.

Failure modes:

- **Server still running** → exits with "another ai-memory process is
  alive (pid X); stop it before restoring" - same wording as `reset`.
- **Data dir not empty + no `--force`** → exits with "data dir not
  empty; pass `--force` to overwrite".
- **Truncated or corrupt tarball, an entry outside the allowed layout, or
  a snapshot the current binary cannot open** (for example a backup taken
  by a newer release) → exits with the error; the existing `wiki/` and
  `db/` are left as they were.
- **A rename in the swap fails and cannot be reversed** → exits with an
  `INCONSISTENT STATE` message naming the
  `<data_dir>/.restore-previous-<stamp>/` directory that still holds the
  pre-restore `wiki/` and `db/`, to be moved back by hand.

### `reset`

```bash
ai-memory reset --confirm
```

Direct-disk operation. Refuses if any sibling `ai-memory` process is
alive. Removes the contents of `wiki/`, `db/`, and `raw/` under the
configured data dir. `config.toml` is preserved.

Identical sysinfo guard to `restore`. The use case is "wipe and start
over" - typically when changing major version with a breaking
migration, or when bootstrapping a new install on top of an old
data dir.

For a docker deploy where the data lives in a host-path bind mount,
you can also just `rm -rf <host-path>/*` after stopping the
container - but `ai-memory reset` is the cross-platform path that
works whether the data dir is local, bind-mounted, or in a named
volume.

### `reindex`

```bash
ai-memory reindex --data-dir <path>
```

If reindex reports a missing scope `_meta.md`, the error includes its exact
path. Restore the original DB, start and stop the current server once so its
startup backfill writes missing manifests, then retry against a clean DB.

Direct-disk lifecycle operation. Refuses if any sibling `ai-memory` process is
alive, and also refuses if SQLite already contains rows. `reindex` is a
rebuild-from-files path, not an in-place dirty-index repair.

Use it when the markdown wiki is intact but you intentionally want a fresh
SQLite migration lineage:

1. Stop the server or container.
2. Take a backup of the current data directory.
3. Move or remove `<data-dir>/db/memory.sqlite` and its WAL/SHM siblings.
4. Run `ai-memory reindex --data-dir <data-dir>`.
5. Run `ai-memory embed` after restart if you need embeddings rebuilt.

What is rebuilt:

- Workspaces and projects from `_meta.md`, preserving the UUIDs encoded in the
  wiki directory names. Project manifests optionally carry the full hostful
  repository identity and its source; the manifest also records
  `canonical_name` and `legacy_name` as consistency assertions. Reindex validates the pair and
  assertions, then derives the indexed compatibility keys from the identity.
- Latest page rows, page links, and FTS from markdown files.

What is not rebuilt:

- Sessions, observations, handoffs, users/tokens, audit rows, access counters,
  and embeddings. Those are DB-only state; keep a backup if you need them.

Every scope directory carries the `_meta.md` manifest `reindex` reads its
workspace/project name from. Identity-backed project manifests also carry
optional typed `identity`, `identity_source`, `canonical_name`, and
`legacy_name` fields. Older manifests without those
fields remain valid and rebuild identity-less projects. The manifest is written
with the scope's first page, so a project that first appears while the server is
running is rebuildable from that moment on — no restart required. The startup
backfill still runs on every boot and repairs a tree written by an older release,
or one whose manifests were removed by hand. If `reindex` reports a missing manifest,
start the server once against that data directory and let the backfill write
it, then stop the server and reindex again.

## Operator workflows

### "Fresh start" (wipe everything)

For a docker / bind-mount deploy where data lives on the host:

```bash
ssh homelab
cd ~/deploy/ai-memory
docker compose down
sudo rm -rf /var/opt/docker/utils/ai-memory/data/*
docker compose up -d
```

Or via the CLI from any machine (slower but portable):

```bash
docker stop ai-memory   # so sysinfo guard passes
ai-memory reset --confirm   # against the same data dir
docker start ai-memory
```

### "Snapshot before risky op"

```bash
ai-memory backup --to "/tmp/ai-memory-$(date +%Y%m%d-%H%M).tar.gz"
# … do the risky thing …
# … oh no something broke …
docker compose down
ai-memory restore --from /tmp/ai-memory-2026-05-23-1530.tar.gz --force
docker compose up -d
```

### "Drop one experimental project, keep everything else"

```bash
ai-memory purge-project --project experimental --confirm
# Sibling projects (ai-memory, distrobox-gaming, …) untouched.
```

### "Rename a project after moving its directory"

```bash
ai-memory rename-project --from old --to new
# Future sessions in /path/to/new will append to the same project
# (the hook router stamps by basename(cwd) = "new"); past
# observations stay under that project too because the project_id
# is stable.
```

### "Reattach a session captured under the wrong project"

```bash
ai-memory move-session <session-id> --to my-project          # dry run
ai-memory move-session <session-id> --to my-project --confirm
# Or empty a stray project into the right one, then drop the husk:
ai-memory move-session --from-project tmp --to my-project --confirm
ai-memory purge-project --project tmp --confirm
```

## Why this matters: the flat-wiki incident

Before the per-project disk layout (commits up to `e7b9a17`), the
wiki was flat: `wiki/<page-path>` regardless of project. Two
projects with the same `pages.path` shared one file on disk. The
`purge-project` handler then iterated and deleted those files,
clobbering pages owned by the sibling project. The DB rows for the
sibling survived (FK is scoped by `project_id`), but every `/web/`
click returned 404 because the on-disk file was gone.

The shipped band-aid was a `path_still_referenced` check before each
delete. The proper fix landed in `e7b9a17`: per-project disk roots
make path-collision structurally impossible. Both the band-aid and
the underlying class of bug are gone. Lifecycle ops are now safe
by construction.

This is also why `rename-project` is free: the disk path is keyed
by surrogate `project_id`, not the mutable name. Rename touches one
column; nothing moves.
