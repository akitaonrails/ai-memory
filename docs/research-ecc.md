# ECC — Research Report

*Repo:* <https://github.com/affaan-m/ECC> · MIT · primarily JavaScript/TypeScript
(with Python, Go, Shell). *Snapshot:* 2026-09-03: ~247k stars, ~37k forks,
created 2026-01, pushed daily. One of the most-starred AI-agent repos on
GitHub, single-maintainer-led with an active contributor community.

> **Category caveat, up front.** ECC is **not** a memory backend like
> ai-memory. It bills itself as *"the agent harness operating system"*, a
> plan → test → implement → review → verify → **remember** → improve loop
> installed once into a coding agent (Claude Code, Codex, OpenCode, Cursor,
> Gemini, Zed, …) as skills, subagents, rules, hooks, and MCP wiring. Memory
> is one pillar among Skills, Instincts, Security, and Research-first
> development, not the product itself. This report compares only the overlapping
> surface (its memory / continuous-learning component) and mines it for
> insights, rather than treating ECC as a like-for-like competitor.

## 1. Purpose & Scope

ECC's pitch: *"Your agent can write code, but ECC gives it a coordinated
engineering system and toolbox: it plans before it builds, verifies changes
with tests, reviews its own work from a fresh context, remembers what
matters, and turns repeated wins into reusable skills and workflows."* The
memory value prop fits in one line, **"Optimize the context window.
Persist everything else."**, which is also ai-memory's reason to exist.
ai-memory *is* the "persist everything else" layer; ECC ships a lightweight
version of it inside a much larger workflow framework.

ai-memory is the deeper, dedicated memory system; ECC is the broader agent
scaffolding with a shallow-but-opinionated memory slice. The two sit next to
each other and do not replace one another.

## 2. Architecture

- **File-based, no server.** State lives as Markdown under
  `$ECC_AGENT_DATA_HOME` / `.ecc/memory/` (project), version-controlled team
  memory, and `~/.ecc/memory/` (user). A SQLite state store backs instinct
  tracking and session recording; git distributes config.
- **Hook-driven persistence.** PreToolUse / PostToolUse / Stop hooks
  (`hooks/hooks.json`, with `ECC_HOOK_PROFILE=minimal|standard|strict`
  runtime gating) capture session summaries and learned patterns. This is the
  same lifecycle-hook capture approach ai-memory uses, but it writes local
  files instead of posting to a server.
- **Skills / Agents / Rules.** ~286 Markdown "skills" (workflows loaded on
  demand), ~68 subagents with isolated context + scoped tool permissions, and
  always-loaded language/framework "rules" packs. This is a governance layer
  ai-memory does not have and does not aim to.

Contrast: ai-memory is a single-writer SQLite + wiki-git **server** with a
real retrieval engine (FTS5 + vector + entity + graph, fused by RRF) and an
OKF-portable on-disk format; ECC is a distributed set of Markdown files + a
SQLite side-table, retrieved by search over active files.

## 3. Memory Model

