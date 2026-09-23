# How ai-memory compares

For anyone evaluating ai-memory against another agent-memory tool — or
migrating from one. This page aims to be **fair and specific**: what each
approach does well, where ai-memory differs, where others are ahead, and how
the field has independently validated the bets ai-memory made. The deep
analysis behind it is in [`research-2026-landscape.md`](research-2026-landscape.md)
and the per-project research docs; the numbers are from
[`benchmarks/`](benchmarks/README.md), reproducible from the in-repo harness.

## The short version

Most memory tools optimize one of: extracting atomic **facts** per turn
(Mem0, LangMem), a temporal **knowledge graph** (Zep/Graphiti), an
agent-editable **memory OS** (Letta, MemOS), or a hosted **context database**
(OpenViking). ai-memory optimizes something different: **a git-backed markdown
wiki as the source of truth, with a derived index for retrieval**, captured
automatically from lifecycle hooks, shared across agents and machines.

What actually distinguishes it:

- **Cross-agent and cross-machine by construction.** One server; 20+ harnesses
  (Claude Code, Codex, Cursor, Gemini, OpenCode, Kimi, …) feed and read the
  same memory. Quit one agent, open another in the same repo, get a real
  handoff. Handoffs are a typed protocol (owned, claimed exactly once), not a
  note file.
- **Zero-LLM default.** Capture, FTS5 + entity + graph retrieval, local
  embeddings, and rule-based summaries all work with **no API key**. An LLM is
  opt-in for richer consolidation — not a requirement to function.
- **Files are the truth.** The wiki is plain markdown + YAML frontmatter — a
  native [Open Knowledge Format](okf.md) (OKF v0.2) bundle. `grep` it, open it
  in Obsidian, edit by hand, `rsync` it. The SQLite index is derived and
  rebuildable. Nothing is trapped in a vector store or binary blob.
- **Multi-user without a paid tier.** An auth ladder (root → DB-user tokens →
  OIDC), per-person attribution, audit log, and the invariant that **pages are
  shared per project while handoffs stay owned** — a team story built in.
- **One self-contained binary.** Bundled SQLite, vendored libgit2; no external
  services to stand up. Runs on a laptop, a homelab box, or a LAN server.

