# How ai-memory compares

This page is for anyone evaluating ai-memory against another agent-memory tool,
or migrating from one. It aims to be **fair and specific**: what each
approach does well, where ai-memory differs, where others are ahead, and where
other projects reached the same design choices independently. The detailed
analysis behind it is in [`research-2026-landscape.md`](research-2026-landscape.md)
and the per-project research docs; the numbers are from
[`benchmarks/`](benchmarks/README.md), reproducible from the in-repo harness.


## The short version

Most memory tools optimize one of: extracting atomic **facts** (Mem0,
LangMem), a temporal **knowledge graph** (Zep/Graphiti, Cognee), an
agent-editable **memory OS** (Letta, MemOS), a **context database**
(OpenViking), or **hook capture for coding agents** (claude-mem, MemPalace,
agentmemory, and the coding plugins of most of the above). ai-memory keeps
**a git-backed markdown wiki as the source of truth, with a derived index
for retrieval**, captured automatically from lifecycle hooks and shared
across agents and machines.

Coding-agent memory is now the most-starred part of this field, and two
choices that used to set ai-memory apart are now common: several tools keep
memory as git-backed or editable markdown (Letta, Supermemory's memoryrepo,
OpenViking, EverOS), and hook capture is standard in the coding plugins.
What still sets ai-memory apart:

- **Zero-LLM by default.** Capture, FTS5 + entity + graph retrieval, local
  embeddings, and rule-based summaries all work with **no API key**. An LLM is
  opt-in for richer consolidation; nothing requires one.
- **Git-versioned markdown is the source of truth, not a mirror.** The wiki
  is plain markdown + YAML frontmatter, a native
  [Open Knowledge Format](okf.md) (OKF v0.2) bundle. You can `grep` it, open
  it in Obsidian, edit it by hand, or `rsync` it. The SQLite index is derived
  and rebuildable.
- **Claim-once handoffs and typed cross-project messaging.** Quit one agent,
  open another in the same repo, and you get a handoff that is owned and
  claimed exactly once. Projects can message each
  other through a typed queue ([agent messaging](agent-messaging.md)).
- **Per-project multi-user auth on one self-hosted binary.** An auth ladder
  (root → DB-user tokens → OIDC), per-person attribution, an audit log, and
  the invariant that **pages are shared per project while handoffs stay
  owned**. Bundled SQLite and vendored libgit2 mean no external services; it
  runs on a laptop, a homelab box, or a LAN server, with no paid tier.
- **Capture under the harness you already use.** One server; 20+ harnesses
  (Claude Code, Codex, Cursor, Gemini, OpenCode, Kimi, …) feed and read the
  same memory. Replication between servers is not automatic; in 3.0
  the `ai-memory-wikisync` companion adds two-way,
  conflict-aware sync between a server's wiki and a git repository.

It also publishes retrieval numbers, produced by the in-repo harness with full
provenance and measured end to end through the shipped capture pipeline:
LongMemEval-S hit@5 **0.666** zero-LLM and **0.815** with the default local
embeddings in the published 2.6 line. The observation stream in 3.0
(on by default) raises zero-LLM to **0.951** and local embeddings
to **0.945** on all 470 questions, measured on one build with the stream off
and on. The 3.3.0 release run measures **0.951** zero-LLM and **0.945** local
at hit@5 ([report](benchmarks/longmemeval-s-3.3.0-release.md)); its local
top-ten movement is published and tracked in #1854. With capture skipped and
each raw session indexed whole (component
mode, ranking only), zero-LLM FTS scores 0.972; the
[decomposition report](benchmarks/longmemeval-s-decomposition-2026-10-12.md)
explains the difference.

