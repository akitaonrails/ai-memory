# OKF conformance (2.0)

## What this buys you

Your memory is portable beyond ai-memory. Hand a project bundle to a
teammate who runs a *different* OKF-aware tool — or no tool at all —
and they read your decisions, gotchas and procedures as ordinary
markdown with standard metadata:

```bash
ai-memory export-okf --project myproject -o myproject-bundle.tar.gz
```

The receiving side unpacks a directory of `.md` files where every page
declares its `type`, provenance (`generated`, `sources`) and freshness
(`stale_after`) in the vocabulary Google's Open Knowledge Format
standardized — greppable, Obsidian-openable, importable by anything
OKF-aware. Nothing is held hostage: the export is a validated copy of
the files ai-memory already lives on.

The rest of this page is the design: how conformance is enforced and
how existing stores migrate.

---

ai-memory's wiki is natively an **Open Knowledge Format** bundle from
2.0 on: every page a consumer reads off disk is a conformant OKF
concept file, and a project's wiki directory is a conformant bundle.
"Native" means the wiki files *are* the OKF files — no export step
forks the truth (an `export --okf` / `import --okf` pair still exists
for moving bundles across tools).

## Target: OKF v0.2

Spec: `GoogleCloudPlatform/knowledge-catalog`, `okf/SPEC.md` (verified
2026-09-01). v0.2 supersedes v0.1 with two breaking changes
(`timestamp` → `generated: {by, at}`; the `# Citations` body section →
`sources` frontmatter) and additive trust/lifecycle/provenance
families. Summary of what conformance requires:

- every non-reserved `.md` file: parseable YAML frontmatter with a
  non-empty **`type`**;
- bundle root `index.md` declares **`okf_version: "0.2"`** (the only
  index.md frontmatter allowed) and lists the directory;
- reserved names `index.md` / `log.md` follow spec structure when
  present;
- consumers MUST tolerate unknown keys — all ai-memory extension
  fields are spec-safe as-is.

## Field mapping

| OKF key | ai-memory source |
|---|---|
| `type` (required) | derived from path family + existing frontmatter: `sessions/` → `Session Summary`, `_rules/` → `Rule`, `gotchas/` → `Gotcha`, `decisions/` → `Decision`, `procedures/` → `Procedure`, `concepts/` → `Concept`, `notes/` → `Note`, `runbooks/` → `Runbook`, `_slots/` → `Invariant`/`State` (from `slot_kind`), `_lint/` → `Lint Report`, `_pending/` → `Pending Note`; `kind:` frontmatter (`fact`/`note`/`procedure`/`decision`) wins over the path default when present |
| `title` | already written by every producer |
| `description` | existing `summary` field, when present |
| `tags` | already written |
| `generated.by` | actor convention: `process:ai-memory/<version>` for the zero-LLM consolidator and system writers; `<provider-model>` (e.g. `openai-compat/qwen3:32b`) for LLM-written pages; `human:<user>` for wiki edits attributed via the watcher |
| `generated.at` | the page version's `updated_at` |
| `sources` | session provenance: pages already stamped with `session_id`/`agent` get `[{resource: "ai-memory://session/<uuid>", author: "<agent>"}]` |
| `stale_after` | existing `expires_at` (TTL), when present: an RFC 3339 value verbatim, a bare `YYYY-MM-DD` as the end of that day in UTC (`2026-10-01T23:59:59.999999Z`), since OKF timestamps carry an explicit offset |
| `status` | `deprecated` when TTL-expired but retained; otherwise omitted (spec default `stable`) |

Extension fields kept verbatim (unknown keys are conformant): `tier`,
`kind`, `slot_kind`, `entities`, `pinned`, `consolidated`,
`session_id`, `agent`, `summary`, `expires_at`.

## Bundle boundary

