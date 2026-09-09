# Design: Page-grain ingestion windows + version-filtered `as_of` (issue #656)

*Status: implemented on the `release/2.2` line (2.2.0) — V62 page-grain
windows, version-filtered `as_of` FTS fused via the default path's RRF,
`docs/temporal.md` updated. Scope stayed Phase A only; Phase B remains
deferred (§7). Open question 1 resolved by the v2.1.0 cut (new features
ride 2.2). Open question 2 resolved for the materialized pair
(`valid_from` + `valid_to`): the symmetry with the V56 link windows was
worth the duplicated column. Open question 3 resolved for RRF merge
rather than bare fallback. One deliberate deviation from §3: the FTS leg
evaluates TTL expiry at T while the entity leg ignores it (see
`docs/temporal.md` "one asymmetry"); changing the entity leg's rule
would alter existing behaviour and is out of scope.*

## 1. The problem, and why entity-link windows are not enough

`docs/temporal.md` (bi-temporal-lite, 2.0 item 4) answers "what did we know
about X as of June" through validity windows on `entity_page_links` (V56,
repaired by V58). Issue #656 shows three gaps that live at page grain, not
link grain:

1. **Paraphrase blindness in audit mode.** `as_of` is entity-only: the
   question must contain the exact retired entity *and* the page must carry
   the right `entities:`/`tags:` frontmatter. "Which database were we on
   when incident X happened?" misses when phrased differently, because
   FTS/vector/graph are skipped in `as_of` mode.
2. **The page timeline is implicit, not queryable as a window.** Page
   versions form a chain (`supersedes` + `created_at`), but no predicate
   expresses "versions alive at T" without re-deriving it per query.
3. **V58-class risk has no page-grain equivalent.** V58 repaired merge paths
   that left link windows open, letting `as_of` resurrect retired knowledge.
   Whatever page-grain shape we choose must close transactionally on every
   retire path, not just the supersede path.

## 2. Vocabulary: this is ingestion time, not valid time

The issue title frames the feature as valid-time ("true in the world").
Phase A's `valid_from`/`valid_to` are **ingestion time**: the version's own
`created_at` and the superseding version's `created_at` — the *same*
dimension the entity-link windows already use, just materialized at page
grain. "Valid-time" stays reserved for the world-time split (Phase B, §7).

Pinned terms for this doc and the implementation PR:

