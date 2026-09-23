# Design: memory aging — decay curves, tier-down, dream pass, access-weighting (2.4)

Design-of-record for a 2.4 feature set that improves how ai-memory handles
**old** memory: decay/compression/consolidation and access-weighted retention.
It is honest about what already ships, what is dormant, and what is proposed;
every claim about current behavior cites the code. It lays out three feature
buckets and a phased PR plan, and it gates every ranking-affecting or
LLM-touching change on the R2 recall-eval harness before it defaults on.

Prior art in-repo you must read before implementing any of this:
[`docs/design-hindsight-borrowings.md`](design-hindsight-borrowings.md) (the
belief-strength / `page_evidence` design and its R2 gate),
[`docs/competitive-parity.md`](competitive-parity.md) (what is dormant and why),
and the R2/R7 roadmap items in
[`docs/research-2026-landscape.md`](research-2026-landscape.md) §§R2, R7.

---

## Current state (verified against the code — do not rebuild this)

The aging machinery is further along than a first reading suggests. What exists:

### The retention formula and its inputs

`crates/ai-memory-store/src/decay.rs` computes a pure retention score:

```
retention = salience·exp(−λ·age_days)
          + σ·ln(1+access_count)·exp(−μ·days_since_access)·breadth
```

with defaults `DecayParams` (`decay.rs:35-46`): `λ=0.02` (≈35-day half-life),
`σ=0.6`, `μ=0.04`, `salience_default=1.0`, `cold_threshold=0.20`,
`hard_delete_after_days=180`. The `breadth` factor
(`retention_score_with_breadth`, `decay.rs:94-120`) is
`1 + breadth_weight·ln(1+ max(distinct_actors,1)−1)`, identity at
`breadth_weight=0.0` (the default) so enabling the per-actor table changes no
score until an operator turns the weight up — there is no eviction cliff to
migrate around. Salience is clamped to `[0.25, 2.0]` in steps of `0.25`
(`SALIENCE_MIN/MAX/STEP`, `decay.rs:126-130`); `salience_after_feedback`
(`decay.rs:141-154`) steps on `Helpful`/`NotHelpful` and drops straight to the
floor on `Stale`/`Wrong`.

Input provenance (all denormalized onto `pages`, no hot-path join except
breadth): `pages.updated_at` → `age_days`; `pages.access_count` and
`pages.last_accessed_at` (V03, `migrations/V03__decay.sql`);
`pages.salience` (V37, `migrations/V37__page_feedback.sql`); breadth via the
`page_access(page_id, actor)` table (V43, `migrations/V43__page_access_by_actor.sql`).

**Critical scope fact: `retention_score` is consumed only by the forget-sweep
and curator, never by retrieval ranking.** `reader.rs:3957-3975`
(`decay_candidates`) hands the sweep its inputs; the read/search path does not
read the score. Any proposal that wants aging to influence *ranking* is a new
behavior, not a re-wiring.

### Access reinforcement — already built (M8), with one gap

`ops::bump_access_for_pages_for_actor` (`ops.rs:2151`, inserting into
`page_access` at `ops.rs:2190`) bumps `access_count` + `last_accessed_at` +
per-actor breadth rows. It runs **async, fire-and-forget** through the
single-writer actor (`spawn_access_bump`, `server.rs:5237`, `tokio::spawn` →
`writer.bump_access_for_actor`), **throttled** to ≤1 per `(page, operator)` per
60s (`ACCESS_BUMP_COOLDOWN`, `server.rs:5133`; `select_bumpable`), and is
**FTS-exempt** (the bump never rewrites the search index). This is the
sanctioned read-path-write pattern and it already honors invariants #2/#3/#13/#16.

It is wired into **`memory_query`** (`server.rs:2463`, plus the raw-observation
fallback at `2545`) and **`memory_recent`** (`server.rs:2715`) — and nowhere
else. It is **not** wired into `memory_read_page` (`server.rs:3612`), the
related-walk it performs (`related_walk`, `server.rs:3688`, over `page_links`),
or `memory_explore` (`server.rs:4701`). **That is the gap** the user is asking
about: a page a human opens directly, or one the graph walk surfaces, earns no
reinforcement today. (Bucket C1.)