- **Format:** `ecc.memory.v1` Markdown documents, portable and inspectable
  (the same idea as ai-memory's OKF-v0.2 human-readable wiki).
- **Scopes:** project / team / user. The **team scope is version-controlled
  and shared through git**, a different multi-user approach from
  ai-memory's server-mediated workspaces (see §6).
- **Capture:** semi-manual (`ecc memory save`, `ecc memory handoff`,
  `--stdin` / `--body-file`) plus hook-driven session summaries. This is
  the biggest divergence: ai-memory's capture is **automatic and
  zero-ceremony** (every prompt/tool/session boundary sanitized and stored by
  lifecycle hooks with no `save` call), whereas ECC leans on explicit save
  commands for durable notes and reserves automation for summaries.
- **Retrieval:** `ecc memory search "query"` over active memories, and
  `ecc memory doctor` for vault integrity. No evidence of hybrid
  vector/entity/graph retrieval or temporal (`as_of`) queries. It appears to
  be text search over a curated file set, which is adequate for a small,
  human-pruned vault but not for a large auto-captured corpus.

## 4. Consolidation & Continuous Learning ("Instincts")

ECC's "Continuous Learning v2" extracts **instincts with confidence
scoring** from session patterns (*"instinct-based learning with confidence
scoring, import/export, evolution"*) and can promote recurring wins into
generated skills. This maps closely to ai-memory's session-end consolidation
+ auto-improve loop, with two notable differences:

1. **Explicit confidence + evolution.** ECC attaches a confidence score to
   each learned instinct and supports import/export and "evolution" over
   time. ai-memory's auto-improve is eval-gated and staged, but confidence is
   implicit; surfacing a per-page/per-rule confidence would be a cheap,
   legible upgrade.
2. **Promotion to skills, not just recall.** ECC turns repeated patterns into
   *executable* skills/workflows, not only retrievable notes. That is a stronger
   "learning produces behavior" story than ai-memory's "learning produces
   better recall."

## 5. Distinctive Ideas Worth Noting

- **"Memory is unreviewed context, not executable policy. Verify important
  claims against authoritative sources and promote accepted knowledge into
  governed project documentation."** This is the most useful idea in the repo.
  It draws a hard line between *raw captured memory* (untrusted, for context)
  and *governed knowledge* (reviewed, promoted into rules the agent obeys).
- **AgentShield**: a bundled security scanner that audits harness config,
  hooks, MCP definitions, agent files, permissions, and secrets
  (`ecc-agentshield scan --path .`). ai-memory has a security-audit *skill*
  but ships no scanner for the hook/MCP config it writes.
- **Plan Canvas**: browser review of implementation plans (Mermaid
  diagrams, approval gates). Unrelated to memory, but a useful human-in-the-
  loop surface.
- **Research-first development**: source-verification baked into the flow
  before knowledge is trusted.

## 6. Cross-Agent / Multi-User

ECC's **team memory is git-shared Markdown**: every teammate pulls the same
committed vault. ai-memory instead mediates multi-user/multi-machine through
a **server** (attributed writes, per-user scoping, live sync across
machines). ECC's model is simpler and offline-friendly but has git's
merge/conflict story and no live cross-session handoff; ai-memory's is richer
(real-time handoff, attribution, one store many machines) at the cost of
running a server. Which one fits depends on the team.

## 7. What's Good / What's Missing — Honest Take

**What ECC does better (or at least differently, worth stealing):**
- The **memory-vs-governed-knowledge distinction** as a first-class
  principle. ai-memory already hints at this (the lint pass suggests
  "this looks like a durable rule — copy it into CLAUDE.md/AGENTS.md"), but
  ECC makes *promotion* an explicit, designed path. ai-memory could formalize
  a **promotion flow**: memory page → reviewed → emitted as a governed
  `AGENTS.md`/rule the agent loads every turn, closing the loop the lint only
  hints at.
- **Explicit confidence scores** on learned knowledge (cheap legibility win).
- **A bundled config scanner** (AgentShield analogue) for the hooks/MCP
  ai-memory itself installs. The data-layer audit this project just ran by
  hand could become a shippable `ai-memory audit-config`.
- **Learning that produces behavior** (instincts → skills), not just recall.

**Where ai-memory is clearly ahead (for the memory problem specifically):**
- **Automatic, zero-ceremony capture** vs ECC's `ecc memory save`. ai-memory
  never asks the agent to remember to persist.
- **A real retrieval engine** (hybrid FTS5 + vector + entity + graph RRF,
  temporal `as_of`, benchmarked on LongMemEval-S, hit@5 0.62 → 0.82) vs
  text search over an active file set.
- **Scale of corpus.** ai-memory is built to auto-capture and retrieve over
  thousands of pages and hundreds of thousands of observations; ECC's vault
  is a smaller, human-curated set.
- **Server-mediated multi-user, temporal windows, OKF portability, and
  eval-gated consolidation.**

**Bottom line.** ECC is an agent-harness OS rather than a memory competitor,
and its memory is deliberately thin ("optimize the context window, persist
everything else; keep memory as context, not policy"). Its best lesson for
ai-memory is *governance*: the explicit line between raw memory and promoted,
reviewed, agent-obeyed knowledge, plus confidence scoring and a config
scanner. Its capture and retrieval are well behind ai-memory's, by design.

## 8. Concrete Ideas to Consider for ai-memory

1. **A promotion path** memory → governed rule (`AGENTS.md`/`CLAUDE.md`),
   turning the existing "looks like a durable rule" lint into an actual
   reviewed-and-emitted governance artifact.
2. **Surface confidence** on consolidated pages / auto-improve proposals
   (already eval-gated internally; make it legible in `status`/`memory_query`).
3. **`ai-memory audit-config`**: a shippable scanner for the hook/MCP config
   ai-memory writes (AgentShield analogue), built from the data-layer audit
   work already done.
4. **Consider a lightweight git-shared "team rules" scope** for offline/small
   teams that don't want to run the server, as a complement to the server
   model rather than a replacement.