| Term | Meaning | Where |
|---|---|---|
| **ingestion window (page grain)** | `valid_from` = the version's `created_at`; `valid_to` = the superseding version's `created_at`, else the retirement instant (§3), else `NULL` while the version is latest-live | `pages` (new) |
| **ingestion window (link grain)** | existing `valid_from` / `superseded_at` on `entity_page_links` (V56/V58) | unchanged |
| **valid time / world time** | when a fact was true in the world (Graphiti's `valid_at`/`invalid_at`) | Phase B only, deferred |

### Column names: `valid_from` / `valid_to`

- `valid_from` mirrors `entity_page_links.valid_from` deliberately: same
  dimension, same predicate shape, same tests. The "valid" here reads as
  "the version the store treated as current", never as world truth.
- The window end is **`valid_to`, not `superseded_at`**, for a load-bearing
  reason: `pages.superseded_at` already exists with a different meaning —
  the V03 decay-tombstone eviction marker, written exactly when
  `supersedes IS NULL` (sweep eviction, not supersession). Reusing that
  name would conflate "evicted by the forget sweep" with "replaced by a
  newer version". `valid_to` also covers ends that have no superseding
  version at all (retirement without successor, below), where
  "superseded_at" would be a lie.
- `valid_from` duplicates `created_at` by construction. That duplication is
  accepted (see §4): it keeps the window self-describing per row and the
  `as_of` predicate symmetric with the link-grain one.

## 3. The window end: supersede, retire, or open

A version's window closes at exactly one of:

1. **Supersede** — a new version of the same path is written. `valid_to` =
   the new version's `created_at`, closed in the same transaction as the
   `is_latest` flip (mirrors the link-window close in `upsert_page_in_tx`).
2. **Retire without successor** — decay tombstone, purge-regenerate,
   graveyard merge (the V58 class). `valid_to` = the retirement instant
   (recorded explicitly on both grains), closed in the same
   transaction as the retire. New retire paths must close both grains;
   V58's audit is the checklist.
3. **Open** — `valid_to IS NULL` while the version is the latest live one.

Deletion stays deletion: purged pages cascade away and the timeline does
not survive a purge, exactly as `docs/temporal.md` already states for
links. Expiry/TTL is orthogonal: the FTS leg applies `not_expired` at T;
the entity leg preserves its existing ignore-expiry behavior.

## 4. Materialized columns vs. a view/join

The window is derivable today: `valid_from = p.created_at`,
`valid_to = (SELECT s.created_at FROM pages s WHERE s.supersedes = p.id)`
over the already-indexed `pages(supersedes)`, with a `COALESCE(updated_at)`
for successor-less retirements. No migration required. Weighed against
stored nullable columns + backfill:

**View/join.**
- Pro: zero migration — no refinery step, no backup gate, no backfill, no
  new V58-class risk; a window can never be left open by construction.
- Con: every `as_of` FTS query pays a self-join against `pages_fts`
  candidates on the read path, with an FTS5+join plan to defend in review;
  the retirement-instant rule does not disappear, it relocates into the
  view definition (the V58 logic must still be specified somewhere); no
  covering index for the range predicate.

**Materialized (`ALTER TABLE pages ADD COLUMN valid_from/valid_to` +
index + one-shot backfill).**
- Pro: symmetric with the V56 link windows — same predicate shape
  (`valid_from <= T AND (valid_to IS NULL OR valid_to > T)`), same
  close-in-the-same-transaction discipline, same test shapes; a sargable
  range predicate with a covering index for the `as_of` FTS join; each
  row's window is auditable without joining.
- Con: it is a backup-gated store migration (§5) — a bigger step than the
  issue frames; it introduces a new leave-open risk, held down only by
  transactional closes plus tests with controls.

**Recommendation: materialized.** The deciding factors are predicate
symmetry across the two grains (one window semantic, two tables) and
keeping the `as_of` FTS join sargable. The migration cost is real but
bounded and precedented (V56 pattern + the §5 backup gate). If review
prefers zero-migration, the view is a legitimate fallback with identical
query semantics — the rest of this doc is unchanged either way.

No FTS rebuild is needed in either case: the `pages_fts_*` triggers index
every version row already (including superseded ones); version-filtered
FTS is a join-predicate change, not an index change.

## 5. Migration: backup-gated, and stated plainly

Phase A is additive (nullable columns + index + backfill, no removed or
renamed surface, no changed defaults) — but additive is not free:

- **Refinery migration**, one step: add the nullable columns, backfill
  (`valid_from = created_at`; `valid_to` from the superseding version's
  `created_at` via `pages.supersedes`, `NULL` for latest;
  successor-less retired rows closed at the decay marker, else the
  earliest existing entity-link close, else `updated_at` as an
  approximation when no retirement timestamp survived),
  create the `(valid_from, valid_to)` index. Idempotent re-run migrates
  zero rows.
- **Backup gate.** The migration refuses to run without a verified
  pre-migration safety archive, per the current store-migration policy
  (the #633 ordering fix: archive *before* the schema migration, abort
  when the archive cannot be written or verified). This is the step the
  issue understates, stated here so the implementation PR is sized
  honestly.
- **Rollback.** Restore the verified pre-migration snapshot from #633
  before starting the older binary. Refinery rejects newer applied
  schema versions; an older binary cannot read the migrated store.
  Forward re-runs are no-ops.

## 6. `as_of` gains version-filtered FTS: the explicit reversal

`docs/temporal.md` deliberately runs `as_of` as a time-travel *entity*
lookup and skips FTS/vector/graph: "mixing current relevance with
historical validity answers neither honestly" (`server.rs` returns early
with `streams_active: ["entity"]`, no raw-observation fallback).