### The forget-sweep and its tiers

`crates/ai-memory-consolidate/src/sweep.rs` runs, in order: a **TTL pass**
(frontmatter `expires_at` in the past → hard-delete file+rows regardless of
tier or pin, V36); an **episodic-only decay pass** (retention `< cold_threshold`
→ remove the Markdown, turn the latest row into a decay tombstone); a
**hard-delete pass** (tombstones + supersession ancestry older than
`hard_delete_after_days`); and an opt-in **observation prune** that runs last so
a page evicted this run keeps its raw capture. Tiers are `working` / `episodic`
/ `semantic` / `procedural` (V01); **semantic and procedural never decay**
("semantic compounds"), and **pinned pages are exempt** regardless of tier.
Eviction is destructive-through-the-wiki-layer, so it goes through supersession
and git — the loser stays reachable until hard-delete.

### Dormant substrate (exists on disk, ranking-inert today)

- **Abstract embeddings** (V61, `page_abstract_embeddings`, `abstract:`
  frontmatter): a page body is separable from a short abstract; the
  `abstract_vectors` retrieval stream is **off by default**.
- **`page_evidence`** (V63, `source_kind` including `reconsolidation`) and
  **`salience`** (V37): both persisted, both **ranking-inert** — they move the
  sweep, not the results. See `competitive-parity.md` and
  `design-hindsight-borrowings.md`: belief-strength was designed and deferred
  behind R2, not shipped into rank.

### Hook points for a consolidation ("dream") pass

- `experience.rs` (`apply_eval_gate` at `:195` → `apply_batch`): the existing
  cross-session LLM pass that turns observations into pages under the eval gate.
- `curator.rs` (`:147`): already emits a **`cold_episodic`** finding list —
  literally a prediction of what the sweep would evict, i.e. a ready-made work
  queue for a compression/merge pass.
- `Wiki::write_page` / `Wiki::apply_batch`: the only sanctioned wiki mutation
  path (supersession, admission, attribution, git, index) — invariant.
- `auto_improve.rs`: the approval-gated learning loop.

### Invariants that constrain everything below

