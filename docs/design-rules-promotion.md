# Design: Promoting Memory into Governed AGENTS.md Rules (2.1)

*Status: design, targeting the `release/2.1` line. Not implemented yet — this
is the balance research the feature needs before code.*

## 1. The idea, and the trap

Insight borrowed from ECC (`docs/research-ecc.md`): **"Memory is unreviewed
context, not executable policy. Promote accepted knowledge into governed
project documentation."** ai-memory already gestures at this — the lint emits
a `rule_suggestion` finding ("this page looks like a durable rule; consider
copying it into CLAUDE.md/AGENTS.md"). The 2.1 feature closes that loop:
turn a durable memory into a **governed, always-loaded rule** the agent obeys
every turn.

The trap the maintainer named, and the one that kills this feature if we get
it wrong: **AGENTS.md is a scarce, shared budget, not a filing cabinet.** If
promotion dumps every learned rule in, AGENTS.md bloats, adherence collapses,
and we've made the agent *worse*, silently.

## 2. Why the budget is real (not a preference)

From the 2026 best-practice consensus (sources below):

- Frontier models reliably follow only **~150–200 instructions**; the harness
  system prompt already spends **~50** of them. The usable budget for
  *promoted user rules* is therefore ~**100 instruction-lines**, shared with
  everything the human already wrote in AGENTS.md.
- Past ~200 lines, **"context rot" / "lost in the middle"** sets in —
  measured adherence drops **>30%**, and it drops for the rules that *matter*,
  not just the new ones. Bloat doesn't add weak rules; it weakens strong ones.
- **`@path`/import pointers do not save context** — imported files load at
  launch. "Move it to a linked doc" is not a budget saving; it's the same
  tokens with indirection.
- The load-bearing test for any AGENTS.md line: *"Would removing this cause
  the agent to make a mistake?"* If no, it does not belong there.

Conclusion: promotion must be **subtractive by default** — it competes for a
fixed number of slots and must *evict* to *admit*.

## 3. Two tiers, one clear boundary

| | **Governed rule** (AGENTS.md) | **Retrieval knowledge** (default) |
|---|---|---|
| Loaded | Every turn, always | On demand, during planning, via `memory_query` |
| Cost | A slot in the scarce budget | Zero standing cost |
| For | Invariants, guardrails, conventions the agent must *never* violate and that apply *broadly* | Context-specific facts, decisions, gotchas, history — "what we did / why X is the way it is" |
| Test | "Forgetting this causes a mistake on many tasks" | "Relevant only when the task touches it" |
| Examples | "All SQLite writes go through the single writer actor"; "never write files in place — tmp+rename" | "In July we chose Caddy over nginx because…"; a month-old debugging gotcha |

**The default is retrieval.** A memory is `_rules/`-kind and durable, and it
*still* stays retrieval-only unless it clears a higher bar (below). This is
the anti-bloat stance: promotion is the exception, not the reward for
durability. Old knowledge, one-off gotchas, and decisions-with-context are
*exactly* what should surface during task planning, never permanently.

## 4. What clears the bar for promotion

A candidate (a `_rules/`-kind page the lint already flags) is eligible only
if it is **broad, imperative, durable, and cheap to state**:

1. **Broad applicability** — it constrains many tasks, not one file/feature.
   Heuristic: referenced/accessed across multiple sessions and paths, not a
   single-scope note.
2. **Imperative & falsifiable** — it says *do X / never Y*, not "we discussed
   Z." A rule you can catch the agent violating.
3. **Durable & high-confidence** — survived N sessions without contradiction,
   with a confidence score (another ECC borrow) above a threshold; no open
   `contradicts` typed-edge against it.
4. **Compact** — one or two lines. If it needs a paragraph of context, it is
   retrieval knowledge, not a rule.

Everything that fails any test stays retrieval-only. This keeps the promoted
set small *by construction*, not by after-the-fact pruning.

## 5. Anti-bloat by mechanism, not discipline

- **Hard budget.** A configurable cap (default small, e.g. **≤ 15 promoted
  rules / ~40 lines**) on the managed block. Promotion is admission-controlled
  against it.
- **Evict to admit.** At the cap, a new rule promotes only by demoting the
  lowest-scoring current rule back to retrieval-only (it is not deleted — it
  returns to the `_rules/` page, still queryable). Score = confidence ×
  breadth × recency-of-use.
- **Decay & demotion.** A promoted rule that stops being exercised (no related
  activity, no reinforcement) decays and is demoted, so the block reflects
  *current* policy, not accumulated history — the same decay philosophy the
  store already uses for pages.
- **Never auto-promote silently on the hot path.** Promotion runs through the
  existing eval-gated, staged **auto-improve** pipeline (pending-writes,
  reviewed), never as a live side effect of a session.

## 6. Transparency — "just enough," never behind the user's back

The maintainer's exact requirement: transparent by default, not verbose,
notify *just enough* so nothing changes silently.

- **One managed, delimited block.** Reuse the existing marker mechanism
  (`ai_memory_core::routing_snippet`: `<!-- ai-memory:start -->` …
  `<!-- ai-memory:end -->`). Promoted rules live **only** inside a clearly
  labelled sub-block (e.g. `<!-- ai-memory: promoted rules (managed) -->`),
  so the human's hand-written AGENTS.md is never touched, and the boundary of
  "what ai-memory added" is unambiguous and reversible.
- **Provenance per rule.** Each promoted line carries a terse trailer — source
  page + confidence — so the reader can see *why* it's there and open the full
  memory.
- **Opt-in, and staged.** Promotion is **off by default**; when on, each
  promotion/demotion is a *pending* change the operator reviews (the
  auto-improve staging surface), not an automatic edit — the same gate the
  rest of auto-improve uses.
- **Notify at the right altitude.** When a promotion/demotion is staged (or
  applied, if the operator enabled auto-apply), surface it *once*, concisely:
  a `status` line ("2 rules promoted, 1 demoted this cycle — review with …")
  and the diff of the managed block. No per-turn chatter; no silent edits.
  The principle: the user should never discover a behavior change by
  debugging — but should also never be nagged.
- **Fully reversible.** Removing the managed block (or turning the feature
  off) restores the prior AGENTS.md byte-for-byte; demoted rules lose nothing
  (the `_rules/` page remains).

## 7. Reusing what already exists

Nothing here needs a new subsystem — it composes primitives 2.0 already ships:

- **Candidate signal:** the lint `rule_suggestion` finding.
- **Durability/typed contradiction:** `_rules/` pages, `kind: rule`, typed
  `contradicts` edges, decay/salience.
- **Review gate:** the auto-improve staged, eval-gated, pending-writes path.
- **The write surface:** the `routing_snippet` marker-block mechanism that
  already manages a delimited region of CLAUDE.md/AGENTS.md.
- **Confidence:** to be surfaced (a small, legible addition — see
  `docs/research-ecc.md`).

## 8. Proposed 2.1 scope vs later

**2.1 (this feature):**
- Config: `[rules_promotion] enabled` (default false), `max_promoted`,
  `min_confidence`, `auto_apply` (default false → stage only).
- The classifier (§4) + scorer (§5) over `_rules/` candidates.
- A managed "promoted rules" sub-block writer via `routing_snippet`, with
  provenance trailers, staged through auto-improve.
- `status` reporting of the promoted set + last cycle's promote/demote deltas.
- Demotion/decay.

**Later (explicitly out of 2.1):**
- Cross-project / team rule sharing.
- Per-glob scoped rules (Cursor-style) rather than one global block.
- Automatic rule *synthesis* (merging several memories into one crisp rule) —
  starts as human-reviewed only.

## 9. Open questions (for maintainer input before coding)

1. Budget default: is ~15 rules / ~40 lines the right ceiling, or tighter?
2. Should promotion ever auto-apply, or always stay stage-and-review?
3. Where should the managed block sit — AGENTS.md, CLAUDE.md, or both (and
   how to avoid double-loading when a tool reads both)?
4. Demotion signal: pure decay, or also an explicit "this rule was wrong"
   feedback that hard-demotes and records a `contradicts` edge?

---

### Sources
- [Best practices for Claude Code — Claude Code Docs](https://code.claude.com/docs/en/best-practices)
- [CLAUDE.md Best Practices: What the Evidence Supports (2026) — Alex Dunlop](https://www.alexdunlop.com/writing/claude-md-best-practices)
- [AGENTS.md Best Practices: Template and Guide (2026) — BetterClaw](https://www.betterclaw.io/blog/agents-md-best-practices)
- [CLAUDE.md Best Practices, 2026 — AgentLint](https://www.agentlint.app/blog/claude-md-best-practices-2026/)
- `docs/research-ecc.md` (ECC's "memory as context, not policy")