Other tools publish numbers of different kinds. MemPalace's 96.6% R@5 (raw)
and 98.4% (450 held-out questions) are retrieval-only, the closest in kind
to our hit@5, though from a different harness. agentmemory publishes 95.2%
`recall_any@5` from a component harness (whole raw sessions, reranker
disabled), and mcp-memory-service 0.804 R@5. Mem0, Zep, Hindsight and EverOS
publish end-to-end QA accuracy, where an LLM reads the retrieved context and
an LLM judges the answer, with their own reader and judge models and context
budgets. None of these is like-for-like with ours or with each other, so this
page claims no ranking; the table under
[where we're behind](#where-were-behind-or-different-by-choice) lists each
setup.

## By camp

| Approach | Representatives | Strength | Trade-off vs ai-memory |
|---|---|---|---|
| Coding-agent hook capture | **claude-mem**, **MemPalace**, agentmemory | Lifecycle hooks tuned for Claude Code and Codex; large user bases; claude-mem shows past observations for a file when the agent reads it and answers native memory-folder reads from its store; MemPalace has a team server and temporal graph | claude-mem runs an LLM "observer" over every observation (SQLite FTS5 + Chroma) and has a paid cloud tier; agentmemory keeps state in iii-engine's KV store (file-backed or Redis) with a one-way Obsidian export; ai-memory adds claim-once handoffs and typed cross-project messaging |
| Fact extractors | Mem0, LangMem | Cheap personalization; Mem0 retrieval fuses semantic + BM25 + entity matches, and it ships native coding plugins for seven agents with repo, subdirectory and per-user scopes | Atomic facts lose relational/causal context (see [TriMem](research-2026-landscape.md#4-research-developments-worth-knowing)); LLM extraction; not file-first; Mem0's coding plugins need a Mem0 cloud API key and send telemetry under the account email |
| Hosted memory API (hybrid) | **Supermemory**, **LiquidLM** | Chunk-RAG + LLM temporal fact-graph + per-user profiles in one query; managed connectors (Drive/Notion/GitHub), multimodal, metadata/tag query filters; Supermemory's Claude Code and Codex plugins capture through lifecycle hooks and inject matches on every prompt | Cloud-first (best features + extraction are paid/hosted); LLM-required quality path; the default store is opaque. Supermemory's self-host server binary is not open source (free within a lite licence), lacks its MCP server, and has telemetry on by default; LiquidLM is closed-source and cloud-only |
| User-modeling / theory-of-mind | **Honcho** (Plastic Labs) | Reasoning-derived model of what each "peer" knows/believes over time; personalizes around the *human* the agent serves | Different problem: it remembers the *user*, ai-memory remembers the *project*. Opaque Postgres, LLM-required (Deriver/Dreamer), multi-service stack; adjacent (shares MCP/plugin delivery) but not a coding-memory migration target. ai-memory borrowed the *conveniences*, not the engine: an opt-in dialectic **answer** over its own hits and a **reasoning** tier now ship (both off by default, LLM-required) |
| Temporal knowledge graph | Zep/Graphiti, Cognee | Bi-temporal "what was true vs believed when"; Cognee adds data connectors (Drive, Gmail, Slack), a repo code-graph indexer, and a Claude Code plugin listed in Anthropic's plugin directory | Graphiti needs a graph database (Neo4j, FalkorDB, or the embedded FalkorDBLite); Cognee 1.6 runs on embedded SQLite + LanceDB + Kuzu files and can start without an LLM key. In both, memory lives in database files, not git-versioned markdown. ai-memory ships **bi-temporal-lite** on SQLite ([`temporal.md`](temporal.md)) + typed edges ([`typed-edges.md`](typed-edges.md)) for the useful part |
| Memory OS / self-editing | Letta (now Letta Code), MemOS, MIRIX | Agent curates its own tiered memory; Letta's memory is a git repo of markdown files with a `MEMORY.md` index, written by "dreaming" subagents through git worktrees | Token-expensive self-editing; Letta's V1 server is retired and its memory lives inside its own harness (Letta Code, Letta Cloud by default), not under the harness you already use |
| Context database | **OpenViking** (ByteDance) | Progressive L0/L1/L2 loading; global recall + rerank with subtree filters; editable markdown memories; plugins for seven coding agents; a v0.5 gateway proxy that injects recall and keeps prompt caches valid | Needs a VLM (Ollama works); embeddings are local by default; AGPLv3 self-hostable server; its `ov-kanban` task handoff has no claim-once guard |
| Paper-backed pages | **Hindsight** (Vectorize) | "Mental models" = living markdown pages an agent boots from; belief-strength consolidation; a preprint; a coding-agent package that hooks 16+ agents, ingests git history, and shares one bank per repo; reranker on by default | Postgres/pgvector-primary; needs an LLM (local via Ollama/llama.cpp, or a Claude/ChatGPT/Copilot subscription); markdown is a one-way mirror of the database. ai-memory ships belief-strength `confidence` + an opt-in LLM "dream" rewrite, both zero-LLM-default-preserving, off by default, and gated on a recall eval before default-on |
| Fact-row store with hook capture | doobidoo/mcp-memory-service | SQLite(+vec), local ONNX, hook capture, typed edges, published numbers | Stores fact rows where ai-memory stores wiki **pages** |
| Platform-native | Claude Code auto-memory | Zero setup, on by default | Machine-local, **no sync**, single-agent, repo-scoped, no tool-lifecycle capture, no team |
| **File-first wiki (ai-memory)** | ai-memory, basic-memory, OKF | Human-editable markdown truth + derived index; cross-agent; zero-LLM default; multi-user | No local reranker yet; LLM-optional means no VLM fact-extraction sophistication |
| File-first wiki (LLM-leaning) | **memU**, **EverOS** | Markdown source of truth + derived vector/SQLite(+LanceDB) indexes; offline consolidation; skill distillation from session history; EverOS ships an MCP server and published benchmarks; memU auto-detects unknown agents | LLM-required for the core capture/distill flow (no zero-LLM default); Python stacks + a hosted-cloud option, vs one self-contained zero-LLM binary; memU has no MCP server and patches host instruction files rather than exposing a git-diffable wiki |
| Self-hosted coding-agent store | **Engram** | Single Go binary, SQLite+FTS5, MCP stdio, project-scoped, `mem_session_summary` handoffs, git-sync across machines | Structured rows the agent saves through MCP tools (SQLite is the truth, git-syncs compressed chunks), not a hand-editable git-markdown wiki captured automatically from lifecycle hooks |
| Governed multi-agent / fleet memory | **Caura** | Shared fleet memory: agent/team/org visibility scopes, four trust tiers, audit log, PII flagging, contradiction/supersession, auto knowledge graph | Fleet-governance-first on Postgres+pgvector (LLM extraction), hosted option; ai-memory optimizes one project's git-backed wiki with `pages shared, batons owned`, not org-scale fleet governance |
| Shared memory server (proxy fan-in) | **TencentDB Agent Memory** | One proxy in front of many harnesses distills chats/docs/code into Chat Memory / Skill / Wiki / CodeGraph assets; v2 adds per-asset access control (private / team / restricted / agent) | Proxy base-URL interception, not MCP or OS lifecycle hooks; the v2 hub needs an LLM; despite the name it needs no Tencent DB product (SQLite by default); a Node three-service stack vs one file-first binary |

## Maturity and maintenance

This is a crowded, fast-moving field, and nearly every tool compared here is
actively maintained. As of 2026-10-18, the tools re-checked that day had
recent October work except for the quiet projects named below. Letta's
original server is retired (its work continues in Letta Code), but its GitHub
repository is an active redirect with the old source on an `archive` branch.
memvid has had no commits since 2026-07-14. Coding-agent memory sits at the top of the star chart:
claude-mem leads, MemPalace is third, and most of the app-memory leaders
(Mem0, Hindsight, OpenViking, Supermemory, Cognee) ship coding-agent plugins.

| Project | Stars (~) | Latest release | Maintenance |
|---|---|---|---|
| claude-mem | 101.6k | v14.0.1 (2026-10-17) | active |
| Mem0 | 67.4k | core v2.2.1 (2026-09-25) | active |
| MemPalace | 59.6k | v3.11.0 (2026-10-14) | active |
| Hindsight | 49.1k | v1.0.0 (2026-10-13) | active |
| OpenViking | 40.3k | v0.5.0 (2026-10-09) | active |
| Cognee | 32.5k | v1.6.5 (2026-10-16) | active |
| Zep/Graphiti | 32.0k | v0.31.1 (2026-10-15) | active |
| Supermemory | 31.3k | server-v0.0.9 (2026-10-12) | active |
| agentmemory | 29.7k | v0.9.30 (2026-10-06) | active |
| TencentDB Agent Memory | 29.6k | v2.0.3-beta.1 (2026-10-13) | active |
| Letta (V1 server) | 25.2k | 0.16.8, frozen | retired; source on `archive` branch |
| Letta Code | 3.7k | v0.34.16 (2026-10-16) | active |
| Memori | 17.2k | v3.3.6 (2026-05-28) | quiet since 2026-10-03 |
| memvid | 16.6k | v2.0.140 (2026-05-27) | no commits since 2026-07-14 |
| memU | 14.6k | PyPI `memu-cli` 0.10.0 stable | active |
| EverOS | 13.5k | v1.4.2 (2026-10-14) | active |
| MemOS | 11.9k | v2.0.34 (2026-09-23) | active; no change after cutoff |
| Honcho | 7.7k | v3.3.0 (2026-10-14) | active |
| Engram | 7.4k | v3.4.0 (2026-10-12) | active |
| basic-memory | 4.3k | v0.23.2; v0.24 unreleased | active |
| mcp-memory-service | 2.0k | v12.1.1 (2026-10-16) | active |
| LangMem | 1.7k | PyPI 0.0.30 | quiet |
| Caura | 0.5k | backend-v3.34.5 (2026-10-18) | active |
| LiquidLM | closed-source | `@liquidlm/cli` 0.1.6 stable | active |

Adjacent tools, not memory systems: codebase-memory-mcp (~46.5k, a code
graph), qmd (~30.4k, markdown search), context-mode (~27.9k, survives
compaction with a hook-fed per-project SQLite FTS5 store) and beads (~27.8k,
an issue tracker with `bd remember`).

Full figures, sources, and per-tool caveats: [`research-2026-landscape.md`](research-2026-landscape.md#popularity-and-maintenance-signal-as-of-2026-10-18).

## How the field validates the approach

Other projects, built on different substrates, arrived at ai-memory's core
design choices independently:

- **Google standardized file-first.** The [Open Knowledge Format](okf.md) (June
  2026), markdown + YAML frontmatter with one concept per file and no runtime, is
  the interop layer for agent memory. ai-memory's wiki is a native OKF bundle.
- **Letta moved its memory into git-backed markdown.** Since February 2026
  ("context repositories") every Letta agent's memory is a git repository of
  markdown files with YAML frontmatter and a `MEMORY.md` index, with no vector
  index by default. That is ai-memory's substrate, inside Letta's own harness.
- **Hindsight's paper supports pages over facts.** A competitor on a
  different substrate (Postgres, LLM in the loop) makes its top tier "mental
  models": living markdown pages a background process rewrites and the agent
  boots from. That is ai-memory's `wiki/` + [auto-improve loop](auto-improvement-loop.md)
  under another name, backed by arXiv:2512.12818.
- **OpenViking uses document memory + a consolidation loop.** ByteDance's
  entrant stores document/directory-scoped memory as editable markdown (not
  atomic facts) with a background extract/merge pass and a "compile" step into
  wikis. That is the same shape as ai-memory's consolidation, from a
  VLM-required starting point.
- **Hook capture became standard.** claude-mem, MemPalace, Mem0, Supermemory,
  Hindsight, OpenViking and Cognee all capture coding sessions through
  harness lifecycle hooks now.
- **The literature backs it.** TriMem (arXiv:2605.19952) argues document/page/
  narrative hierarchies beat atomic-fact stores; the Storage→Reflection→
  Experience survey (arXiv:2605.06716) names cross-trajectory abstraction as
  the frontier, which ai-memory's [experience pass](experience.md) targets.

ai-memory also **shipped the borrowable ideas** from that research instead of
only cataloguing them: typed relation edges, ingestion-time temporal validity
with `as_of` queries, local (no-key) embeddings as the default, and the
cross-session abstraction pass are all in the product today. The 2.4 line added
five retrieval conveniences, each **opt-in with defaults byte-identical**: a
bounded related-pages graph walk (`memory_read_page include_related`,
basic-memory/LiquidLM), an opt-in dialectic **answer** over hits and a
**reasoning** tier (Honcho, both LLM-required and off by default), a
**pin-before-search** contract (`memory_query pin_first` + a pinned
standing-context briefing, LiquidLM), and a **show-superseded** retrieval knob
over the supersession chains.

The 2.4 line also closed most of the **memory-aging** gap (the decay,
compression, and consolidation machinery the field converged on) on
ai-memory's own zero-LLM, file-first terms (design of record:
[`design-memory-aging.md`](design-memory-aging.md)):

- **Per-tier decay curves** (mcp-memory-service's 365/180/90/30 shape): the
  single global half-life is now a tunable per-tier `[decay.half_life_days]`
  table. The default is the identity, so an upgrade changes no score.
- **Extractive tier-down of cold episodic pages** (agentmemory and
  mcp-memory-service ship extractive, zero-LLM compression as a default):
  instead of evicting a cold page, ai-memory can compact it to its abstract,
  a summary, and a regex-mined keep-token set (paths, URLs, code spans, error
  codes) and drop the prose. This is reversible through git + the supersession chain.
- **Cold-cluster dedup** (mcp-memory-service's DBSCAN): near-duplicate cold
  episodic pages cluster (adaptive-eps DBSCAN over already-stored embeddings)
  and collapse to one survivor. The others are superseded, never deleted.
- **Zero-LLM contradiction flagging** (mcp-memory-service's 0.4–0.75
  cosine band): `memory_lint` now surfaces likely-conflicting pages, advisory
  only, from existing embeddings.
- **Access-weighted retention** (mcp-memory-service's access boost): a page a
  human opens, searches, or reaches through the link graph resists decay like
  a search hit, now on *every* read path.
- **Belief-strength confidence** (Hindsight, mcp-memory-service): a read-time,
  zero-LLM `confidence` per page (distinct-session breadth, recency, live
  `contradicts` count), surfaced in `memory_query explain` and `memory_status`.
- **An opt-in LLM "dream" pass** (Hindsight's Dreamer, Honcho's Dreamer,
  Supermemory's "dreaming"): idle-scheduled, cancel-on-activity,
  surprisal-first cross-session rewrite/merge of cold clusters.

Two of these, belief-strength folded into ranking and the dream pass, change
behavior only where a provider is configured and the operator opts in,
and both are **off by default and gated on a recall eval before they may
default on**. No eval has shown a gain yet (the zero-LLM benchmark corpus
has no consolidation evidence for belief-strength to act on), so ai-memory
claims *parity of mechanism, not a measured quality win*: the code ships,
switched off.
Everything else here is zero-LLM, reversible, and off by default until an
operator turns it on.

## Coming from another tool?

Each entry lists what you gain by moving and what you give up. Where a tool
ships something ai-memory does not, it is named.

- **From claude-mem:** you keep automatic hook capture and progressive
  search. You gain zero-LLM capture (claude-mem runs an LLM "observer" over
  every observation), git-versioned markdown as the source of truth instead
  of SQLite + Chroma, the same memory under 20+ harnesses, claim-once
  handoffs, cross-project messaging, and per-project multi-user auth without
  a paid cloud tier. You give up its to-do tools, the bridge that answers
  Claude Code's and Codex's native memory-folder reads from its store, the
  past observations it shows when the agent opens a file, and v14's polished
  local “My Work” dashboard. TeamBrain adds optional paid shared-work and
  collision views; ai-memory shares project memory without that cloud layer
  but has no equivalent human work queue yet. The optional
  `ai-memory-importer claude-mem` command can plan or migrate supported local
  claude-mem SQLite schema families into explicitly mapped existing projects;
  sensitive rows are excluded and live writes use bounded atomic batches.
- **From MemPalace:** you keep hook capture for Claude Code, Codex and Cursor
  and a shared server. You gain git-versioned markdown pages as the source
  of truth, claim-once handoffs, typed cross-project messaging, and the auth
  ladder. You give up its temporal graph. Its 96.6% / 98.4% R@5 and our hit@5
  are both retrieval-only, but from different harnesses, so neither number
  ranks the tools.
- **From agentmemory:** you keep automatic hook capture for coding agents
  and keyless operation by default. You gain markdown in git as the
  source of truth (agentmemory keeps its state in iii-engine's KV store,
  file-backed or Redis, with a one-way Obsidian export), session summaries and
  handoffs without an LLM key (agentmemory's summarize step does nothing
  without one), one process instead of a Node worker plus the iii-engine
  binary, workspace/project isolation with adversarial tests, per-user auth
  with DB tokens and OIDC, claim-once handoffs and cross-project messaging,
  guarded backup/restore/purge, and no runtime dependency under the Elastic
  License (iii-engine's engine is ELv2). You give up editing and forgetting
  memories in a browser (the 3.0 web UI adds page history with diffs, a
  session timeline, a recall trace and a health panel, but stays read-only by
  design), the iii console's OpenTelemetry traces (ai-memory 3.0 has
  per-stream timings in `explain=true` and a root-only Prometheus
  `/admin/metrics` instead), the coordination tools (leases, actions,
  routines, signals, sentinels), mesh sync (the nearest equivalent is the
  git-based `ai-memory-wikisync` companion), image memory, the two-way Claude
  `MEMORY.md` bridge, PaaS deploy templates, an npm package (ai-memory installs
  natively on Linux, macOS and Windows in two commands — `install.sh`/
  `install.ps1` then `ai-memory setup` — but has no `npx` route), and the
  opt-in local cross-encoder reranker. The benchmark numbers are not
  like-for-like (component vs pipeline, see above). To move your data, the
  `ai-memory-importer` companion (3.0) reads agentmemory's Obsidian vault
  (`agentmemory-vault`) or its `ExportData` JSON (`agentmemory-export`), and
  `ai-memory doctor` flags agentmemory MCP entries and rules-file guideline
  blocks left behind; see
  [Coming from agentmemory](cookbook.md#coming-from-agentmemory).
- **From Mem0 / a fact extractor:** you keep automatic capture, but memory
  compiles into readable **pages** you can open and edit, not opaque fact rows.
  Both fuse lexical and semantic retrieval (Mem0 combines semantic, BM25 and
  entity matches; ai-memory fuses FTS + entity + graph + local vectors). You
  gain capture and retrieval with no cloud API key and no telemetry (Mem0's
  coding plugins need a Mem0 cloud key and report under the account email),
  plus handoffs and cross-project messaging. You give up Mem0's native
  plugins for seven coding agents, its failed-command-to-fix capture with a
  `problems_and_fixes` category, its subdirectory search scope, and its
  cloud-only user profiles.
- **From Zep/Graphiti:** you get bi-temporal-lite (`as_of`, version-filtered
  search) and typed edges in one binary, without standing up a graph
  database. You give up true world-time validity, graph queries, custom
  entity/edge types, and hosted Zep's Context MCP server with per-user OIDC
  sign-in and a write kill switch.
- **From Cognee:** you gain git-versioned markdown as the source of truth
  (Cognee keeps memory in SQLite + LanceDB + Kuzu files), the same memory
  under 20+ harnesses, claim-once handoffs, cross-project messaging, and
  per-project multi-user auth. You give up its data connectors (Drive, Gmail,
  Slack), the repo code-graph indexer and statusline in its Claude Code
  plugin, and a listing in Anthropic's official plugin directory.
- **From Letta / Letta Code:** both keep memory as git-versioned markdown
  files. The difference is where memory lives: Letta argues memory belongs
  inside the harness and ships its own (Letta Code, with Letta Cloud as the
  default backend), while ai-memory sits under the harness you already use,
  zero-LLM by default and self-hosted. You give up Letta's "dreaming"
  subagents and an agent that edits its own memory.
- **From Claude Code's built-in memory:** the same "remember my project"
  convenience, but synced across machines and agents, searchable, usable by a
  team, and capturing tool lifecycle, where the built-in memory is a per-laptop `MEMORY.md`.
- **From mcp-memory-service:** the switch is fact-rows →
  wiki pages (human-editable markdown truth) and cross-agent handoffs as a
  first-class protocol. Cross-project [agent messaging](agent-messaging.md) is
  new ground neither had as a typed queue. On aging you keep what you relied on:
  the 2.4 line matches its per-tier decay curves, extractive compression,
  DBSCAN cold-cluster dedup, access boosts, and 0.4–0.75-band contradiction
  detection, but does them **zero-LLM by default, reversibly** (every collapse
  supersedes instead of deleting; `restore-page` recovers the original), and
  **off by default** so an upgrade evicts nothing.
- **From Supermemory / LiquidLM (a hosted memory API):** you trade a cloud
  vault and a managed multimodal RAG service for a self-contained binary whose
  memory lives in git-versioned markdown you own and works zero-LLM by
  default. Supermemory's Claude Code, Codex and OpenCode plugins also capture
  through lifecycle hooks now, sharing one container per repository; its
  isolation is a tag, and its self-hosted server binary is not open source.
  You give up (for now) their multimodal ingestion (video/audio/PDF/Office), a
  polished consumer web app + grounded chat, managed hosting, and
  Supermemory's recall injected on every prompt; you gain data ownership, no
  required API spend, offline operation, claim-once handoffs, per-project
  team sharing, and (opt-in on the 2.4 line) a pin-before-search contract and
  a bounded related-pages graph walk over the link neighbours ai-memory
  already computes. The jobs differ: they build a general "second brain,"
  while ai-memory remembers *this repo*.
- **From Hindsight:** you trade a Postgres-primary service that needs an LLM
  (local or by subscription) for a self-contained binary that runs zero-LLM
  by default and keeps memory in markdown files that are the source of truth,
  not a mirror. The consolidation shape is here on 2.4: a
  belief-strength `confidence` over evidence and an idle-scheduled,
  cancel-on-activity, surprisal-first LLM "dream" rewrite of cold clusters.
  Both are **opt-in layers that never delete a source** (the pre-merge versions
  stay reachable), **off by default and gated on a recall eval before
  default-on**, and sit over a zero-LLM core instead of forming a mandatory
  loop. Both tools share memory across agents in one repository (Hindsight
  through one bank per repo with user/team/company scopes inside it). You
  give up its git-history ingestion, its 16+ agent hook installer, a reranker
  on by default, image and file ingestion, and its published end-to-end QA
  numbers.
- **From OpenViking:** you trade a self-hostable AGPL server that needs a VLM
  for a zero-LLM binary under a permissive licence. OpenViking's L0/L1/L2
  progressive tiers map onto ai-memory's extractive tier-down (abstract +
  summary + keep-tokens) and onto the opt-in session-start brief, which
  gives every standing page an excerpt and adds whole bodies while the
  budget lasts. You gain claim-once handoffs (its `ov-kanban`
  skill passes tasks as markdown files without a claim guard) and per-project
  multi-user auth; you give up its VLM-driven extraction depth, its
  ACLs and user groups, and the v0.5 gateway proxy that injects recall into
  any Anthropic- or OpenAI-compatible client.
- **From Engram (self-hosted coding-agent store):** a single self-hosted
  binary, SQLite+FTS5, MCP, project scope, and session-summary handoffs. The
  switch is Engram's
  agent-saved structured rows (the agent calls `mem_*` MCP tools; SQLite is the
  truth, git-syncing compressed chunks) → ai-memory's automatic lifecycle-hook
  capture compiled into a hand-editable git-markdown **wiki**, plus typed
  claim-once handoffs, cross-project messaging, and multi-user page sharing. You
  keep a keyless, offline, single-binary posture; you give up (for now) Engram's
  managed Cloud replication add-on.
- **From memU / EverOS (file-first, LLM-leaning):** you keep markdown as the
  source of truth, but ai-memory's core capture, retrieval, and summaries run
  **zero-LLM by default** rather than requiring a provider for the capture/
  distill flow, and ship as one binary instead of a Python stack.
  memU's skill-distillation and EverOS's offline consolidation map onto
  ai-memory's [experience pass](experience.md) + auto-improve loop; EverOS's
  SQLite+LanceDB derived indexes map onto ai-memory's derived SQLite/FTS5(+local
  embeddings). You give up (for now) memU's turnkey skill-distillation
  packaging, its `memu-agent detect` for unknown agents, and its hosted cloud,
  and EverOS's ANN vector index, optional Milvus backend and published
  end-to-end benchmark. EverOS v1.4.2 also reserves query-embedding capacity
  from background writes. ai-memory now does the same for in-process local
  inference, backed by an opt-in real-model contention probe; hosted embedding
  providers keep their existing concurrency, and the reranker remains bounded
  and fail-open.
- **From Caura (governed multi-agent fleet memory):** a different buyer. Caura
  optimizes org-scale fleet governance (agent/team/org visibility scopes, four
  trust tiers, an audit log, PII flagging) on Postgres+pgvector with LLM
  extraction. ai-memory shares the multi-agent/multi-user goal but at
  *project* scope (`pages shared, batons owned`, invariant #16) with a
  self-hosted auth ladder and audit log, without fleet-wide trust tiers. If you need
  cross-fleet trust governance across hundreds of agents, use Caura; if you want
  file-first, zero-LLM, single-binary project memory, use ai-memory. (Caura's
  trust-tier governance is a real capability we do not match; see
  [`competitive-parity.md`](competitive-parity.md).) Caura's beta
  collaboration extension also adds live peer discovery, presence, leases,
  correlated replies, and runtime wake helpers. That is fleet orchestration,
  not durable project memory, and external effects remain at-least-once.
- **From TencentDB Agent Memory (proxy fan-in server):** you trade a Node
  three-service proxy that intercepts each harness's API base URL (distilling
  Chat Memory / Skill / Wiki / CodeGraph assets) for a file-first binary that
  captures through OS lifecycle hooks and stores git-versioned markdown you own.
  Despite the name, TencentDB Agent Memory needs no Tencent database (SQLite by
  default). You give up its zero-code proxy integration, the v2 hub's
  per-asset access control, and its CodeGraph code index (an adjacent,
  codebase-intelligence feature ai-memory does not do); you gain hook
  capture, a git-markdown wiki, typed handoffs, and zero-LLM operation.

## Where we're behind, or different by choice

- **Workstream operations are now visible to operators.** `/web/workstreams`
  presents trusted lifecycle states for managed workstreams without exposing
  checkout paths, native session ids, or transcript content. It deliberately
  does not infer blocked or review-ready states from untrusted prose.
- **Structured page filters are available before ranking.** `memory_query`
  accepts kind, tier, literal path prefix, and exact-array tag filters across
  normal, scoped, global, historical, profile, and pinned results. `as_of`
  remains the valid-time control. Author filtering is intentionally deferred
  until null root/anonymous attribution has explicit semantics.
- **Local foreground embedding is isolated from background writes.** A
  real-model probe demonstrated CPU contention, so local document inference is
  limited to one slot and a second slot remains available for query embedding.
  Hosted providers retain their existing concurrency policy; the LLM reranker
  remains separately bounded and fail-open.
- **Per-workload LLM behavior is measured before it is routed.** Root-only
  status JSON reports bounded calls, outcomes, latency, estimated tokens, and
  provider-reported usage for each closed internal workload. It records no
  prompt or output text. Provider/model selection remains global until real
  measurements and quality evaluation justify a larger configuration surface.
- **Document extraction is optional and text-first.** The standalone importer
  turns one explicit local text/Markdown, text-layer PDF, DOCX, PPTX, XLSX,
  VTT, or SRT file into attributed, sanitized markdown with
  bounded parsing and a conditional write. Unlike hosted multimodal products,
  it has no connectors, OCR, image understanding, or raw media transcription.
- **Marketplace reach is narrower than harness support.** ai-memory now ships
  its own Claude Code and Codex marketplace plugins and `ai-memory setup`
  detects and wires installed agents across Linux, macOS, and Windows. It is
  not listed in Anthropic's official directory, and long-tail agents still use
  setup/MCP recipes where no native plugin format exists.
- **No end-to-end QA number.** We publish retrieval hit@5; Mem0, Zep,
  Hindsight and EverOS publish end-to-end QA accuracy with a reader and a
  judge model. Our opt-in `answer=true` synthesis (2.4) has only an early,
  small-sample QA result (see
  [`benchmarks/retrieval-ab-r2.md`](benchmarks/retrieval-ab-r2.md)), so it is
  not a headline claim.
- **No git-history ingestion.** Hindsight ingests a repository's git history
  on first run. ai-memory's `backfill` replays native agent transcripts, not
  commits.
- **Recall is injected at session start, not on every prompt.** Supermemory's
  plugins search and inject on every prompt, and OpenViking's gateway injects
  recall into each request. ai-memory's hooks inject the session brief at
  session start (on the first prompt for harnesses without a start hook);
  later recall is an agent tool call.
- **No "show what was recalled" view in the agent.** The 3.0 web UI has a
  recall trace with per-stream ranks, but the agent session has no compact
  card showing the user which memories were injected.
- **Retrieval through capture, and no local reranker.** Through the shipped
  pipeline, LongMemEval-S hit@5 is 0.815 with local embeddings and 0.666
  zero-LLM (2.6); the 3.0 observation stream raises them to 0.945 and 0.951
  (3.3.0: 0.945 and 0.951). Ranking
  the same raw sessions whole scores 0.972 zero-LLM. The
  [decomposition](benchmarks/longmemeval-s-decomposition-2026-10-12.md) traced
  the gap to capture, and within capture not to the 2 KB privacy cap (pages holding exactly the captured text
  score 0.968) but the zero-LLM session page, which indexed only short turn
  titles. The only reranker is LLM-as-judge, so the zero-LLM default path has
  none, while Hindsight turns one on by default and agentmemory and
  basic-memory ship local ones.
- **Belief-strength and the dream pass ship, but off, with no proven win yet.**
  The 2.4 memory-aging set includes the two pieces that could move retrieval
  *quality*, folding belief-strength `confidence` into ranking authority and
  the LLM "dream" rewrite of cold clusters, but both are **off by default and
  gated on a recall eval that has not shown a gain yet**. We claim parity of
  *mechanism* with Hindsight/mcp-memory-service here, not a measured recall or
  QA improvement. Until the eval shows a positive delta the accurate statement is
  "shipped, opt-in, unproven," and the default path is unchanged.
- **Headline benchmark comparability.** Published numbers in this field
  measure different things:

  | Tool | Number | Kind | Setup |
  |---|---|---|---|
  | ai-memory 3.3 | LongMemEval-S hit@5 0.951 FTS / 0.945 local (3.0: 0.951 / 0.945; 2.6: 0.666 / 0.815) | Retrieval through capture | In-repo harness, all 470 questions; component mode FTS 0.972; published local top-ten movement tracked in #1854 |
  | MemPalace | 96.6% R@5 raw, 98.4% held-out (450 q) | Retrieval only | Different harness |
  | agentmemory | 95.2% `recall_any@5`; 2026-10-09 baseline 96.2% any-hit, 84.68% strict recall-all@5 | Retrieval, component | Whole raw sessions, reranker disabled; the authors say the baseline supports no competitive claim |
  | Mem0 | LongMemEval 94.4, LoCoMo 92.5, BEAM 64.1 (1M) / 48.6 (10M) | End-to-end QA, LLM-judged | Managed platform, top-200 budget; harness defaults to gpt-4o reader and judge; headline models not stated |
  | Zep | LongMemEval 90.2%, LoCoMo 94.7% | End-to-end QA, LLM-judged | gpt-5.4 reader and judge, cross-encoder rerank, ~5K context tokens; no public harness |
  | Hindsight | LongMemEval-S 94.6%, LoCoMo 92.0%, BEAM-10M 64.1% | End-to-end QA, LLM-judged | AMB harness, Gemini reader and judge (model unverified), ~43.6K context tokens per question |
  | EverOS | LongMemEval-S 94.00, LoCoMo 94.42 | End-to-end, LLM in the loop | Judge unverified; its website says 83.00% |
  | Supermemory | 95% Recall@15 (README), 97% "Recall@20" (research page) | Mixed | The 97% repeats in an LLM-judge table |

  None of these is like-for-like, and all but ours are self-reported. We
  publish a reproducible harness and a single stated metric.
- **No VLM fact extraction.** Because the default path is zero-LLM, ai-memory
  does not do the LLM-per-turn atomic extraction the fact-extractor and
  LLM-required systems build on. Consolidation is opt-in and page-shaped.
- **Not a graph database.** ai-memory chooses bi-temporal-lite on SQLite over a
  full temporal knowledge graph. It covers the useful 80% without the graph-query surface.
- **Single server, not SaaS.** There is no hosted multi-region tier and no enterprise
  console. That is intentional (own your data, one binary), but it matters
  if you want managed infrastructure.

If a specific comparison here reads as unfair or out of date, open an issue.
These numbers and claims are meant to be checkable against the linked research
and the reproducible harness.
