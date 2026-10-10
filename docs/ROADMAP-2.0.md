# ai-memory 2.0 - Plan

> Source: the September 2026 landscape research
> (`research-2026-landscape.md`) and its ranked recommendations, promoted
> to a release plan on 2026-09-01. Method: **one item at a time**, each
> designed → tested → implemented → full gate → committed before the next
> begins. 2.0 is cut only when every item and its documentation is done.
> Nothing here is released early.

## Ground rules (apply to every item)

- **Baseline before change.** Item 1 establishes retrieval-quality
  numbers; every later item re-runs them. A 2.0 feature that costs
  retrieval quality is a regression, not a feature.
- **Migrations are the product.** Existing users upgrade a living store:
  wiki pages, SQLite schema, embeddings, and config all have state.
  Every item states its migration and rollback story *in its design*,
  before code. DB changes ride refinery as always; **wiki-format changes
  get their own versioned mechanism** (see item 2) because refinery
  cannot version markdown on disk.
- **Clean-code bar.** Each item ends with a pass for duplication, dead
  code, and unnecessary surface: refactors that make the item's tests
  easier belong to the item, not to "later". No speculative
  abstractions; delete what an item obsoletes in the same commit.
- **Tests with controls.** Every guarantee gets a test that fails
  against a deliberately broken implementation (logic broken, never a
  destination redirected). Multi-session invariant 16 applies to any
  item touching scope, pages, or retrieval.
- **Docs in the same commit** as the behaviour they describe. CHANGELOG
  under `[Unreleased]` per item, with migration notes accumulated into
  `docs/MIGRATION-2.0.md` as they arise, not reconstructed at the end.

## Item 1 - Retrieval evaluation harness (LongMemEval-V2)

*Why first:* it is the regression guard for items 2-6, and the field now
audits published numbers (mempalace). We publish none until this exists.

- In-repo harness, on-demand like `writer_throughput` (`#[ignore]`d or a
  `cargo run -p ai-memory-eval` target; the `evals/` crate already
  exists as a home). Fetches/loads the LongMemEval-V2 dataset, ingests
  through the *real* store (hooks-shaped ingestion, not direct SQL),
  queries through the real retrieval stack, reports R@k / P@k per task
  category, zero-LLM and LLM-assisted modes separately.
- Deterministic where possible; provider-dependent parts clearly marked.
- **Dataset decision (verified 2026-09-01):** V2 is Apache-2.0 on
  HuggingFace (`xiaowu0162/longmemeval-v2`) with an official harness,
  but its trajectories are WebArena/ServiceNow-style *web-agent*
  histories, not coding sessions. The numbers competitors publish
  (agentmemory 0.967 R@5, mcp-memory-service 0.804/0.860) are against
  **v1** chat histories. Therefore: target **v1 (S variant) for
  cross-project comparability**, and adopt **V2's five-ability taxonomy**
  (static state recall, dynamic state tracking, workflow knowledge,
  environment gotchas, premise awareness) as the rubric for a
  coding-agent-native companion eval built from replayed real sessions
  (the agentmemory "coding-agent-life" precedent, with our own corpus).
- Deliverables: harness, baseline numbers committed to
  `docs/benchmarks/` with date + commit + hardware, docs on how to run.
- Migration: none.

## Item 2 - OKF v0.1 conformance (the 2.0 headline)

*Why:* our wiki is nearly an OKF bundle; conformance makes ai-memory the
server-grade implementation of the standard Google published, and the
knowledge portable to any OKF-aware consumer.

- **Design doc first** (`docs/okf.md`): field-by-field mapping of our
  frontmatter to OKF (`type` required; our `tier`, `kind`, `tags`,
  `entities`, TTL fields as extensions; OKF explicitly allows unknown
  fields). Decide native conformance vs export-only. Bias: native (wiki
  pages *are* OKF files), because "export" forks the truth.
