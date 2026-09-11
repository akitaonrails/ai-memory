# Design / implementation plan: borrowings from the Hindsight study

*Status: proposed (design only, no code). Targets the `release/2.2` line
(2.2.x — every item here is additive). Derived from
[`research-hindsight.md`](research-hindsight.md) §11 ("Strengths worth
borrowing") and the landscape recommendations R3/R5 in
[`research-2026-landscape.md`](research-2026-landscape.md). Extends, and is
consistent with, [`design-page-ingestion-windows.md`](design-page-ingestion-windows.md)
(which already flagged two of these as follow-ups) and `docs/temporal.md`.
Nothing here implies re-architecture; the whole point of the Hindsight study
was that a paper-backed competitor converged on our substrate. This plan turns
the four borrowable ideas into bounded, independently-shippable phases.*

## 0. First, what already exists (so we do not rebuild it)

The Hindsight analysis named five borrowables. Grounding them against the code
on `release/2.2`, **three are already partly or wholly built** — the honest
starting point:

| Borrowable (research-hindsight §11) | Current state on release/2.2 | Gap this plan fills |
|---|---|---|
| Bi-temporal / "recall June" | **Shipped.** `entity_page_links` windows (V56/V58) + page windows (V62, `valid_from`/`valid_to`) + `as_of` FTS (`reader.rs:1968`). | none — R4 done, out of scope here |
| Continuously-rewritten "mental models" (cross-session reflect loop) | **Mostly shipped.** `experience.rs::run_experience_review` reviews the last N session pages and rewrites procedures/rules/preferences; wired into the scheduler tick (`auto_improve_schedule.rs:190-251`), cadence-gated (`experience_pass_due`, `min_new_sessions`), state in `V57`. | **P4** — standing-answer semantics + boot surfacing + belief strength |
| Belief-strength / evidence-count consolidation | **Absent.** Pages carry `salience` (feedback, V37) and `importance` on observations (1..10), but **no evidence/proof/confidence** on a page or belief. `confidence`/`evidence_json` exist only on auto-improve *proposal* tables. | **P2** — the genuinely new mechanism |
| Typed causal-edge *retrieval* | **Data exists, retrieval ignores it.** `Relation{Causes,Fixes,Contradicts}` (`page.rs:208`) persists as `links.link_type` rows (`ops.rs:1052`); lint reads `contradiction_edges` (`reader.rs:6247`). But `graph_neighbors_for_project_explained` (`reader.rs:4165`) joins `links` with **no `link_type` filter** — one-hop neighbour expansion, no `fixes`/`causes` traversal, no `contradicts` in rank. | **P3** — the `design-page-ingestion-windows.md` §8 follow-up |
| Typed redaction labels (`[REDACTED:<kind>]`) | **Absent.** `sanitize.rs` replaces every match with the single constant `"[REDACTED]"` (`scrub()` :229) from a flat `BUILTIN_PATTERN_STRS` (:51). | **P1** — small clarity refinement |

So the plan is: **one small clarity fix (P1), one genuinely-new mechanism
(P2), one already-planned retrieval follow-up (P3), one extension of an
existing subsystem (P4), and a positioning doc (P5).** Bi-temporal (R4) needs
nothing.

## 1. Invariants every phase must hold

Restated from `AGENTS.md` because they bound each design below:

- **Zero-LLM default path (#13).** No phase may make capture, search, or a
  durable count *require* a provider. P2's evidence count is rule-based; P4's
  reflect pass stays LLM-opt-in exactly as `experience.rs` already is.
- **Single-writer actor (#2)** for every new write; **indexes commit in the
  data transaction (#3)**; **atomic wiki writes via `Wiki::write_page`/
  `apply_batch` (#10, the wiki mutation rules)**.
- **Migrations are backup-gated (#9 / #633 ordering).** Any new column/table is
  a `V63__*.sql` refinery step behind the pre-migration archive, plus the pin
  bump at `api_credentials.rs:414` (currently `62`).
- **MCP tool count is frozen at 19** (`server.rs:5198`, test `:5326`,
  `AGENTS.md:504`). None of these phases needs a *new* tool — they extend
  existing surfaces (`memory_query` explain, `memory_status`, `memory_lint`,
  the scheduler). If P2 ever warranted a dedicated tool, that is a separate
  decision with the count bump and both prompt-surface updates.
- **Semver.** All additive → **2.2.x minor**, on `release/2.2`. None is a
  patch (P1 changes durable sanitized output; the rest add surface).

## 2. P1 — Typed redaction labels (`[REDACTED:<kind>]`)

**Borrow:** Hindsight's Memory Defense redacts to a *typed* marker
(`[REDACTED:github_token]`) so a later reader knows *what kind* of secret was
present without leaking it — strictly more useful than an anonymous mask.

**Design.**
- Change `BUILTIN_PATTERN_STRS: &[&str]` (`sanitize.rs:51-148`) to
  `&[(&str, &str)]` (pattern, label) — e.g. `("gh[pousr]_…", "github_token")`,
  `("AKIA|ASIA…", "aws_key")`, `("eyJ[…]", "jwt")`.
- Change `SanitizerInner.patterns: Vec<Regex>` (:156) to
  `Vec<(Regex, &'static str)>`; `scrub()` (:219-235) formats
  `format!("[REDACTED:{label}]")` instead of the constant at :229. Operator
  `extra_patterns` (SanitizeConfig, :173) default to label `custom`.
- **Backward-compat / output-change note (this is why it is a minor, not a
  silent patch):** the sanitized string is *durable* — it is stored in pages
  and observations. Old rows keep `[REDACTED]`; new writes get
  `[REDACTED:<kind>]`. That is acceptable (both are non-secrets), but it is a
  visible format change, so it ships in a minor with a CHANGELOG note, not
  quietly.
- **Test surface:** assertions that check `contains("[REDACTED]")`
  (`sanitize.rs:368-598`) break, because `[REDACTED:jwt]` does **not** contain
  the exact substring `[REDACTED]`. Update them to `contains("[REDACTED")` (open
  bracket-prefix) for the positive cases; the negative `!contains(...)`
  no-false-positive tests are unaffected. Add one test per label asserting the
  right `kind`.

**Cost:** small, no migration, single file + tests. **Risk:** low (trust
boundary #6 is preserved — still the only path from untrusted text to store,
still fail-safe: an unknown/`extra` pattern falls back to label `custom`, never
to leaking).

## 3. P2 — Belief-strength / evidence-count on pages (the new mechanism)

**Borrow:** Hindsight observations are *evidence-backed beliefs* — quotes plus a
**proof count** — and "new information strengthens, weakens or extends an
existing belief instead of silently replacing it." We supersede (a version
chain); we do not track *how much evidence* stands behind the current version.

**Why it is worth it.** "The team decided against a queue" should not be just
the latest page; it should be a page with N supporting sightings, where a
single contradicting sighting *weakens* rather than instantly *replaces* it.
That belief strength is exactly the missing input to (a) ranking authority and
(b) the `contradicts` lint — and it is the substrate the deferred Phase-B
`confidence` in `design-page-ingestion-windows.md` §7 was gesturing at, now
with a zero-LLM way to populate it.

**Design (evaluate-first; a design doc precedes the PR, this is it).**

- **New append-only table `page_evidence`** (`V63`): `(page_id, source_kind
  CHECK('session'|'observation'|'feedback'|'reconsolidation'), source_id,
  created_at)`, unique on `(page_id, source_kind, source_id)`. Written **in the
  same transaction** as the page upsert (consolidator's `write_page`/
  `apply_batch` path, `consolidator.rs:199/564`) from the sessions/observations
  that produced the page. Purely rule-based — no LLM — so the zero-LLM path
  populates it. This is the "proof count" analogue; keeping the source ids
  (not just an integer) lets a later reader see *what* supports a belief and
  lets recency-weighting work (below).
- **Derived `confidence` at read time**, not a stored mutable scalar (avoids a
  hot-path write on every access and a second source of truth): `confidence =
  f(evidence_count, distinct_sessions, age_of_newest_evidence,
  unresolved_contradiction_count)`. Start simple and monotone: more distinct
  supporting sessions ⇒ higher; an unresolved `contradicts` edge (from
  `contradiction_edges`, `reader.rs:6247`) ⇒ lower.
- **Feed it into ranking through the existing `PageAuthority`** (`reader.rs:209`)
  as one more bounded factor, clamped inside the current `[0.55, 1.50]` — never
  a new multiplier tower. Report it in `SearchExplain` (`reader.rs:547`) as
  `confidence` + `evidence_count` alongside `authority`, so it is inspectable
  via `memory_query(explain=true)` and never a black box. Surface the count in
  `memory_status` too.
- **Strengthen / weaken / extend semantics:**
  - *Strengthen* — reconsolidating the same path from a new session appends a
    `reconsolidation`/`session` evidence row (count rises). Already the shape of
    the supersede path; P2 just records the source.
  - *Weaken* — an unresolved `contradicts` edge landing lowers derived
    confidence (no write; it is a read-time input).
  - *Extend* — unchanged supersession (the body grows/changes); P2 does not
    alter versioning.
- **The entrenchment failure mode** (flagged in `research-hindsight.md` §5): a
  popular-but-wrong belief could outvote a correct correction. Mitigations
  baked into `f`: recency-weight evidence (newest sightings count more), and
  **a supersession always wins regardless of count** — a human/agent correction
  replaces the version; evidence count only shades *ranking*, never gates
  *whether* a correction takes. Confidence influences rank; it never blocks a
  write. That keeps invariant #16's "a divergent write supersedes, never
  destroys" intact.

**Cost:** medium — one backup-gated migration (V63 + pin bump), one write-path
hook (transactional evidence insert), one read-time derivation + explain field,
retention interaction (evidence rows cascade on page purge like links).
**Risk:** medium; gate behind an eval (does confidence-in-authority improve
retrieval on the R2 harness, or just add noise?) before it touches default
ranking — same discipline `design-page-ingestion-windows.md` §7 applied to
freshness. Ship the *table + count + explain field* first (inert to ranking),
turn on the authority factor only with a number behind it.

## 4. P3 — Typed causal-edge retrieval + `contradicts` in rank

**Borrow:** Hindsight runs a graph retrieval strategy over
entity/temporal/**causal** links, not just neighbour expansion. Our typed edges
(`causes`/`fixes`/`contradicts`) exist as data but are invisible to retrieval.

**Design.**
- **Teach the graph stream the edge type.** `graph_neighbors_for_project_
  explained` (`reader.rs:4165`) currently joins `links` with no `link_type`
  filter. Add the edge kind to the traversal: expose `link_type` in `GraphVia`
  (`reader.rs:508`) so `explain` shows *why* a neighbour surfaced (a `fixes`
  edge vs a bare `references`), and let a typed edge contribute a **stronger RRF
  rank** than a plain reference (a `fixes`/`causes` neighbour is more relevant
  to a bug/decision query than an incidental wikilink).
- **Bounded `fixes`/`causes` chain traversal.** Optionally follow `fixes` one
  extra hop (bug → fix → the fix's own page), strictly depth-capped (≤2) and
  behind the same `[retrieval]` opt-in gate style as `abstract_vectors` — so the
  default fan-out stays bounded (invariant: no unbounded graph walks).
- **`contradicts` into rank** — the explicit follow-up named in
  `design-page-ingestion-windows.md` §8 ("typed contradicts edges entering
  rank: natural follow-up once page windows exist"). A page on the unresolved
  side of a `contradicts` edge takes an authority cap (like the existing
  `superseded → 0.65` cap in `PageAuthority`), so a contradicted-but-latest page
  ranks below its challenger until the contradiction resolves. This composes
  directly with P2's confidence (a contradiction weakens both rank and
  confidence).

**Cost:** small-medium — no migration (edges exist), changes localized to the
graph-stream builder + `PageAuthority` + `GraphVia`/explain. **Risk:** low-medium
— keep the chain traversal opt-in and depth-capped; prove the typed-edge rank
weighting on the R2 harness before making it default (same baseline-before-change
rule).

## 5. P4 — "Mental models": standing-answer pages the agent boots from

**Borrow:** Hindsight's top tier is a markdown page of *settled* knowledge an
agent "boots" from instead of rediscovering context each session.

**Honest scope.** We already have the reflect loop: `experience.rs::
run_experience_review` reviews the last N session pages across a project and
rewrites procedures/rules/preferences (`EXPERIENCE_SYSTEM_PROMPT`), cadence-gated
in the scheduler tick. The two deltas vs Hindsight's mental models are:

1. **Standing-answer semantics.** Give the experience pass an explicit output
   lane: a small set of durable, per-project "what we've settled on" pages (a
   `PageKind`/tier already exists — `rule`/`decision` carry the highest
   `PageAuthority`; reuse them, do not invent a tier). The pass *maintains* these
   rather than creating new episodic pages, and P2's evidence count is what
   makes "settled" measurable (a rule with many supporting sessions is settled;
   a one-off is not).
2. **Boot surfacing.** `memory_briefing` (an existing tool, no new surface) is
   the "boot from a page of settled knowledge" seam — bias it to lead with the
   high-confidence standing-answer pages (P2 feeds this) so a new session opens
   with settled knowledge first, exactly Hindsight's "page of settled knowledge
   instead of rediscovering it every session."

**Cost:** medium, but mostly prompt + selection + briefing wiring on top of an
existing subsystem — no new scheduler, no new subsystem. Depends on P2 for the
"settled" signal. **Risk:** medium; the experience pass is LLM-opt-in and stays
so (zero-LLM installs keep session pages + briefing without the abstraction
lane). Guard against the pass churning the standing pages every tick (the
existing cadence gate + "only rewrite when evidence changed" covers this).

## 6. P5 — "Use the subscription you already pay for" (positioning)

**Borrow:** Hindsight advertises no-API-key onboarding via ChatGPT Plus / Claude
Pro / Copilot subscriptions. We already support Copilot and Anthropic-OAuth
tokens — this is a **docs/positioning** change, not code: promote "use your
existing Claude Pro / Copilot subscription, no API key" to a first-class line in
`docs/install.md` and the provider setup, and confirm the OAuth/Copilot paths are
a one-liner in `install-mcp`/config. **Cost:** trivial (docs). **Risk:** none.
Include it so the study's onboarding insight is not lost.

## 7. Sequencing & how each ships independently

Smallest-first, each a standalone 2.2.x PR:

1. **P1** (typed redaction labels) — no deps, no migration. Immediate.
2. **P3** (typed-edge retrieval + `contradicts` rank) — no migration; the
   already-planned §8 follow-up. Prove the rank weighting on R2 before default.
3. **P2** (evidence table + count + explain, inert to rank) — V63 migration;
   ship the count + `explain`/`status` surfacing first, authority factor behind
   an eval.
4. **P4** (standing-answer lane + briefing bias) — depends on P2's "settled"
   signal; extends `experience.rs`.
5. **P5** (subscription onboarding docs) — any time; trivial.

R2 (the reproducible LongMemEval harness, `research-2026-landscape.md` R2) is the
prerequisite for turning P2's authority factor and P3's rank weighting *on by
default* — both are gated "table/stream first, ranking change only with a number."
That keeps us out of the mempalace trap the research flagged.

## 8. Explicitly NOT borrowing (from research-hindsight.md §12)

- **LLM-required capture** — we own the zero-LLM segment; P2's counts and P1 are
  rule-based, P4 stays opt-in. Do not follow Hindsight into provider-mandatory
  writes.
- **Postgres / enterprise substrate** — single self-contained binary stays the
  homelab differentiator.
- **Strict bank isolation** — that is issue #708 (per-project authorization),
  tracked separately; the design there layers a boundary *above* our
  shared-within-project model (invariant #16), it does not replace sharing with
  Hindsight-style isolation. #709 (project identity) is its prerequisite.
- **Disposition traits** (skepticism/literalism/empathy) — unproven
  metaphor-as-architecture until an ablation says otherwise; not planned.

## 9. Open questions (for maintainer input before any coding)

1. **P1 output change** — is a durable move from `[REDACTED]` to
   `[REDACTED:<kind>]` wanted at all, given it changes stored text and touches
   the #6 trust boundary? (Alternative: keep `[REDACTED]` and add the kind only
   in a structured sidecar — but that loses the in-body readability that is the
   whole point.)
2. **P2 confidence placement** — derived-at-read (this plan's recommendation, no
   hot-path write, single source of truth in `page_evidence`) vs a
   materialized `pages.confidence` column (cheaper reads, but a mutable scalar to
   keep honest and a second write on every access)? The V62 doc chose
   materialized for symmetry; here the read-derived option avoids a hot-path
   write, which is the opposite tradeoff and worth a call.
3. **P2 grain** — evidence on *pages* (this plan) vs on the typed *edges*/
   entity-links (closer to Hindsight's per-belief proof count, but a bigger
   change)? Pages are the unit our ranking already authority-weights, which is
   why this plan starts there.
4. **P3 default vs opt-in** — should typed-edge rank weighting be on by default
   once eval-proven, or a `[retrieval]` opt-in like `abstract_vectors`?
5. **P4** — is a distinct standing-answer surfacing in `memory_briefing`
   desirable, or does biasing by P2 confidence within the existing briefing
   suffice without a visible new lane?

## 10. Acceptance (per phase, when each PR is written)

- **P1:** every builtin pattern asserts its `kind`; operator `extra_patterns`
  default to `custom`; the no-false-positive tests still hold; CHANGELOG
  `### Added`/`### Changed` note on the output-format change.
- **P2:** `page_evidence` populated transactionally with the page (test:
  evidence count rises on reconsolidation, unaffected by a plain read); purge
  cascades evidence; confidence appears in `explain`/`status`; **ranking
  unchanged until the authority factor is explicitly enabled with an R2 number**;
  V63 migration backup-gated + pin bumped + idempotent-backfill test.
- **P3:** `explain` shows edge `link_type` in `graph_via`; typed edges outrank
  bare references on a fixture query; `contradicts` caps the contradicted page's
  authority (test with a resolved vs unresolved control); chain traversal
  depth-capped and opt-in.
- **P4:** experience pass maintains (not multiplies) standing-answer pages;
  `memory_briefing` leads with high-confidence settled pages; zero-LLM install
  still gets session pages + briefing.
- **P5:** `docs/install.md` documents the no-API-key subscription path; verified
  against the existing Copilot/Anthropic-OAuth support.
- **All:** full local gate per `AGENTS.md`; fast Linux CI per merge; full
  macOS/Windows matrix on the release-candidate SHA before the 2.2.0 tag.