One **project scope directory = one bundle**: the portable unit of
knowledge is a project. Each project dir gets a generated `index.md`
(frontmatter `okf_version: "0.2"`, body = directory listing). The
existing `_meta.md` scope manifest is unchanged — it is ai-memory's
identity record; `index.md` is the OKF-facing description. Nothing in
the current tree writes `index.md`, so that reserved name is free.
`log.md` is not adopted: git is the log.

The hooks *do* write a raw per-month event ledger at the project root
(`log-YYYY-MM.md` — `## [ts] event | title` lines, no frontmatter). It
is capture, not a concept file, so the export drops it exactly as it
drops `log.md`, and the conformance gate never sees it (#748). The
exclusion is content-gated, the same way the migration scan's is
(#669): an ordinary page that happens to be named `log-2026-09.md`
still ships in the bundle and still has to declare a `type`.

## Enforcement: one choke point

Every page write funnels through `ops::upsert_page_in_tx`. A
deterministic `okf::conform_frontmatter(path, frontmatter, meta)`
normalization runs there for every new version: fills `type` /
`generated` / `sources` / `stale_after` from the mapping above,
touches nothing already present, invents nothing non-derivable.
Determinism matters: the identical-content idempotency check hashes
frontmatter, so conforming the same input twice must yield identical
bytes.

## Migration of existing stores

Order is fixed; each step gates the next:

1. **Proactive backup, first, always.** The migration compresses the
   entire data dir (wiki, SQLite DB, manifests) to
   `~/ai-memory-backup-pre-2.0-<date>.tar.gz` — outside the data dir —
   verifies the archive is listable and size-sane, and **aborts if the
   backup cannot be written or verified**. The archive path is recorded
   in the wiki meta manifest.
2. **In-place frontmatter rewrite.** Same page id, same version row,
   body untouched, `updated_at` untouched: no version explosion, no
   embedding invalidation, no `updated_at` stampede. One git commit
   ("okf-migration") on the wiki, after a pre-migration checkpoint
   commit. Reindex afterwards.
3. **Generation marker**: the migration ships as a `WikiMigration`
   (tracked in the `wiki_migrations` table), and the runner now refuses
   to open a wiki whose table records a migration this binary does not
   know (`NewerWikiFormat`) — the downgrade guard mirroring the DB
   schema-ahead rule. Scope `_meta.md` manifests get their `type` only;
   they are identity records, not concept pages.
4. **Idempotent**: a re-run migrates zero pages.
5. **Homepage notice** until the archive is deleted: path, size, date,
   plus "everything looks right → delete the archive" and "something
   is missing → restore steps" (linking `MIGRATION-2.0.md`).

Rollback: restore the archive (blunt, no git knowledge needed), or the
pre-migration git checkpoint + `reindex` (surgical).

### Repairs to already-migrated stores

A conformance bug found after a store migrated is repaired by an
idempotent startup pass, not by a new `WikiMigration`: a registered
migration name makes every older binary refuse the wiki
(`NewerWikiFormat`), a format-generation step a patch fix should not
force. The pass follows the
migration's no-churn rules (row in place, same version, `updated_at` and
`generated.at` untouched, body untouched, one git commit) and is a no-op
once the store is clean. `conform_frontmatter` applies the same repair,
so any later rewrite of an affected page (a restore, a hand edit, a
`reindex`) heals it too.

- **Date-only `stale_after`.** Builds before the fix copied a bare
  `expires_at` date into `stale_after` verbatim. `serve` rewrites a
  `stale_after` that equals its date-only `expires_at` to the instant the
  TTL names (`2026-10-01` → `2026-10-01T23:59:59.999999Z`); a
  `stale_after` that differs from `expires_at` was not derived by
  ai-memory and is left alone.

## Reconcile-delete safety net (opt-in, #929)

The watcher's 30s reconcile pass only ever reindexes create/modify events by
default — a page whose file disappears from disk stays indexed until
`ai-memory delete-page` removes it explicitly. `[maintenance]
reconcile_tombstones_deleted_pages` (default `false`) opts into a background
safety net that closes that gap, designed to be safe even though it adds an
automatic mutation to a background loop:

- A page's file must be observed missing on **two consecutive** reconcile
  passes (not one) before anything happens, and the check is re-verified
  against the live filesystem immediately before acting.
- Only pages the walk could ever have returned are eligible — `bootstrap.md`,
  `_meta.md`, and `_pending/` sidecars are never candidates, and a project
  whose walk hit `NotFound` (a vanished subdirectory, a whole project
  directory gone mid-pass) contributes no "missing" evidence for that pass.
  `sessions/*.md` pages are excluded too, for an unrelated reason: a
  same-workspace `move-session` re-home can leave a correct DB row with no
  file at its new scope (a separate, pre-existing bug in `move-session`'s
  file relocation — tracked as a follow-up, not fixed here). This mechanism
  is meant for OKF-imported content pages only.
- A circuit breaker refuses to act on a whole scope when more than `max(3,
  50%)` of its candidate pages look missing in one pass, OR when a
  non-partial walk finds nothing at all in a scope that has candidates — the
  latter catches the "directory exists but came back empty" mount-failure
  signature for scopes too small to ever trip the percentage math. Either
  shape is far more likely a walk/mount problem (an unmounted volume, a git
  checkout mid-walk) than genuine mass deletion, and is logged at `warn`.
- The action itself is a soft tombstone (`is_latest = 0` + `superseded_at`),
  the same row shape decay eviction already uses, and is picked up by the
  exact same aged-tombstone hard-delete sweep decay eviction uses
  (`hard_delete_after_days`, tier/pin-agnostic) — it is NOT exempt from that
  sweep. The precise guarantee: **a reconcile tombstone is never itself
  destroyed while its chain has no successor; if the file returns, the new
  version re-links to the tombstoned chain instead of starting fresh, so
  nothing is orphaned for the 180-day sweep to destroy.** Concretely, a fresh
  write at a tombstoned path (`upsert_page_in_tx`) supersedes the tombstoned
  row via `supersedes` and clears its `superseded_at`, turning it into an
  ordinary, protected supersession-chain member — the same mechanism that
  already keeps normal edit history off that sweep. It also never dispatches
  the BLOCKING admission gate (nothing gets to refuse it — this is a
  background safety net reacting to an already-vanished file, not a
  user-initiated delete) and never touches the filesystem (there is nothing
  to write; the file is already gone); it DOES fire-and-forget any
  non-blocking observer/mirror webhook on success, so a mirror learns about
  the tombstone instead of silently diverging. `restore-page` and the page's
  version history remain intact.
- With the flag off (the default), behavior is byte-identical to before this
  feature existed: nothing new is logged, and reconcile never mutates the
  store.

## Tests (each with a control that must fail on a broken build)

- Round-trip: page → OKF file on disk → parsed back identical.
- Conformance: every file in a migrated store has parseable
  frontmatter + non-empty `type`; bundle root carries `okf_version`.
- No-churn: page ids, version rows, and `updated_at` byte-identical
  across migration (control: a migration that supersedes pages fails).
- Idempotency: second run migrates zero pages.
- Backup gate: archive step broken → migration refuses to run.
- Homepage notice renders the recorded archive path and clears when
  the file is gone.
- Foreign OKF v0.2 bundle imports into a project; `export-okf`
  emits a bundle a strict reader accepts (a non-conformant page fails
  the export). Import has no dedicated command by design: the format is
  native, so unpacking a bundle's concept files into a project's wiki
  directory and letting the watcher (or `reindex`) ingest them IS the
  import path. Overwriting an already-imported concept file gets its new
  version embedded the same way a brand-new file does — no manual
  `ai-memory embed` needed. Deleting one does not remove it from the index by
  default: the watcher only reconciles create/modify events, so a deleted
  concept file needs an explicit `ai-memory delete-page` unless the opt-in
  safety net below is enabled (#929).
- Retrieval regression: LongMemEval baseline re-run; no material drop.