Version-filtered FTS at T is a genuine answer to that objection, and this
section is the explicit argument the maintainer asked for rather than a
silent scope slip:

- The original objection is about mixing **present-tense relevance** with
  **past validity**. Constraining the FTS corpus to versions whose
  ingestion window contains T makes the retrieved content historical.
  Ranking is not a historical snapshot: FTS5 BM25 still uses the current
  index's statistics, including later versions and other projects.
  Later writes can change ordering and the limited result set at a
  fixed T. Phase A retrieves historical versions with current-index
  relevance; it does not reproduce what search would have ranked then.
- Concretely, `as_of` runs two streams at T and merges via the existing
  RRF: the unchanged entity-window lookup plus FTS over page versions
  with `valid_from <= T AND (valid_to IS NULL OR valid_to > T)` (and the
  usual not-expired predicate). `explain` gains the `fts` stream;
  `streams_active` reports both.
- **Still out of `as_of`:** vector (embeddings are present-tense artifacts
  of the current text; version-scoped vectors are a separate project),
  graph neighbours (latest-scoped by construction), and the
  raw-observation fallback (unchanged: off in `as_of`).
- **Default path unchanged:** without `as_of`, retrieval searches latest
  versions exactly as today. The `as_of`+`global`/`scopes` refusal and
  the invalid-instant error stay as they are.

This directly fixes gap 1 in §1: entity-less pages and paraphrased
questions become answerable in audit mode, while the entity stream keeps
its precision for entity-phrased questions.

## 7. Explicitly out of Phase A

- **Phase B (world-time frontmatter `true_from`/`true_until` +
  confidence)** stays deferred per the roadmap: dating facts by LLM
  without a confidence fallback is untrustworthy, and the zero-LLM path
  could never populate it. `NULL = unknown, fall back to ingestion time`
  remains a future design, not this one.
- **Default-path rank changes.** The issue proposes capping superseded /
  validity-expired pages in non-`as_of` ranking — but the default path
  only indexes `is_latest = 1` rows, so superseded versions never rank
  today; the "freshness never touches ranking" complaint is really about
  *stale-but-latest* pages, a different problem. Tag-carried staleness
  already has an authority mechanism (`superseded → 0.65`,
  `historical → 0.80` in `PageAuthority`). Phase A makes **no default-path
  rank change**; any freshness signal is a separate proposal with eval
  numbers behind it (roadmap ground rule: baseline before change).
- Typed `contradicts` edges entering rank: natural follow-up once page
  windows exist, not this PR.

## 8. Acceptance for the implementation PR (Phase A only)

- Table-driven test `postgres → sqlite → postgres` (+ revert): `as_of`
  inside the middle window returns that version via the entity stream
  **and** via version-filtered FTS; default query unchanged.
- Backfill idempotence; retire-without-successor closes both grains in
  the same transaction (V58 checklist paths); purge still destroys.
- `docs/temporal.md` updated (the §6 reversal + the pinned vocabulary),
  CHANGELOG under `[Unreleased]` (`### Added`, `(#656)`), migration note.
- Full local gate per AGENTS.md; fast Linux CI per merge, full matrix on
  the release-candidate SHA before any tag, per the CI pacing rule.

## 9. Open questions (for maintainer input before coding)

1. The v2.1.0 cut landed after your "targeting 2.1" note: does the
   implementation PR still ride the `release/2.1` line (2.1.x additive)
   or wait for the next minor?
2. Materialized pair (`valid_from` + `valid_to`) vs. minimal single
   `valid_to` with `valid_from` read as `created_at` — is the symmetry
   worth the duplicated column?
3. In `as_of` mode, should FTS and entity merge via RRF, or should FTS
   act strictly as fallback when the entity stream misses?