Any read-path write stays on the sanctioned async/throttled/FTS-exempt path
(invariant #2 single-writer no-N+1, #3 no index-after-return). The system must
keep working with **no LLM provider** (#13). Multi-session/multi-user
correctness (#16) means access reinforcement, merges, and supersession must not
become per-operator read filters — `page_access` is per-actor for *breadth*, not
for *visibility*.

---

## Competitor grounding (mechanisms, cited to our research docs)

Concise map of how the field handles old memory. Numbers marked *(self-reported)*
are vendor claims, not our measurements. Sources:
[`research-2026-landscape.md`](research-2026-landscape.md),
[`competitive-parity.md`](competitive-parity.md),
[`research-hindsight.md`](research-hindsight.md),
[`research-agentmemory.md`](research-agentmemory.md).

| Tool | Aging / decay | Compression | Dedup / merge | Reinforcement | Belief / contradiction | Dream trigger |
|---|---|---|---|---|---|---|
| **mcp-memory-service** | type-dependent TTL 365/180/90/30d; multi-horizon schedule | extractive 500-char + regex | DBSCAN, adaptive-eps | access + connection + quality boosts | confidence = support − contradict − decay; contradiction via 0.4–0.75 similarity band | scheduled |
| **Honcho Dreamer** | — | — | ≥2-evidence induction | — | surprisal-first ordering | event + idle, cancel-on-activity |
| **Supermemory** | TTL expiry | — | Updates / Extends / Derives supersession | — | supersession types | dream-when-idle |
| **Letta sleep-time** | `forgetAfter` | idle shared-block rewrite | block rewrite | — | — | idle |
| **Hindsight** | belief decay | — | extend/merge beliefs | proof-count strengthens | belief-strength (proof count); **failure = belief entrenchment** | — |
| **OpenViking** | L0/L1/L2 progressive tiers | tier-down | create / merge / skip | — | — | — |
| **agentmemory** | exp decay (validates ours) | **zero-LLM compression default** | — | access boost | — | — |
| **A-MEM / Mem0** | none (no decay) | — | link-evolution / consolidate-on-write | — | — | on-write |

Takeaways that shape the design: (1) type-dependent retention is the near-universal
next step past a single λ (mcp-memory-service, Letta, OpenViking); (2) extractive,
zero-LLM compression is a shipped default elsewhere (agentmemory, mcp-memory-service)
— we can do it without a provider; (3) DBSCAN with adaptive eps is the standard
cold-cluster dedup; (4) the dream-pass consensus is **event+idle with
cancel-on-activity** and **surprisal-first** ordering; (5) the documented failure
mode of belief-strength is **entrenchment** (raw proof-count wins) — Hindsight's own
warning, which our B1 must design against.

---

## The design — three buckets

Each item states **what / why / mechanism / schema delta / invariants / R2 bar /
failure modes**. Latest migration on disk is V64; new migrations start at **V65**.

### Bucket A — zero-LLM decay / compression (invariant #13 intact)

Everything in A works with no provider and does not require ranking changes.

**A1. Type/tier-dependent retention curves.**
- *What:* replace the single `λ` with a per-`(tier, kind)` half-life lookup,
  config-driven, mirroring mcp-memory-service's 365/180/90/30 shape.
- *Why:* one λ over-keeps working-tier scratch and under-keeps important episodic
  history; the field has moved past a global constant.
- *Mechanism:* extend `DecayParams` with an optional `half_lives:
  HashMap<Tier, f64>` (and optionally by kind) resolved once at `Config::load()`;
  `retention_score_*` takes the per-page λ from the lookup, falling back to the
  scalar `lambda`. **No new column** — tier already lives on every page (V01);
  kind is already in frontmatter. Default map = current behavior (all tiers λ=0.02),
  so byte-identical scores until an operator sets curves.
- *Invariants:* pure-function change; #13 intact; identity default preserves #16
  (no eviction cliff).
- *R2 bar:* none to *ship* (defaults unchanged), but any non-default curve an
  operator wants blessed as a recommended preset must show no recall regression on R2.
- *Failure modes:* mis-tuned curve evicts load-bearing episodic history →
  mitigated by the identity default and by C1/C2 reinforcement raising the access term.

**A2. Extractive tier-down of cold episodic pages (instead of eviction).**
- *What:* when an episodic page goes cold, **compact** it rather than tombstone it:
  keep the V61 abstract (L0), keep a first-paragraph/frontmatter summary (L1), keep
  a regex-mined **keep-token** set (identifiers, file paths, URLs, code spans,
  error codes), and drop the prose body.
- *Why:* the body is the expensive, low-signal part; agentmemory and
  mcp-memory-service ship extractive compression with no LLM. Tier-down beats
  eviction because the durable facts survive.
- *Mechanism:* a new sweep pass, before the decay-eviction pass, that rewrites the
  page through `Wiki::write_page`/`apply_batch` (supersession + git preserve the
  original — **reversible**: the full body stays in the version chain and git
  history). Regex keep-token miner lives beside the sanitizer's patterns. Needs a
  **`tier_state`/`compacted` marker** (frontmatter flag + a `pages` column, V65) so
  the sweep and `curator.rs` distinguish a *deliberately short* page from a *cold*
  one and never re-compact or misread it as cold.
- *Schema delta:* V65 `pages.compacted_at` (nullable) + frontmatter mirror.
- *Invariants:* mutation through the wiki layer only (#16 supersession keeps the
  loser reachable); zero-LLM (#13).
- *R2 bar:* compaction must not drop recall on R2 vs. the pre-compaction corpus
  (the keep-token set is the hypothesis: facts survive, prose doesn't matter).
- *Failure modes:* keep-token miner drops a load-bearing sentence → the original is
  one supersession-restore away; `restore-page` covers it. Re-compaction loop →
  the `compacted_at` marker blocks it.

**A3. Cold-cluster dedup (DBSCAN, adaptive eps).**
- *What:* cluster near-duplicate cold pages by embedding and collapse each cluster
  to one page via supersession/merge-note.
- *Why:* long-lived projects accumulate near-dup episodic pages; DBSCAN with
  adaptive k-distance eps is the field-standard, and it avoids O(N²) all-pairs.
- *Mechanism:* run over the `cold_episodic` candidate set from `curator.rs`
  (bounded, not the whole corpus). Adaptive eps from the k-distance elbow. Collapse
  = write one survivor, supersede the rest with a merge note pointing at the
  survivor. Zero-LLM: the survivor is the highest-retention member, body chosen by
  A2's extractive rules; the *LLM* rewrite of a cluster is B2, opt-in.
- *Schema delta:* none beyond A2's marker; merge provenance goes in `page_evidence`
  (`source_kind`) and supersession.
- *Invariants:* #2 (cluster on the already-materialized cold set, one batch write);
  #16 (supersede, never delete).
- *R2 bar:* dedup must not drop recall (a collapsed duplicate should still be found
  via the survivor).
- *Failure modes:* over-eager clustering merges distinct pages → conservative eps +
  survivors keep both bodies' keep-tokens; reversible via supersession.

**A4. Entropy / boilerplate pre-filter before consolidation.**
- *What:* drop low-information observations/pages (near-empty, boilerplate, high
  repetition) before they reach a consolidation or dedup pass.
- *Why:* cheap noise reduction that improves every downstream pass; complements A3.
- *Mechanism:* a Shannon-entropy + boilerplate-pattern gate in the consolidate
  pipeline, before `experience.rs`. Pure function, unit-testable.
- *Invariants:* #13; no schema.
- *R2 bar:* no recall regression (it should only remove noise).
- *Failure modes:* filters a terse-but-important note → tune threshold low; keep it
  advisory (skip-from-consolidation, not delete).

**A5. Zero-LLM `contradicts` edges via similarity band.**
- *What:* detect likely contradictions between pages in the 0.4–0.75 cosine band
  (mcp-memory-service's heuristic) and record a `contradicts` edge resolved by
  timestamp (newer wins), feeding the existing `memory_lint`.
- *Why:* surfaces stale/conflicting knowledge without a provider; lint already
  exists to consume it.
- *Mechanism:* pairwise within cold clusters (bounded); emit an edge + a lint
  finding. Timestamp resolution only *flags*, never auto-deletes.
- *Schema delta:* reuse `page_evidence`/edge tables; add a `contradicts` edge kind
  if not present (V66 if needed).
- *Invariants:* #13; lint stays advisory (#16 — a contradiction is not a delete).
- *R2 bar:* none for detection; if the edge later feeds *rank* (per
  `design-hindsight-borrowings.md` §4) that step is R2-gated separately.
- *Failure modes:* false-positive band → advisory-only output; the operator/lint
  decides.

### Bucket B — LLM "dream" pass (opt-in, OFF by default, R2-gated)

All of B is off by default and touches either ranking or LLM; each gates on R2.

**B1. Activate belief-strength (the dormant `page_evidence` + `salience`).**
- *What:* turn `page_evidence` proof-count and `salience` into a **clamped
  authority factor** in ranking (evidence strengthens / weakens / extends a page).
- *Why:* the substrate exists and is inert (`competitive-parity.md`); Hindsight
  shows proof-count is a real signal.
- *Mechanism:* exactly the design in
  [`design-hindsight-borrowings.md`](design-hindsight-borrowings.md) §3 — a
  bounded multiplier weighted by **distinct actors and recency, not raw count**,
  with a confidence cap. This is the anti-entrenchment guard (Hindsight's
  documented failure mode).
- *Schema delta:* none (V37/V63 exist); the change is in the ranker.
- *Invariants:* ranking change → **must not** default on without an R2 number;
  breadth weighting keeps #16 (a team's page outranks one loud operator's).
- *R2 bar:* prove a triple/QA delta on R2 before default; ships behind a config
  flag until then.
- *Failure modes:* **belief entrenchment** — a wrong page with many proofs pins
  itself → distinct-actor + recency weighting and the confidence cap; `Stale`/`Wrong`
  feedback still floors salience.

**B2. Cross-session LLM merge/rewrite of cold clusters.**
- *What:* take A3's clusters and have the LLM rewrite each into one coherent page.
- *Why:* extractive collapse (A3) keeps facts but not prose coherence; the LLM
  earns its keep on genuinely mergeable clusters.
- *Mechanism:* route through `experience.rs` → `apply_eval_gate` → `apply_batch`
  (the existing, gated pass). **Originals kept reachable** via supersession;
  `dry_run` first. JSON-schema structured output only (#7).
- *Invariants:* #7 (structured output), #16 (never delete a source), #13 (only
  runs when a provider is configured; the zero-LLM path is A3).
- *R2 bar:* merged corpus must beat the pre-merge corpus on R2 before default-on.
- *Failure modes:* **hallucinated merge** — invents a fact not in any source →
  never delete the source, `dry_run`, R2 gate, and `page_evidence` records which
  sources fed the merge.

**B3. Event + idle scheduling with cancel-on-activity.**
- *What:* schedule the dream pass on idle, triggered by accumulated events, and
  **cancel it the moment the operator becomes active** (Honcho/Supermemory shape).
- *Why:* consolidation is expensive and must never contend with live work.
- *Mechanism:* an idle detector over the existing client-activity signal
  (`CLIENT_ACTIVITY_FLUSH`, `server.rs:843`); a debounced trigger; a cancellation
  token dropped on new activity. No unbounded fan-out (#5 spirit).
- *Invariants:* #2 (all writes via the actor), #5 (bounded, cancellable).
- *R2 bar:* scheduling is mechanism, not quality; it inherits B1/B2's gate.
- *Failure modes:* **silent-window bug** — a pass runs and quietly corrupts →
  every run emits an observable `SweepReport`-style outcome (pages touched, merged,
  superseded) so a bad run is visible, not silent.

**B4. Surprisal-first ordering.**
- *What:* order the dream pass's work queue by surprisal (novelty) first.
- *Why:* Honcho's finding — the most informative consolidation is on the most
  surprising material.
- *Mechanism:* proxy surprisal = embedding distance to the nearest existing page
  (already have embeddings); order the `cold_episodic`/cluster queue by it.
- *Invariants:* ordering only; no schema.
- *R2 bar:* inherits B2's gate.
- *Failure modes:* proxy diverges from true surprisal → it only *orders* bounded
  work, worst case is suboptimal ordering, not wrong output.

### Bucket C — access-weighted retention (the user's specific ask; mostly built)

**C1. Close the read-path reinforcement gap.**
- *What:* wire `spawn_access_bump` into `memory_read_page` (`server.rs:3612`), the
  related-walk (`related_walk`, `server.rs:3688`), and `memory_explore`
  (`server.rs:4701`) — the three read paths that reinforce nothing today.
- *Why:* a page a human opens directly, or one the graph surfaces, is *used* and
  should resist decay exactly as a search hit does.
- *Mechanism:* reuse the exact sanctioned path — `spawn_access_bump(ids, actor)`,
  async, throttled by `ACCESS_BUMP_COOLDOWN`, FTS-exempt. For the related-walk,
  bump the walked pages (not just the seed), keyed per operator.
- *Schema delta:* none (V03/V43 exist).
- *Invariants:* #2/#3 (already honored by the pattern), #13 (no LLM), #16
  (per-actor breadth is attribution, not a read filter).
- *R2 bar:* none — this is reinforcement input, not a ranking change; but a recall
  spot-check that reinforced pages survive a sweep they'd otherwise fail.
- *Failure modes:* read-amplification (a walk of 50 pages bumps 50) → the 60s
  per-(page,operator) throttle already bounds it; cap walk-bump breadth if needed.

**C2. Connection / graph-degree boost.**
- *What:* let a well-linked page (high `page_links` degree) resist decay.
- *Why:* mcp-memory-service's "connection boost"; a hub page is load-bearing even
  if not directly hit often.
- *Mechanism:* fold a bounded degree term into the retention formula (a second
  breadth-like factor), computed from `page_links` at sweep time (not on the hot
  read path — sweep already does a batch pass).
- *Schema delta:* none (`page_links` exists); degree computed in the sweep.
- *Invariants:* #2 (sweep-time batch, no hot-path N+1); default weight 0 →
  identity, no eviction cliff (#16).
- *R2 bar:* if degree ever feeds *rank*, R2-gate it; for retention-only it needs a
  no-regression check on the sweep's eviction set.
- *Failure modes:* popularity/hub bias entrenches a stale hub → keep the weight
  small and bounded; `Stale` feedback still floors it.

**C3. Guards (keep these separate axes).**
- Protected **floor** for pinned/critical pages so popularity bias can't sink them
  (already: pinned is sweep-exempt).
- Keep **log-compression** on access (`ln(1+access_count)`) so a runaway reader
  can't dominate.
- Keep **access→retention** and **evidence→confidence** as **separate axes**
  (C vs B1): reinforcement says "still used", belief-strength says "still true".
  Conflating them re-creates the entrenchment failure.
- Access **must not block TTL cleanup**: an `expires_at` page is deleted no matter
  how hot (current TTL-pass ordering already enforces this).

---

## Schema deltas summary

| Migration | Purpose | Bucket |
|---|---|---|
| V65 | `pages.compacted_at` (nullable) + frontmatter `compacted` marker | A2 (and read by A3) |
| V66 *(only if needed)* | `contradicts` edge kind, if not already expressible | A5 |
| — | none | A1, A3-merge-provenance (reuse `page_evidence`), A4, B1-B4 (reuse V37/V61/V63), C1-C3 (reuse V03/V43/`page_links`) |

Most of the design ships **without new columns** — the substrate (V37 salience,
V43 per-actor access, V61 abstracts, V63 evidence, `page_links`) already exists
and is under-used. The only firm new column is A2's compaction marker.

---

## Phased PR plan (release/2.4; each TDD, `### Added` CHANGELOG, R2 delta where relevant)

Ordering is cheapest-and-safest first. Each PR that touches LLM or changes ranking
ships **opt-in / off-by-default** and does not default on until the **R2 harness**
(now built, #771/#772; `crates/ai-memory-consolidate/tests/recall_eval.rs`) proves
a triple/QA delta.

**Phase 1 — cheap, zero-LLM, safe (no ranking change):**
- **PR: C1 access reinforcement on all read paths** — wire `spawn_access_bump`
  into `memory_read_page`, the related-walk, and `memory_explore` on the existing
  throttled async single-writer path. Tests: reinforced page survives a sweep it'd
  otherwise fail; throttle honored; multi-actor breadth recorded.
- **PR: A1 type/tier retention curves** — config-driven per-`(tier,kind)`
  half-lives, identity default. Tests: default map = byte-identical scores;
  table-driven per-tier eviction.

**Phase 2 — zero-LLM depth:**
- **PR: A2 extractive tier-down** (+ V65 marker) — compact cold episodic instead
  of evict; reversibility test (restore from supersession); no re-compaction.
- **PR: A3 + A4 cold-cluster dedup + entropy pre-filter** — DBSCAN adaptive-eps
  over the `cold_episodic` set; supersede-not-delete; entropy gate. Tests:
  no O(N²), conservative-eps, recall preserved via survivor.
- **PR: A5 zero-LLM contradiction edges** — 0.4–0.75 band → `contradicts` edge →
  `memory_lint` finding, timestamp resolution advisory only.

**Phase 3 — LLM dream pass (opt-in, R2-proven):**
- **PR: B1 belief-strength activation** — clamped authority factor
  (distinct-actor + recency weighted, confidence cap), per
  `design-hindsight-borrowings.md` §3; behind a flag until an R2 number.
- **PR: B2 + B3 + B4 dream pass** — cold-cluster LLM rewrite through
  `experience.rs`/`apply_eval_gate`/`apply_batch` (`dry_run`, sources kept
  reachable) + event/idle scheduling with cancel-on-activity + surprisal-first
  ordering. Off by default; each run emits an observable outcome report.

Every ranking-affecting or LLM-touching PR (all of Phase 3, and any non-default
A1 curve or A5-edge-in-rank) lands its **R2 delta in the CHANGELOG/PR body** and
defaults on only after R2 shows no regression (recall) and a positive
triple/QA delta where it claims quality. See R2 in
[`research-2026-landscape.md`](research-2026-landscape.md) §R2 and the
progressive-disclosure work in §R7 (which A2's L0/L1/L2 tier-down feeds).

---

## Migration & upgrade safety (existing stores must not break or lose data)

An upgrade to a 2.4 that ships any of this must open a large, already-migrated
store cleanly, keep every index consistent, and lose nothing — the bar the
V62/#776 rollout was held to. This section makes the guarantees explicit, per
phase, and encodes the lessons from that incident.

### 1. Migrations are additive DDL only; backfills are inert / idempotent / resumable / WAL-bounded

New migrations start at **V65** and are additive. A2's `compacted_at` is an
`ADD COLUMN` (nullable) — instant, no table rewrite, no lock on a large store.
No shipped migration's SQL is ever edited in place: doing so trips refinery's
divergence check, which is exactly why `migrations.rs:33-34` sets
`set_abort_divergent(false)` (a hash mismatch on an already-applied migration is
tolerated) while keeping `set_abort_missing(true)` (a store *ahead* of the binary
fails closed). Any change to a shipped migration must preserve that
divergence-tolerance handling.

**Any data backfill runs on the boot path, not inside the migration
transaction** — chunked, resumable, and WAL-checkpointed, mirroring the #776
precedent for V62: `backfill_page_windows` / `backfill_page_windows_in_batches`
(`ops.rs:861`, `:867`), which commits in bounded batches
(`PAGE_WINDOW_BACKFILL_BATCH`, `ops.rs:824`) and checkpoints the WAL after each
so it never accumulates unbounded WAL on a huge store, and resumes from a cursor
if interrupted (`ops.rs:830-839`). A backfill is a **fast no-op** on a store that
doesn't need it (no rows match the predicate). A2 is the only phase here that
touches page bodies, and it does so lazily through the sweep — there is no
one-shot body backfill; the marker column is populated as the sweep compacts, not
in a boot migration.

### 2. No behavior change surprises an existing user on upgrade

- **A1 type/tier retention curves — defaults reproduce current behavior.** The
  default `half_lives` map is a single ~35-day curve for every tier, byte-identical
  to today's scalar `λ=0.02` (`decay.rs:38`). An upgrade therefore does **not**
  mass-evict episodic memory on the first post-upgrade forget-sweep. More
  aggressive per-tier curves are strictly opt-in via `[decay]` config. (Episodic
  eviction is already the recoverable cold→tombstone→180-day-grace path, and
  pinned/semantic/procedural are exempt — but the explicit rule stands regardless:
  no upgrade mass-evicts.)
- **A2 extractive tier-down — reversible and non-destructive.** The full body
  stays in git history and in the supersession chain, so the pre-compaction
  version is reachable (invariant #16, loser stays reachable; `restore-page`
  recovers it). Only episodic pages compact, never pinned/semantic/procedural. The
  `compacted_at` marker (V65) distinguishes a deliberately-short page from a cold
  one so the sweep never double-processes or re-compacts. Tier-down is opt-in/gated.
- **A3 cold-cluster dedup + A5 contradiction edges — annotate, never destroy.**
  A3 collapses via supersession (the merged-away duplicates stay reachable), never
  a hard delete of a source; A5 only writes an edge and a lint finding
  (timestamp resolution is advisory). Both use conservative thresholds and are
  opt-in.
- **B1 belief-strength → ranking — off by default.** This is the one bucket that
  changes *retrieval ranking*, so it is OFF by default and gated on an R2 number
  before it may default on. An existing user sees no ranking change until they opt in.
- **B2 dream rewrite/merge — never deletes a source.** Originals live in git and
  the supersession chain; `dry_run` first; R2-gated; opt-in LLM. The zero-LLM
  default path (#13) is untouched — a provider-less store never runs B2.
- **C1 access reinforcement on more read paths — strictly non-destructive.** It
  only raises retention scores (keeps memory a little longer); it can never lower a
  score or delete anything, and it still cannot block TTL cleanup (the TTL pass
  runs regardless of how hot a page is). It stays on the throttled/async
  single-writer path, so there is no hot-path or data-integrity risk.

### 3. Recovery & proof

`serve` startup already writes a **pre-migration safety archive** of the whole
data directory before applying migrations (#633; `ai-memory-wiki/src/backup.rs`,
`create_pre_migration_backup` at `:125`, receipt
`pre-migration-backup.json`, `:21`), and the wiki error path points a stuck user
at restoring it (`ai-memory-wiki/src/error.rs:51`). Beyond that, every
state-touching step keeps its source **diffable in git and reachable via
supersession** until the existing 180-day tombstone grace + hard-delete
(`DecayParams::hard_delete_after_days`, `decay.rs:43`).

**Proof bar before any release carrying these features** (the #776 / 2.3.2
model): the full cross-platform CI matrix green on the exact RC SHA, **and** a
live deploy + verify against the real marvin store — a large, already-migrated
store opening cleanly, indexes consistent, no data loss — exactly as V62/#776 was
validated. A green unit suite is not sufficient for anything that mutates stored
content.

### 4. Downgrade note

Additive columns plus `set_abort_missing(true)` (`migrations.rs:34`) mean an older
binary opening a newer store **fails closed** with the actionable
`StoreError::DataSchemaAhead` (`error.rs:36`, raised at `migrations.rs:55`) — it
names the applied-vs-supported versions and refuses to open, rather than silently
corrupting a store it doesn't understand. That guarantee must be preserved by
every new migration.

## R2 acceptance

R2 is the reproducible LongMemEval-V2 recall-eval harness
(`crates/ai-memory-consolidate/tests/recall_eval.rs`, built in #771/#772). It is
the gate, not a formality:

- **No-regression gate (mandatory for anything that changes stored content or
  ranking):** A2 compaction, A3 dedup, and A4 filtering must not lower recall vs.
  the pre-change corpus. If keep-tokens are the right hypothesis, recall holds.
- **Quality gate (mandatory before default-on for B1/B2):** must show a positive
  triple-accuracy / QA delta, not just "no worse". Ships behind a flag until the
  number exists.
- **Ordering/scheduling (B3/B4):** no independent R2 bar — they inherit B2's gate;
  correctness is proven by unit/integration tests (cancel-on-activity, bounded
  work, observable outcome per run).
- **Access reinforcement (C1/C2):** reinforcement is an *input* to the sweep, not a
  ranking change, so it has no R2 quality bar; it carries a targeted regression
  test (a used page survives a sweep it would otherwise fail) instead.

Honest bottom line: buckets A and C are shippable now on our zero-LLM turf and
close a real gap (the read-path reinforcement hole in C1, single-λ decay in A1).
Bucket B activates substrate that has been deliberately dormant behind exactly
this harness — it does not default on until R2 says it earns its place.