And it publishes numbers: **LongMemEval-S hit@5 0.823** (local-embeddings
default; 0.668 zero-LLM), produced by the in-repo harness with full provenance
— not a marketing claim. For context on the same dataset, mcp-memory-service
reports 0.804 R@5 and agentmemory 0.967 R@5 (hybrid + reranking); ai-memory is
comparable to the former and honestly below the latter, and it documents *why*
(the pipeline pays a real 2 KB privacy-capture cost the raw-log retrievers do
not). See [where we're behind](#where-were-behind-or-different-by-choice).

## By camp

| Approach | Representatives | Strength | Trade-off vs ai-memory |
|---|---|---|---|
| Fact extractors | Mem0, LangMem | Cheap per-turn personalization | Atomic facts lose relational/causal context (see [TriMem](research-2026-landscape.md#4-research-developments-worth-knowing)); LLM-per-turn; not file-first |
| Hosted memory API (hybrid) | **Supermemory**, **LiquidLM** | Chunk-RAG + LLM temporal fact-graph + per-user profiles in one query; managed connectors (Drive/Notion/GitHub), multimodal, metadata/tag query filters | Cloud-first (best features + extraction are paid/hosted); LLM-required quality path; an opaque store is the source of truth (not file-first, nothing to `grep`/diff). Supermemory's MIT self-host binary drops the connectors + extraction models; LiquidLM is closed-source and cloud-only (no self-host at all) |
| User-modeling / theory-of-mind | **Honcho** (Plastic Labs) | Reasoning-derived model of what each "peer" knows/believes over time — personalizes around the *human* the agent serves | Different problem: it remembers the *user*, ai-memory remembers the *project*. Opaque Postgres, LLM-required (Deriver/Dreamer), multi-service stack; adjacent (shares MCP/plugin delivery) but not a coding-memory migration target. ai-memory borrowed the *conveniences*, not the engine: an opt-in dialectic **answer** over its own hits and a **reasoning** tier now ship (both off by default, LLM-required) |
| Temporal knowledge graph | Zep/Graphiti, Cognee | Bi-temporal "what was true vs believed when" | Needs a graph DB; heavier to self-host. ai-memory ships **bi-temporal-lite** on SQLite ([`temporal.md`](temporal.md)) + typed edges ([`typed-edges.md`](typed-edges.md)) for the useful part |
| Memory OS / self-editing | Letta, MemOS, MIRIX | Agent curates its own tiered memory | Token-expensive self-editing; Letta itself now concedes file-first ("Is a Filesystem All You Need?") |
| Hosted context database | **OpenViking** (ByteDance) | Progressive L0/L1/L2 loading; directory-scoped retrieval; broad integrations | LLM-**required** (VLM + embeddings); opaque swappable storage; AGPLv3 core + SaaS/enterprise weight |
| Paper-backed pages | **Hindsight** (Vectorize) | "Mental models" = living markdown pages an agent boots from; belief-strength consolidation; a preprint | Postgres/pgvector-primary, LLM-required; strict per-bank isolation (no team-sharing within a project). ai-memory now ships belief-strength `confidence` + an opt-in LLM "dream" rewrite, both zero-LLM-default-preserving, off by default, and R2-gated before default-on |
| Closest sibling (fact-row twin) | doobidoo/mcp-memory-service | SQLite(+vec), local ONNX, hook capture, typed edges, honest numbers | What ai-memory would be if it chose fact-rows over wiki **pages** |
| Platform-native | Claude Code auto-memory | Zero setup, on by default | Machine-local, **no sync**, single-agent, repo-scoped, no tool-lifecycle capture, no team |
| **File-first wiki (ai-memory)** | ai-memory, basic-memory, OKF | Human-editable markdown truth + derived index; cross-agent; zero-LLM default; multi-user | Below the reranking leaders on raw R@5; LLM-optional means no VLM fact-extraction sophistication |
| File-first wiki (LLM-leaning) | **memU**, **EverOS** | Markdown source of truth + derived vector/SQLite(+LanceDB) indexes; offline consolidation; skill distillation from session history | LLM-required for the core capture/distill flow (no zero-LLM default); Python multi-service + a hosted-cloud option, vs one self-contained zero-LLM binary; memU patches host instruction files rather than exposing a git-diffable wiki |
| Self-hosted coding-agent store | **Engram** | Single Go binary, SQLite+FTS5, MCP stdio, project-scoped, `mem_session_summary` handoffs, git-sync across machines | Structured rows the agent saves through MCP tools (SQLite is the truth, git-syncs compressed chunks), not a hand-editable git-markdown wiki captured automatically from lifecycle hooks |
| Governed multi-agent / fleet memory | **Caura** | Shared fleet memory: agent/team/org visibility scopes, four trust tiers, audit log, PII flagging, contradiction/supersession, auto knowledge graph | Fleet-governance-first on Postgres+pgvector (LLM extraction), hosted option; ai-memory optimizes one project's git-backed wiki with `pages shared, batons owned`, not org-scale fleet governance |
| Shared memory server (proxy fan-in) | **TencentDB Agent Memory** | One proxy in front of many harnesses distills chats/docs/code into Chat Memory / Skill / Wiki / CodeGraph assets | Proxy base-URL interception, not MCP or OS lifecycle hooks; despite the name it needs no Tencent DB product (SQLite by default); a Node three-service stack vs one file-first binary |

## Maturity and maintenance

This is a crowded, fast-moving field, and it is only fair to say so: **every tool
compared here is actively maintained** (as of 2026-09-18, all had commits within
the last ~10 days — none stale, none archived; the five new-entrant rows below
were verified 2026-09-22). Raw GitHub popularity, though,
tracks funding and app-developer reach more than coding-agent fitness — the
star leaders are the app-personalization and hosted-context players (a different
buyer), while the tools closest to ai-memory's file-first, self-hosted,
coding-continuity niche are smaller by design.

| Project | Stars (~) | Latest release | Maintenance |
|---|---|---|---|
| Mem0 | 65.6k | 2026-09-18 | active |
| OpenViking | 38.0k | 2026-09-14 | active |
| Zep/Graphiti | 31.0k | 2026-09-08 | active |
| Cognee | 30.8k | 2026-09-15 | active |
| Supermemory | 30.1k | 2026-08-17 | active |
| agentmemory | 28.6k | 2026-08-16 | active |
| TencentDB Agent Memory | 27.2k | v2.0.1 (2026-08-25) | active |
| Letta | 24.8k | 2026-05-14 | active (releases lag code) |
| Hindsight | 23.9k | 2026-09-14 | active |
| memU | 14.4k | v1.5.1 (2026-03-23) | active (releases lag code) |
| EverOS | 13.1k | v1.3.1 (2026-09-08) | active |
| Honcho | 7.2k | tag v3.2.0 | active |
| Engram | 6.8k | v2.0.0 (2026-09-18) | active |
| basic-memory | 4.0k | 2026-08-25 | active |
| mcp-memory-service | 2.0k | 2026-09-14 | active |
| LangMem | 1.7k | PyPI-only | active |
| Caura | 0.5k | backend-v3.17.2 (2026-09-19) | active |
| LiquidLM | closed-source | `@liquidlm/cli` 0.1.5 (2026-09-17) | active (young, solo) |

Full figures, sources, and per-tool caveats: [`research-2026-landscape.md`](research-2026-landscape.md#popularity-and-maintenance-signal-as-of-2026-09-18).

## How the field validates the approach

The strongest endorsement of ai-memory's design is that others arrived at its
core bets independently, from different substrates:

- **Google standardized file-first.** The [Open Knowledge Format](okf.md) (June
  2026) — markdown + YAML frontmatter, one concept per file, no runtime — is
  the interop layer for agent memory. ai-memory's wiki is a native OKF bundle.
- **Letta conceded the filesystem.** The "memory OS" camp's own leader
  published that agents post-trained for file search rival specialized memory
  systems — the file-as-truth + derived-index split ai-memory uses.
- **Hindsight's paper validates pages-over-facts.** A funded competitor on the
  *opposite* (Postgres, LLM-required) substrate makes its top tier "mental
  models" — living markdown pages a background process rewrites and the agent
  boots from. That is ai-memory's `wiki/` + [auto-improve loop](auto-improvement-loop.md)
  under another name, backed by arXiv:2512.12818.
- **OpenViking validates document memory + a consolidation loop.** ByteDance's
  entrant stores document/directory-scoped memory (not atomic facts) with a
  background extract/merge pass and a "compile" step into wikis — the same
  shape as ai-memory's consolidation, from an LLM-required corner.
- **The literature backs it.** TriMem (arXiv:2605.19952) argues document/page/
  narrative hierarchies beat atomic-fact stores; the Storage→Reflection→
  Experience survey (arXiv:2605.06716) names cross-trajectory abstraction as
  the frontier — which ai-memory's [experience pass](experience.md) targets.

ai-memory also **shipped the borrowable ideas** from that research rather than
just cataloguing them: typed relation edges, ingestion-time temporal validity
with `as_of` queries, local (no-key) embeddings as the default, and the
cross-session abstraction pass are all in the product today. The 2.4 line added
five retrieval conveniences, each **opt-in with defaults byte-identical**: a
bounded related-pages graph walk (`memory_read_page include_related`,
basic-memory/LiquidLM), an opt-in dialectic **answer** over hits and a
**reasoning** tier (Honcho, both LLM-required and off by default), a
**pin-before-search** contract (`memory_query pin_first` + a pinned
standing-context briefing, LiquidLM), and a **show-superseded** retrieval knob
over the supersession chains.

The 2.4 line also closed most of the **memory-aging** gap — the decay,
compression, and consolidation machinery the field converged on — on
ai-memory's own zero-LLM, file-first terms (design of record:
[`design-memory-aging.md`](design-memory-aging.md)):

- **Per-tier decay curves** (mcp-memory-service's 365/180/90/30 shape): the
  single global half-life is now a tunable per-tier `[decay.half_life_days]`
  table. Identity default — an upgrade changes no score.
- **Extractive tier-down of cold episodic pages** (agentmemory and
  mcp-memory-service ship extractive, zero-LLM compression as a default):
  instead of evicting a cold page, ai-memory can compact it to its abstract,
  a summary, and a regex-mined keep-token set (paths, URLs, code spans, error
  codes) and drop the prose — reversible through git + the supersession chain.
- **Cold-cluster dedup** (mcp-memory-service's DBSCAN): near-duplicate cold
  episodic pages cluster (adaptive-eps DBSCAN over already-stored embeddings)
  and collapse to one survivor — superseded, never deleted.
- **Zero-LLM contradiction flagging** (mcp-memory-service's 0.4–0.75
  cosine band): `memory_lint` now surfaces likely-conflicting pages, advisory
  only, from existing embeddings.
- **Access-weighted retention** (mcp-memory-service's access boost): a page a
  human opens, searches, or reaches through the link graph resists decay like
  a search hit — now on *every* read path.
- **Belief-strength confidence** (Hindsight, mcp-memory-service): a read-time,
  zero-LLM `confidence` per page (distinct-session breadth, recency, live
  `contradicts` count), surfaced in `memory_query explain` and `memory_status`.
- **An opt-in LLM "dream" pass** (Hindsight's Dreamer, Honcho's Dreamer,
  Supermemory's "dreaming"): idle-scheduled, cancel-on-activity,
  surprisal-first cross-session rewrite/merge of cold clusters.

Two of these change behavior only where a provider is configured and the
operator opts in — belief-strength folded into ranking, and the dream pass —
and both are **off by default and gated on a recall eval (R2) before they may
default on**. No such eval has been run yet, so ai-memory claims *parity of
mechanism, not a measured quality win*: the substrate ships, honestly, off.
Everything else here is zero-LLM, reversible, and off by default until an
operator turns it on.

## Coming from another tool?

- **From Mem0 / a fact extractor:** you keep automatic capture, but memory
  compiles into readable **pages** you can open and edit, not opaque fact rows.
  Retrieval fuses FTS + entity + graph + vectors instead of vector-only.
- **From Zep/Graphiti:** you get bi-temporal-lite (`as_of`, version-filtered
  search) and typed edges without standing up a graph database — on one binary.
- **From Claude Code's built-in memory:** the same "remember my project"
  convenience, but synced across machines and agents, searchable, team-capable,
  and capturing tool lifecycle — not a per-laptop `MEMORY.md`.
- **From mcp-memory-service:** a very close sibling; the switch is fact-rows →
  wiki pages (human-editable markdown truth) and cross-agent handoffs as a
  first-class protocol. Cross-project [agent messaging](agent-messaging.md) is
  new ground neither had as a typed queue. On aging you keep what you relied on
  — the 2.4 line matches its per-tier decay curves, extractive compression,
  DBSCAN cold-cluster dedup, access boosts, and 0.4–0.75-band contradiction
  detection — but done **zero-LLM by default, reversibly** (every collapse
  supersedes rather than deletes; `restore-page` recovers the original), and
  **off by default** so an upgrade evicts nothing.
- **From Supermemory / LiquidLM (a hosted memory API):** you trade a cloud
  vault and a managed multimodal RAG service for a self-contained binary whose
  memory lives in git-versioned markdown you own, works zero-LLM by default, and
  captures your coding sessions automatically through lifecycle hooks instead of
  explicit uploads. You give up (for now) their multimodal ingestion
  (video/audio/PDF/Office), a polished consumer web app + grounded chat, and
  managed hosting; you gain data ownership, no required API spend, offline
  operation, per-project team sharing, and — now opt-in on the 2.4 line — a
  pin-before-search contract and a bounded related-pages graph walk over the link
  neighbours ai-memory already computes. Different job: they build a general
  "second brain," ai-memory remembers *this repo*.
- **From Hindsight / OpenViking:** you trade a hosted, LLM-required service for
  a self-contained binary that runs zero-LLM by default and keeps memory in
  files you own. The consolidation shape you came for is here on 2.4 — a
  belief-strength `confidence` over evidence and an idle-scheduled,
  cancel-on-activity, surprisal-first LLM "dream" rewrite of cold clusters —
  but as **opt-in layers that never delete a source** (the pre-merge versions
  stay reachable) and are **off by default and gated on a recall eval before
  default-on**, over a zero-LLM core, rather than a mandatory loop. OpenViking's
  L0/L1/L2 progressive tiers map onto ai-memory's extractive tier-down
  (abstract + summary + keep-tokens). You give up (for now) their VLM-driven
  extraction depth and their published headline accuracy numbers; you gain no
  vendor lock-in, no required API spend, and per-project team sharing rather
  than strict per-bank isolation.
- **From Engram (self-hosted coding-agent store):** the closest operational
  sibling among the newcomers — a single self-hosted binary, SQLite+FTS5, MCP,
  project scope, and session-summary handoffs. The switch is Engram's
  agent-saved structured rows (the agent calls `mem_*` MCP tools; SQLite is the
  truth, git-syncing compressed chunks) → ai-memory's automatic lifecycle-hook
  capture compiled into a hand-editable git-markdown **wiki**, plus typed
  claim-once handoffs, cross-project messaging, and multi-user page sharing. You
  keep a keyless, offline, single-binary posture; you give up (for now) Engram's
  managed Cloud replication add-on.
- **From memU / EverOS (file-first, LLM-leaning):** you keep markdown as the
  source of truth, but ai-memory's core capture, retrieval, and summaries run
  **zero-LLM by default** rather than requiring a provider for the capture/
  distill flow, and ship as one binary instead of a Python multi-service stack.
  memU's skill-distillation and EverOS's offline consolidation map onto
  ai-memory's [experience pass](experience.md) + auto-improve loop; EverOS's
  SQLite+LanceDB derived indexes map onto ai-memory's derived SQLite/FTS5(+local
  embeddings). You give up (for now) memU's turnkey skill-distillation packaging
  and hosted cloud, and EverOS's LanceDB vector tier.
- **From Caura (governed multi-agent fleet memory):** different buyer. Caura
  optimizes org-scale fleet governance — agent/team/org visibility scopes, four
  trust tiers, an audit log, PII flagging — on Postgres+pgvector with LLM
  extraction. ai-memory shares the multi-agent/multi-user goal but at
  *project* scope (`pages shared, batons owned`, invariant #16) with a
  self-hosted auth ladder and audit log, not fleet-wide trust tiers. If you need
  cross-fleet trust governance across hundreds of agents, that is Caura's lane,
  not ours; if you want file-first, zero-LLM, single-binary project memory, it is
  ours. (Caura's trust-tier governance is an honest capability we do not match —
  see [`competitive-parity.md`](competitive-parity.md).)
- **From TencentDB Agent Memory (proxy fan-in server):** you trade a Node
  three-service proxy that intercepts each harness's API base URL (distilling
  Chat Memory / Skill / Wiki / CodeGraph assets) for a file-first binary that
  captures through OS lifecycle hooks and stores git-versioned markdown you own.
  Despite the name, TencentDB Agent Memory needs no Tencent database (SQLite by
  default). You give up its zero-code proxy integration and its CodeGraph code
  index (an adjacent, codebase-intelligence feature ai-memory does not do); you
  gain hook capture, a git-markdown wiki, typed handoffs, and zero-LLM operation.

## Where we're behind, or different by choice

Fair means saying this plainly:

- **Raw retrieval score, and no local reranker.** 0.823 hit@5 on LongMemEval-S
  is comparable to mcp-memory-service and below agentmemory's 0.967 (hybrid +
  reranking). Part is deliberate — a 2 KB privacy cap on captured excerpts puts
  evidence deep inside one long turn out of the index's reach; the benchmark
  measures the *shipped, sanitized* system, not an idealized retriever. The only
  reranker is LLM-as-judge, so the zero-LLM default path has none; the opt-in
  `answer=true` dialectic synthesis (2.4) is likewise LLM-required, off by
  default, and its end-to-end QA-accuracy is early and small-sample (see
  [`benchmarks/retrieval-ab-r2.md`](benchmarks/retrieval-ab-r2.md)), not a
  headline claim.
- **Belief-strength and the dream pass ship, but off — no proven win yet.**
  The 2.4 memory-aging set includes the two pieces that could move retrieval
  *quality* — folding belief-strength `confidence` into ranking authority, and
  the LLM "dream" rewrite of cold clusters — but both are **off by default and
  gated on a recall eval (R2) that has not been run**. We claim parity of
  *mechanism* with Hindsight/mcp-memory-service here, not a measured recall or
  QA improvement; until R2 shows a positive delta the honest statement is
  "shipped, opt-in, unproven," and the default path is unchanged.
- **Headline benchmark comparability.** Hindsight quotes 91.4% *accuracy* and
  OpenViking quotes LoCoMo lifts — different datasets/metrics than our hit@5,
  and both are self-reported/preprint. We publish a reproducible harness and a
  single stated metric rather than a bigger number.
- **No VLM fact extraction.** Because the default path is zero-LLM, ai-memory
  does not do the LLM-per-turn atomic extraction the fact-extractor and
  LLM-required systems build on. Consolidation is opt-in and page-shaped.
- **Not a graph database.** ai-memory chooses bi-temporal-lite on SQLite over a
  full temporal knowledge graph — the useful 80%, not the graph-query surface.
- **Single server, not SaaS.** No hosted multi-region tier, no enterprise
  console. That is the point (own your data, one binary), but it is a
  difference if you want managed infrastructure.

If a specific comparison here reads as unfair or out of date, open an issue —
these numbers and claims are meant to be checkable against the linked research
and the reproducible harness.