- **The migration is the hard part, and it must not churn versions.**
  Renaming/adding frontmatter across every page via normal writes would
  supersede every page in every store (version explosion, embedding
  invalidation, `updated_at` stampede). Instead:
  - **proactive backup before anything is touched**: the migration's
    first step compresses the entire data dir (wiki files, SQLite DB,
    manifest) into a timestamped archive *outside* the data dir, in the
    user's home (e.g. `~/ai-memory-backup-pre-2.0-<date>.tar.gz`),
    verifies the archive is readable (entry listing + size sanity)
    before proceeding, and **aborts the migration if the backup cannot
    be written or verified**: no backup, no migration. The archive
    path is recorded in the wiki meta manifest.
  - **the HTML wiki homepage surfaces the backup**: after migration,
    `serve`'s homepage shows a notice with the archive's path, size,
    and date, plus the two exits: "everything looks right → delete the
    archive" and "something is missing → restore steps" (linking the
    restore section of `docs/MIGRATION-2.0.md`). The notice is
    dismissible and disappears once the archive is deleted.
  - a dedicated one-shot migration: rewrite frontmatter **in place**,
    same page id, same version row, body untouched; single git commit
    ("okf-migration") with a pre-migration checkpoint; reindex after.
  - a `wiki_format` marker (in the wiki meta manifest) records the
    format generation; `serve` refuses to open a newer-generation wiki
    with an older binary (mirror of the DB's schema-ahead guard).
  - rollback: the pre-migration git checkpoint plus `reindex`.
  - idempotent: re-running migrates zero pages.
- Round-trip tests: our page → OKF file → parsed back identical; foreign
  OKF bundle imports into a project; migration control test proves the
  no-churn property (same ids before/after); backup control test (break
  the archive step and the migration must refuse to run); homepage notice
  renders the recorded archive path and clears when the file is gone.
- `ai-memory export --okf <dir>` / `import --okf` for the non-native
  direction regardless, for interop with bundles outside the store.

## Item 3 - Typed relation edges

*Why:* `causes` / `fixes` / `contradicts` on the existing `links` model;
`contradicts` feeds the lint pass that already exists.

- Schema: `links.relation` (nullable TEXT, default NULL = plain link),
  an additive migration with zero rewrite of existing rows.
- Producers: wikilink syntax extension (`[[page|fixes]]` or frontmatter
  `relations:`) decided in a short design note; consolidation prompt
  gains the vocabulary (JSON-schema constrained, per invariant 7).
- Consumers: lint (contradiction findings from `contradicts` edges),
  retrieval graph stream (relation-aware weighting only if item 1 shows
  it helps; otherwise store, don't weight).
- Migration: none needed (additive). Docs: usage + graph docs.

## Item 4 - Temporal validity on entity links (bi-temporal-lite)

*Why:* "what was true in June", the one mainstream graph-camp mechanism
worth having. Builds on item 3's schema work.

- **Design doc before schema.** Reference: Zep/Graphiti paper. Scope
  deliberately small: `valid_from` / `superseded_at` on
  `entity_page_links` (and possibly typed edges), populated by
  supersession events we already emit: when a page version supersedes,
  its entity links inherit the timeline. No world-time vs
  ingestion-time split in v1 of this; ingestion-time only, documented as such.
- Retrieval: `as_of` parameter on entity queries (additive, default
  now).
- Migration: additive columns; backfill `valid_from` from the page
  version's `created_at` as a one-shot data migration inside refinery.

## Item 5 - Local embeddings (all-MiniLM class; shipped via candle, not ONNX)

*Why:* zero-config hybrid retrieval with no provider; `models/` has been
reserved for this since M9.5. Competitors ship it by default.

- `ort` crate; model NOT bundled in the binary (size); it is fetched on
  `ai-memory embed --provider local` first use into `models/`, with
  checksum pinning and an offline path (drop the file in manually).
- Coexistence is already designed: `(provider, model, dim)` is
  denormalized on `page_embeddings`, so local vectors sit beside
  provider vectors and hybrid search already ignores mismatched triples.
  Migration: none forced; `embed --force` re-embeds opt-in.
- Item 1 measures retrieval delta local-vs-provider before making local
  the default for fresh installs; existing installs keep their
  configured provider either way.
- Watch: `ort`/onnxruntime licensing + build weight on all release
  targets (incl. Windows); a `local-embeddings` cargo feature if the
  dependency is heavy. **As built: this watch item decided the implementation: candle (pure Rust) instead of ort, no native
  runtime, feature `local-embeddings` (default on). Same model, same
  use case; see `docs/local-embeddings.md`.**

## Item 6 - Cross-session abstraction ("Experience" stage)

*Why:* the frontier the survey names; the narrative layer TriMem argues
for. Most speculative, therefore last, and shaped by whatever items
1-5 taught us.

- A periodic pass (auto-improve is the host; scheduled like existing
  maintenance) that reads the last N sessions per project and stages
  rewrites of pattern/preference/architecture pages, cross-trajectory rather than per-session. Staged through `pending-writes` exactly like other
  auto-improve output: reviewable, never silent.
- Zero-LLM default preserved: pass is opt-in like consolidation.
- Success metric from item 1 plus a curated before/after page-quality
  review; if it cannot demonstrate value, it ships disabled-by-default
  with a note saying so, or not at all.

## Item 7 - `status` truthfulness audit (pre-cut, low priority)

*Why:* `status` is what a user trusts when deciding whether the system
is healthy. Six items of new state make it easy for the display to drift
from reality. Scheduled deliberately **after** items 1-6 so it audits
the final surface, not a moving one.

- Assessment first, fixes second: for every line `status` prints, trace
  it to the source of truth and answer "has this been true so far?": counts vs actual rows, FTS coverage vs actual index, embedding
  backlog vs reality, spool depth, server/bind/data-dir provenance.
- Then the inverse: what health signal exists that `status` *doesn't*
  show? 2.0 candidates from items 1-6: `wiki_format` generation and
  OKF migration state, the pre-migration backup archive (present/size),
  typed-edge and temporal-column counts, local-ONNX embedder health,
  last eval-baseline date. Also pre-existing gaps (e.g. does anything
  surface a wedged writer queue or a failed auto-improve scope?).
- Same lens over the sibling surfaces that claim health: `serve`
  startup lines, the admin console overview, and the wiki homepage.
- Tests: each status line gets a test that breaks the underlying state
  and proves the line changes (logic broken, never a destination).

## Item 8 - Documentation why/when pass (pre-cut)

*Why:* feature docs written alongside code default to "what it does";
readers deciding whether to ADOPT a feature need "why it exists",
"when to reach for it" (and when not to), and a real-world example
showing the possibility, the way `docs/local-embeddings.md` leads
with the egress/paraphrase-recall story before the mechanics.

- Sweep every user-facing doc (README sections, docs/*.md, the
  config template comments) and grade each feature's coverage: what /
  why / when / example. Fix the gaps: a short scenario ("two
  teammates on one server", "resuming on the laptop what the desktop
  left off", "asking what we knew about X before the rewrite") beats a
  flag list.
- Real examples over abstractions: pick from this project's own
  history where possible (the eval harness catching the FTS5 bug is a
  better typed-edges/`contradicts` story than an invented one).
- When-not-to guidance is part of honesty: every feature doc names the
  case where the simpler default is the right call.
- Runs after items 1-6 so it covers the final surface, alongside the
  item 7 status audit.

## Sequencing and the cut

```
1 harness → 2 OKF (+migration) → 3 typed edges → 4 temporal → 5 local-embed → 6 abstraction → 7 status audit → 8 docs why/when pass
```

Each lands on main individually gated (fmt, clippy -D warnings, full
tests, changelog guards, harness numbers where retrieval is touched), with
**fast Linux CI only per item**; the slow macOS/Windows matrix runs
once, mandatorily, on the release-candidate SHA before the cut (see
AGENTS.md "CI pacing"). 2.0 is cut when:

- all six items (or an explicitly-decided subset; item 6 may justifiably drop) are merged with docs;
- `docs/MIGRATION-2.0.md` reads as a complete upgrade guide and has been
  exercised against a copy of a real 1.x data dir, **including a full
  restore drill from the pre-migration backup archive**;
- README/ARCHITECTURE reflect 2.0 reality;
- the eval numbers are re-run and published for the final tree;
- a **pr-post-audit over the whole 2.0 range** (v1.39.0 → the
  release candidate) has run clean: every item PR re-verified against
  the final combined tree, cross-PR interactions swept, documentation
  ledger checked, with findings fixed and re-audited before anything else
  proceeds;
- the full macOS/Windows CI matrix is green on the release-candidate
  SHA (dispatched, not assumed), and **the user has reviewed the summary
  of everything done and explicitly approved the release**. 2.0 is never cut automatically.

Deploy/live-test follows the same v1.39 pipeline: exact-main gates,
hosted CI, `compose pull` (never `bin/deploy`), health + status + a
migration dry-run against a copied production data dir *before* the
production upgrade.
