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
- **Evict to admit — but the user chooses.** At the cap, `rules approve`
  refuses to silently overflow: it names the lowest-scoring current rule and
  asks the user to `remove` it (or raise the cap) first. Nothing is demoted
  without a command. Score = confidence × breadth × recency-of-use, used only
  to *rank* and to *suggest* what to drop.
- **Decay lowers rank, never removes.** A promoted rule that stops being
  exercised sinks in `rules recommend` and can be flagged stale for the user
  to `remove`, so the block trends toward *current* policy — but the removal
  is always a human command, never automatic.
- **Never writes on the hot path.** The classifier/scorer that *produce*
  candidates can reuse the consolidation machinery, but the AGENTS.md block is
  written *only* by an explicit `approve`/`edit`/`remove` — never as a live
  side effect of a session (see §6).

## 6. Command-driven, never auto-editing (maintainer's call, and the right one)

The feature does **not** edit AGENTS.md on its own. There is no background
writer and no "auto-apply." ai-memory only ever *recommends*; the human's
explicit command is the only thing that changes the file. This is strictly
better than staged-auto-apply for the maintainer's stated fear — the user is
in the loop *by construction*, so nothing can change behind their back, and
there is no "did I get notified enough?" problem to tune.

**The command surface** (CLI, mirrored as MCP tools so an agent can run them
when the user asks):

- **`ai-memory rules recommend`** — read-only. Lists candidate promotions the
  classifier/scorer surfaced (§4–5), ranked, each with: the one-line rule,
  source page, confidence, breadth signal, and *why it qualified*. Changes
  nothing. This is the discovery surface — the user asks "what have you
  learned that's rule-worthy?" and sees a short, ranked list, not a wall.
- **`ai-memory rules approve <id>`** — promote one candidate into the managed
  AGENTS.md block. If the block is at budget, the command *tells the user*
  ("at 15/15; approving this means dropping <lowest>, or raise the cap") and
  does nothing until they decide — eviction is a user choice, never silent.
- **`ai-memory rules edit <id>`** — tweak the rule's wording before/after
  promoting (the human phrasing usually beats the extracted one).
- **`ai-memory rules remove <id>`** — demote a promoted rule back to
  retrieval-only. The `_rules/` page is untouched; only the AGENTS.md line goes.
- **`ai-memory rules list`** — show what is currently promoted (the managed
  block's contents, with provenance).

**Guarantees:**
- **One managed, delimited block.** The block reuses the existing marker
  mechanism (`ai_memory_core::routing_snippet`: `<!-- ai-memory:start -->` …
  `<!-- ai-memory:end -->`), in a clearly labelled sub-block (e.g.
  `<!-- ai-memory: promoted rules (managed) -->`). The human's hand-written
  AGENTS.md outside the markers is **never** touched.
- **Provenance per rule.** Each promoted line carries a terse trailer (source
  page + confidence) so a reader sees *why* it's there and can open the memory.
- **Recommendations are pull, not push.** Candidates accrue silently; the user
  sees them only when they run `recommend`. The one *optional*, low-key nudge:
  a single `status` line ("N new rule recommendations — `ai-memory rules
  recommend`") when the candidate set grows, off by a config flag. No per-turn
  chatter, no pending-writes to babysit.
- **Fully reversible.** Removing the managed block (or never running `approve`)
  leaves AGENTS.md exactly as the human wrote it; `remove`/demotion loses
  nothing (the `_rules/` page remains queryable).

The earlier auto-improve staging path is *not* used here — promotion is a
deliberate, human-issued edit, not an eval-gated background write. (The
classifier/scorer that *produce* candidates can still reuse the consolidation
machinery; only the write is command-gated.)

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
  `min_confidence`, and `recommend_hint` (the optional one-line `status`
  nudge; default off). **No `auto_apply` — there is none.**
- The classifier (§4) + scorer (§5) over `_rules/` candidates.
- The command surface: `rules recommend` / `approve` / `edit` / `remove` /
  `list` (CLI + MCP tools), each read-only except the three that the user
  explicitly invokes to change the managed block.
- The managed "promoted rules" sub-block writer via `routing_snippet`, with
  provenance trailers — written **only** by `approve`/`edit`/`remove`.
- `status` reporting of the currently-promoted set (and the optional
  new-recommendations hint).
- Demotion via `remove`; decay only *lowers a rule's rank in `recommend`* and
  can flag a stale promoted rule for the user to `remove` — it never
  auto-removes.

**Later (explicitly out of 2.1):**
- Cross-project / team rule sharing.
- Per-glob scoped rules (Cursor-style) rather than one global block.
- Automatic rule *synthesis* (merging several memories into one crisp rule) —
  starts as human-reviewed only.

## 9. Open questions (for maintainer input before coding)

1. Budget default: is ~15 rules / ~40 lines the right ceiling, or tighter?
2. Where should the managed block sit — AGENTS.md, CLAUDE.md, or both (and
   how to avoid double-loading when a tool reads both)?
3. Should `approve` also (optionally) emit a small commit so the promotion is
   captured in the repo's history, or leave that to the user's normal flow?
4. `recommend`'s default verbosity: top-N only (say 5), with a `--all` flag?

*(Resolved by maintainer, 2026-09-03: no auto-editing of AGENTS.md — the
feature only recommends; `approve`/`edit`/`remove` commands are the only
things that write the managed block. §6 reflects this.)*

---

### Sources
- [Best practices for Claude Code — Claude Code Docs](https://code.claude.com/docs/en/best-practices)
- [CLAUDE.md Best Practices: What the Evidence Supports (2026) — Alex Dunlop](https://www.alexdunlop.com/writing/claude-md-best-practices)
- [AGENTS.md Best Practices: Template and Guide (2026) — BetterClaw](https://www.betterclaw.io/blog/agents-md-best-practices)
- [CLAUDE.md Best Practices, 2026 — AgentLint](https://www.agentlint.app/blog/claude-md-best-practices-2026/)
- `docs/research-ecc.md` (ECC's "memory as context, not policy")
