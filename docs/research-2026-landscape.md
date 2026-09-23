# Agent-Memory Landscape, September 2026 - Research Report

> Follow-up to the May 2026 research pass (`research-agentmemory.md`,
> `research-basic-memory.md`, `research-cognee.md`,
> `research-karpathy-llm-wiki.md`, and the `issues-*.md` tracker mining).
> Same lens as then: what the competition does, what is worth borrowing,
> what to deliberately avoid. Analysis only - no implementation decisions
> are made here, and none of the recommendations below imply drastic
> change. Sources at the end; every load-bearing claim was checked against
> a live page in September 2026, not remembered.

## 1. The headline: our architectural bet got standardized

On June 12, 2026, Google Cloud published the **Open Knowledge Format
(OKF) v0.1**: organizational knowledge as a plain directory of markdown
files with YAML frontmatter, one concept per file, a single required
frontmatter field (`type`), no SDK, no runtime, vendor-neutral. It is an
explicit formalization of Karpathy's "LLM wiki" - the same gist our
project was built from - and it is already an ecosystem signal: new
memory servers advertise themselves as "OKF-backed", and the format is
positioned as the interop layer that makes agent memory portable across
tools.

Our `wiki/` **is nearly an OKF bundle already**: markdown, YAML
frontmatter, one page per concept, typed via our own frontmatter
conventions. The delta is a conventions mapping, not an architecture
change.

The second validation came from an unexpected direction: **Letta**
(formerly MemGPT) published "Is a Filesystem All You Need?" arguing that
agents post-trained for iterative file search perform well against
specialized memory systems (74.0% on LOCOMO with plain files). Whatever
one thinks of the framing, the leaders of the "memory OS" camp conceding
ground to file-first memory supports the substrate we chose - files as
source of truth, derived indexes for retrieval - which is *both* halves
rather than either alone.

## 2. How the original research subjects evolved

**agentmemory (rohitg00)** - actively maintained and maturing along the
trajectory the May report predicted. Notable since then: a real-workload
benchmark ("coding-agent-life-v1", 635 requests / 888K tokens / 35 hours,
R@5 = 0.967 for the hybrid retriever), `AGENT_ID` multi-agent isolation
with an opt-in isolated recall scope, and runtime cost warnings when a
premium model is configured for compression. Still on the iii-engine +
JSON-in-KV substrate the May report identified as its structural
constraint. The 53-tool surface did not shrink.

**basic-memory** - alive, same positioning (structured markdown both
humans and LLMs edit; local files). No architectural surprises. Its
niche overlaps most with OKF, which may absorb its differentiation.

**cognee** - has moved decisively into content-led growth ("best memory
framework" listicles on its own blog rank for every comparison query)
and now advertises 14 retrieval modes and the broadest integration
matrix. Technically still the triple-store (relational + vector + graph)
ECL pipeline the May report described; still heavy for a homelab.

**mempalace** - the cautionary tale of the year. ~47K stars within two
weeks of its April 2026 launch on the strength of a claimed 96.6% R@5 on
LongMemEval; then an independent audit (its issue #29, corroborated by
`mcp-memory-service`'s issue #27 analysis and a critical-analysis paper,
arXiv:2604.21284) showed the number reproduces with a **minimal ChromaDB
default setup with the palace architecture inactive**. The spatial
metaphor (wings/rooms/closets/drawers) added nothing measurable.
Lessons: (a) benchmark claims are now audited by the community, quickly
and publicly; (b) metaphor-driven architecture is not retrieval
architecture; (c) stars measure virality, not substance.

## 3. The 2026 landscape beyond the original subjects

The market has consolidated into recognizable camps:

| Camp | Representatives | Core idea |
|---|---|---|
| Temporal knowledge graphs | **Zep/Graphiti** (20K+ stars, 25K weekly PyPI installs), Cognee | Facts as bi-temporal graph edges: *when true in the world* vs *when observed*, superseded rather than deleted |
| Memory OS / self-editing | **Letta**, MemOS, EverMemOS, MIRIX | The agent edits its own tiered memory via tools; "sleep-time compute" does consolidation off the hot path |
| Fact extractors | **Mem0**, LangMem | LLM extracts atomic facts per turn; lightweight personalization |
| Hosted memory API (hybrid) | **Supermemory**, **LiquidLM** | Chunk-RAG + LLM temporal fact-graph + per-user profiles, answered in one query; managed connectors + multimodal; cloud-first (see below) |
| User-modeling / theory-of-mind | **Honcho** (Plastic Labs) | Memory as a *reasoning* problem: derive what each "peer" knows/believes about another over time; model the human, not the project (see below) |
| **File-first wiki memory** | **us**, basic-memory, OKF, Letta's filesystem result, mempalace (nominally) | Markdown source of truth, derived indexes, human-editable |
| File-first wiki (LLM-leaning) | **memU**, **EverOS** | Markdown source of truth + derived vector/SQLite(+LanceDB) indexes, but LLM-required for the capture/distill flow; skill-distillation and offline consolidation (see below) |
| Self-hosted coding-agent store | **Engram** | Single Go binary, SQLite+FTS5, MCP, project scope, session-summary handoffs — a structured-row sibling captured via agent MCP calls, not lifecycle hooks (see below) |
| Governed multi-agent / fleet memory | **Caura** | Shared fleet memory with visibility scopes, trust tiers, audit, governance on Postgres+pgvector — org-scale, not project-scale (see below) |
| Shared memory server (proxy fan-in) | **TencentDB Agent Memory** | One proxy in front of many harnesses distilling Chat Memory / Skill / Wiki / CodeGraph; proxy interception, not MCP/hooks (see below) |
| Code intelligence | **DeusData/codebase-memory-mcp** (~42K stars) | Index the *codebase* (162 languages, tree-sitter → SQLite graph, static C binary) rather than the *session* - adjacent, not competing: it remembers what the code is, not what you did (see `research-codebase-memory-mcp.md`) |
| Agent-harness OS | **ECC** (~247K stars), plus the skills/agents-pack ecosystem | Install a whole plan→test→implement→review→**remember**→improve loop into the agent; memory is one thin pillar ("optimize the context window, persist everything else"), deliberately kept as *context, not policy* - adjacent, not competing (see `research-ecc.md`) |

**Closest architectural sibling: `doobidoo/mcp-memory-service`** (1.9K
stars, 3.2K commits, active through August 2026). It is what we would be
if we had chosen fact-rows over wiki pages: SQLite(+vec) storage, local
ONNX embeddings (all-MiniLM-L6-v2, no provider dependency), hook-driven
capture for Claude Code, autonomous scheduled consolidation
(decay + LLM compression + DBSCAN/hierarchical clustering + "belief
derivation" over typed edges), a **typed knowledge graph**
(`causes` / `fixes` / `contradicts` edges), multi-backend sync
(local-first with optional Cloudflare replication), and inter-agent
messaging implemented as tagged memories on the shared pool. Its issue
tracker repeats patterns from our own history - v11.10.0 fixed
consolidation time horizons that *silently ignored their configured
window*, the same class of quiet-lifecycle bug our `#526`/`#528` work
addressed with observable outcomes. Published honest numbers: 80.4% R@5
on LongMemEval turn-level, 86.0% session-level. **Where we now stand
(2.4):** its consolidation set — type-dependent TTL (365/180/90/30),
extractive compression, DBSCAN adaptive-eps clustering, access/connection
boosts, and 0.4–0.75-band contradiction detection — is the direct
grounding for the 2.4 memory-aging program
([`design-memory-aging.md`](design-memory-aging.md)), which now ships each
at parity but **zero-LLM by default, reversible** (supersede-not-delete +
`restore-page`), and **off by default** (per-tier `[decay.half_life_days]`,
extractive tier-down, cold-cluster dedup, access reinforcement on all read
paths, and a `memory_lint` contradiction finding). The difference is
posture, not mechanism: theirs runs autonomously; ours preserves the
zero-LLM default path and evicts nothing on upgrade.

**Platform development: Claude Code native "auto memory"** shipped
default-on (v2.1.59): the agent keeps its own `MEMORY.md` plus topic
files per project, loading the first 200 lines each session. Its
documented limits are precisely our differentiators - machine-local
with **no sync**, single-agent (nothing carries to Codex/Cursor/others),
repo-scoped, no search beyond reading files, no capture of tool
lifecycle, no team story. Two readings, both true: the basic solo
use case ("remember my project between sessions on this laptop") is
being absorbed by the platforms; and the platforms are training users
to *expect* persistent memory, which makes the cross-machine,
cross-agent, multi-user version - what v1.39.0 hardened - the durable
value. Native memory is the funnel, not the competitor.

**Best-funded new entrant: `vectorize-io/hindsight`** (~23.5K stars,
MIT, a company behind it, a preprint paper arXiv:2512.12818, and a
LongMemEval claim of 91.4%). It is the most serious competitor in this
series, and it is useful precisely because it *agrees with our core
bet from the opposite substrate*: Hindsight is Postgres/pgvector-primary
and LLM-required, yet its top memory tier is **"mental models" - living
markdown pages of settled knowledge that a background process
continuously rewrites as the bank learns**, which an agent "boots" from
instead of rediscovering context each session. That is our `wiki/` +
auto-improve loop under another name, arrived at independently and backed
by a paper - the strongest external validation of pages-over-facts in
this whole research series. Two more things to carry forward: its
consolidation is **belief-strength, not binary supersession** (evidence-
backed observations with quotes + a proof count; new evidence
strengthens/weakens/extends rather than replaces), a richer model than
ours; and its **"bank" = strict per-(user|agent|project) isolation, no
cross-bank leakage** is the deliberate *opposite* of our shared-within-a-
project invariant (#16) - and the natural prior art for issue #708
(per-project authorization) and #709 (project identity), where the design
should layer a bank-like boundary *above* sharing rather than replacing
it. Read skeptically where it earns it: the "independently reproduced"
LongMemEval number is from the *co-developing* labs (two of seven authors
are Virginia Tech Sanghani faculty; the Post is a named collaborator), it
is a preprint, and it reports accuracy rather than the R@5 others quote -
still the most credible benchmark claim here (a paper with per-category
sub-scores, gains concentrated where structured temporal memory should
help), and a reminder the serious players now publish numbers. **Where we
now stand (2.4):** the two ideas flagged to carry forward — belief-strength
over binary supersession, and the background rewrite loop — now ship, as
opt-in layers over the zero-LLM core, per
[`design-hindsight-borrowings.md`](design-hindsight-borrowings.md) §3 and
[`design-memory-aging.md`](design-memory-aging.md) B1–B4. Belief-strength is
a read-time, zero-LLM `confidence` (distinct-session breadth + recency +
live `contradicts`, capped) exposed inertly in `explain`/`status` and
foldable into ranking behind a **default-0.0, R2-gated** weight; the
"dream" pass is an idle-scheduled, cancel-on-activity, surprisal-first LLM
rewrite of cold clusters that is **off by default and never deletes a
source**. We designed *against* Hindsight's own documented failure —
belief *entrenchment* (raw proof-count pins a wrong page) — with
breadth-not-count weighting, a confidence cap, and supersession always
winning over evidence. Honest caveat: no R2 delta has been run, so this is
parity of *mechanism*, not a measured accuracy win. Full analysis in
[`research-hindsight.md`](research-hindsight.md).

**Biggest-backed new entrant: `volcengine/OpenViking`** (~37.5K stars,
ByteDance/Volcano-Engine, AGPLv3 core + Apache-2.0 CLI/examples, three
papers incl. **VikingMem** at VLDB 2026, arXiv:2605.29640). Positioned as a
"self-evolving context database" that unifies memory + RAG + skills. It is
LLM-**required** (a VLM for extraction/compilation plus an embedding model)
over a vector store fronted by a `viking://` virtual filesystem, so it sits
in the opposite corner from our zero-LLM, file-first default — and, like
Hindsight, it validates our substrate from there: memory is
document/directory-scoped, not atomic facts, and a background pass
(*commit session → extract → compare candidates for create/merge/skip*, and
optional VikingBot "compile" into wikis/KGs) is our consolidation +
auto-improve loop under other names. Two ideas are genuinely worth carrying
forward. First, **progressive-disclosure tiers**: every memory is loadable
at **L0 (one-sentence abstract, for relevance checks)**, **L1 (overview, for
planning)**, or **L2 (full original, read only when needed)** — the reported
34-91% input-token reduction (LoCoMo) is almost entirely this deferral, and
it maps directly onto work we already have (V61 abstract embeddings, the
session brief) rather than a new architecture — and 2.4's **extractive
tier-down** (A2, [`design-memory-aging.md`](design-memory-aging.md)) now
persists exactly this shape for cold episodic pages: an L0 abstract, an L1
summary, and an L2 keep-token set, with the full prose one supersession-
restore away. Second, **directory-scoped
retrieval** (a "TrieHI" prefix-tree vector index that narrows a query to a
path subtree before ranking) — a cheaper cousin of our per-project scoping,
one level down at the `_rules/` / `norms/` path prefix. Read the numbers
with the same skepticism the rest of this report earns: the LoCoMo lifts
(24->82%, 33->83%, 57->80%) are **in-house** (Volcengine) against
agent-native/stateless and simple-RAG baselines, not against other memory
systems, and the papers are VLDB-2026/submitted, not yet peer-reviewed.
What to avoid is unchanged from the camp it belongs to: LLM-required
capture, an opaque swappable-storage story, AGPLv3 on the core, and the
SaaS/enterprise-licensing weight that pulls against a single self-contained
binary. Per the research-doc convention, OpenViking has no standalone
deep-dive; this section is its record.

**Well-funded generalist: `supermemoryai/supermemory`** (~30K stars, MIT
repo, ~$2.6-3M seed from Susa/Browder/SF1.vc plus angels incl. Jeff Dean,
Cloudflare's CTO, and Logan Kilpatrick; founder Dhravya Shah). Positioned as
"context infrastructure for AI agents": a **hosted memory API** (the paid
`api.supermemory.ai`) with an MIT self-hostable local binary as the on-ramp.
This is the entrant our own docs most **mislabeled** - it is *not* a Mem0-style
atomic fact extractor. It is a **hybrid**: every ingested item is
chunked+embedded for RAG **and** run through an LLM "dreaming" pass that
extracts atomic facts into a **temporal vector-graph** (Updates / Extends /
Derives edges, `isLatest` supersession, decay + forgetting) **and** rolled into
a standing per-user **profile**, with RAG and memory answered in one query. Its
default "Dynamic" mode batches *related* documents rather than extracting per
turn, so even the "LLM-per-turn" half of the old label is wrong (that mode is
opt-in). Like the others it validates our bets from the opposite corner: the
batch "dream" pass is our consolidation ("compile, not retrieve") under another
name, and supersede-don't-delete is our page-supersession chain. Where it
genuinely leads is **ingestion breadth** - managed connectors (Drive, Gmail,
Notion, OneDrive, S3, GitHub, web crawler), multimodal PDF/image/audio, and
metadata + `containerTag` query filters, plus a first-class user profile - but
every one of those is **cloud-only and LLM-required**; the self-hosted binary
(embedded graph store, local `bge-base` embeddings, offline) explicitly "lacks
connectors and the proprietary extraction models." It sits opposite us on every
axis we chose: an **opaque graph is the source of truth** (no git, no markdown,
nothing to `grep` or diff), the quality path is **LLM-required** (no zero-LLM
default), and the best features **pull toward the paid Cloudflare cloud**. It is
also not really in our lane: it is a general **memory-API for apps and user
personalization** whose coding-agent plugins (Claude Code, Cursor, Codex,
OpenCode) capture by **turn-batch polling**, not OS lifecycle hooks, with no
cross-harness handoff, multi-user page-sharing, or per-project 3-tuple isolation
model - the coding-agent-continuity primitives are exactly what we build and it
does not. Read the numbers with the standard skepticism: "95% LongMemEval_s at
Recall@15 with aggregation, ~720 tokens, sub-300ms p50, first on LoCoMo" are all
**vendor self-reported** - though their **MemoryBench** harness and the
accuracy / latency / context-tokens "MemScore" triple are open and worth
borrowing (see R8). Per the research-doc convention, Supermemory has no
standalone deep-dive; this section is its record.

**Solo-built Supermemory-style hosted API: `liquidlm.com`** (**LiquidLM**,
closed-source cloud SaaS, no funding — a single independent maker, Carlos Souza).
Its own tagline is "persistent AI agent memory and RAG as a service." It is the
same *camp* as Supermemory — a proprietary hosted memory API with entity/
relationship extraction, temporal metadata, and an MCP surface on top — but a
smaller, indie, individual-priced take on it (Free / $20 Pro / $200 Ultra,
usage-based by ingestion volume). You ingest files/notes/links/media into cloud
**"Vaults"**; it extracts entities, relationships, timestamps, and **staleness/
`supersedes` signals**, then serves hybrid semantic+FTS retrieval with rerank and
grounded, cited chat. It is on **every axis we deliberately chose against**: the
**vault is an opaque hosted store** (no git, no markdown, no self-host, nothing to
`grep`/diff), **LLM is intrinsic and not optional** (Gemini-class extraction/
embeddings — no zero-LLM mode), and the memory engine is **closed-source** (the
only open artifact is the MIT npm client `@liquidlm/cli`, v0.1.5 published
2026-09-17; the GitHub `cli` repo is archived and empty). Where it is genuinely
interesting is **breadth of ingestion and a clean MCP tool surface**: heavy
multimodal capture (video/audio/image/PDF/Office, auto-transcription, a
`yt-dlp`-fetch-locally pattern), a GitHub repo-sync connector, and eight native
MCP tools worth studying by name — `put/get/list/search_knowledge`,
`list_entities`, `get_related`, **`follow_references`** (walk the citation graph),
**`pin_knowledge`** (standing context surfaced *before* search), and
`forget_knowledge`. Tested harnesses: Claude Code, ChatGPT, Codex CLI, OpenCode
over a hosted MCP endpoint (OAuth or scoped PAT). **No retrieval benchmarks** are
published (only ROI marketing). It is **not in our lane and a weak migration
target either way**: it captures by *explicit upload/sync* into a general "second
brain," where we capture *automatically via lifecycle hooks* compiled into
per-project coding pages you own — different job, different buyer. Two ideas are
worth borrowing conceptually (never the substrate): the explicit **`pin_knowledge`
= "seen before they search"** contract over our existing `pinned` pages, and
**`follow_references`/`get_related` as first-class graph-walk MCP tools** over the
link-neighbor RRF we already compute internally. Per the research-doc convention,
LiquidLM has no standalone deep-dive; this section is its record.

### New entrants raised in issue #810 (verified 2026-09-22)

Five projects flagged in issue #810, each researched from its own repo/README
(and, where the name could mislead, the LICENSE and the metadata endpoint) rather
than from a name search — the failure mode this series has been burned by before.
None displaces the camp taxonomy; they slot into it, and two are close enough to
be honest migration conversations.

**Closest new sibling: `Gentleman-Programming/engram`** (~6.8K stars, MIT, single
Go binary, commit 2026-09-22, release v2.0.0). Engram is the newcomer nearest our
own lane: persistent memory for AI coding agents over **SQLite + FTS5** in a
zero-dependency stateless Go executable (`~/.engram/engram.db`), an MCP **stdio**
server confirmed against Claude Code, Codex, OpenCode, Gemini CLI, Cursor,
Windsurf and more, project-scoped, with **session-summary handoffs**
(a `mem_session_summary` carrying goal/instructions/discoveries/next-steps/files),
**git-sync** of portable compressed chunks across machines, and an optional
Engram Cloud for project-scoped replication. Where it differs from us is the two
choices that define our substrate: capture is **agent-driven** (the agent calls
`mem_*` MCP tools under an "operating contract," not OS lifecycle hooks that
capture automatically), and **SQLite is the source of truth** (git-syncs
compressed chunks, not a hand-editable git-markdown wiki you can `grep`/diff).
It is a fact-row/structured-store sibling in the mold of mcp-memory-service, but
Go and MCP-capture rather than hook-capture — the honest "closest self-hosted
coding-agent store" newcomer. **No retrieval benchmark is published.** Per the
research-doc convention, Engram has no standalone deep-dive; this section is its
record.

**Local-first file-first sibling: `EverMind-AI/EverOS`** (~13.1K stars,
Apache-2.0, Python 3.12+, commit 2026-09-09, release v1.3.1). Architecturally the
closest of the five to our bet: a **local-first memory runtime** with **canonical
Markdown as the source of truth** ("readable, editable, diffable, and
Git-versioned") over derived **SQLite + LanceDB** indexes, direct file edits
cascading through watchers — the same files-as-truth + derived-index split we
chose. It adds dual first-class tracks (a user `episodes/profile` and an agent
`cases/skills`), an editable **Knowledge Wiki** with taxonomy + CRUD APIs, and
**offline memory evolution** that merges episode clusters and refines profiles —
our consolidation/auto-improve loop under other names. It validates the substrate,
but from the LLM-leaning corner: the minimal tier still **requires an LLM for the
core flow** (embedding/reranking optional, OpenRouter the default; a zero-LLM
demo exists but is not the operating default), it is a Python stack rather than a
single binary, and MCP support is not stated in the README. **No retrieval
benchmark is published.** Per the research-doc convention, EverOS has no
standalone deep-dive; this section is its record.

**LLM-wiki with skill distillation: `NevaMind-AI/memU`** (~14.4K stars,
Apache-2.0, Python, commit 2026-09-21; last tagged release v1.5.1 from 2026-03-23,
so releases lag active code). Positioned as a **shared LLM wiki across sessions,
agents and devices** whose signature is **automatic skill distillation** —
converting agent session history into reusable **Markdown** workflows — plus
scheduled memorization and standing retrieval-instruction injection into host
instruction files. Storage is SQLite (default) or Postgres+pgvector with
brute-force-cosine or pgvector search and pluggable embedding providers
(OpenAI/Jina/Voyage/Doubao/OpenRouter); the core logic is advertised at ~500 LOC.
It is **file-first-adjacent** (Markdown output) but **LLM-leaning**: the host
agent does the judgment/synthesis (the MemoryService itself makes no LLM calls, so
the *product* still depends on a provider), there is **no MCP** — it ships as
sidecar binaries that patch host instruction files, with adapters for
Cursor/Claude Code/Codex/OpenClaw/Hermes/WorkBuddy — and a managed cloud exists at
memu.so. **No named benchmark** (no LoCoMo/LongMemEval figure) is in the README.
Its skill-distillation-into-Markdown is the idea worth noting against our
experience pass. Per the research-doc convention, memU has no standalone
deep-dive; this section is its record.

**Governed fleet memory (different buyer): `caura-ai/caura`** (~0.5K stars,
Apache-2.0 core + managed platform at caura.ai, Python/FastAPI, commit
2026-09-22, release backend-v3.17.2). Caura is a genuinely new camp for this
report: **governed shared memory for multi-agent fleets**. On **PostgreSQL 16 +
pgvector** (Redis optional) it layers row-level multi-tenant isolation, three
visibility scopes (`scope_agent`/`scope_team`/`scope_org`), **four trust tiers**
governing cross-fleet read/write/delete, a full **audit trail** ("every write,
delete, and transition logged"), automatic PII flagging, an auto-extracted
knowledge graph, and contradiction detection with supersession — a native MCP
surface at `/mcp` (12 tools) for Claude Desktop/Code, Cursor, Windsurf. Its
benchmark claims are **self-reported** and, per its own README, single-agent
proxies for a fleet product it says those benchmarks "can't measure": **LoCoMo
77.6% accuracy / 96.6% token savings** and **LongMemEval 92.2% accuracy / 79.2%
token savings** (with a "23 ms p50 · 27 ms p95" latency figure and a stated
production deployment of "300+ AI agents" at eToro — vendor-reported, not
independently verified). It sits opposite us on substrate (opaque Postgres, LLM
extraction, hosted-leaning) and orthogonal on goal: **org-scale fleet governance**
rather than one project's git-backed wiki. Its **trust-tier governance is a real
capability we do not have** (noted honestly in `competitive-parity.md`), but it is
a different buyer, not a coding-continuity migration target. Per the research-doc
convention, Caura has no standalone deep-dive; this section is its record.

**Proxy fan-in memory server (misleading name): `TencentCloud/TencentDB-Agent-
Memory`** (~27.2K stars, MIT, Node/TypeScript, commit 2026-09-22, release
v2.0.1). Despite the "TencentDB" name it **requires no Tencent database product**
— storage is **SQLite by default** (an experimental, off-by-default MongoDB
backend exists) — so the name-search trap here would be to file it as a managed
cloud DB feature; it is not. It is a **team-level memory hub** that distills
conversations, documents and code into four reusable assets — **Chat Memory,
Skill, Wiki, CodeGraph** — and its distinctive mechanism is a **proxy fan-in**: a
three-service deployment (Memory Core + Memory Hub + Proxy) where agents point
their API **base URL** at the proxy ("One Proxy, unchanged protocol, zero-code
integration"), so capture is **proxy interception, not MCP or OS lifecycle
hooks**. Documented harness support spans Claude Code, Codex, DeepSeek Harness,
CodeBuddy, WorkBuddy, Hermes and OpenClaw. Its benchmark claim is **self-reported
PersonaMem** (48% without memory → 76% with it, "+59% relative"). Its **CodeGraph**
is a codebase-intelligence feature adjacent to, not overlapping, our session
memory. Per the research-doc convention, TencentDB Agent Memory has no standalone
deep-dive; this section is its record.

**Adjacent, not head-to-head: `plastic-labs/honcho`** (~7K stars, AGPL-3.0
core + managed cloud, **$5.35M pre-seed** led by Variant/White Star/Betaworks).
Honcho is the archetype of a **new camp** this report had no bucket for:
**user-modeling / theory-of-mind memory**. Its unit of memory is a **"peer"**
(any entity that "persists but changes over time — users, agents, objects") and
its differentiator is treating memory as **"a reasoning problem, not a retrieval
problem"** — it models *what one peer knows and believes about another* over
time. The substrate is the opposite of ours on every axis we chose:
**PostgreSQL is the source of truth** (Postgres FTS/GIN + HNSW vectors, Redis
cache, pluggable LanceDB/Turbopuffer — **no git, no markdown, nothing to `grep`
or diff**), it is **LLM-required** (a **Deriver** does one structured-output LLM
call per message batch; a scheduled **Dreamer** runs surprisal-prioritized
"dream-time" consolidation into premise→conclusion reasoning trees — no zero-LLM
path), and it runs as a **multi-service stack** (FastAPI API + async worker +
Postgres + Redis), cloud-leaning with a self-hostable core. Its flagship is the
**Dialectic endpoint** (`peer.chat()`): a natural-language *oracle* — you ask a
question about a peer and a synchronous tool-using agent (`search_memory`,
`grep_messages`, `get_reasoning_chain`, temporal search, five reasoning tiers)
synthesizes an answer rather than returning raw hits. Benchmarks are strong but
**vendor self-reported** (blog dated Dec 19 2025): LongMemEval-S 90.4% (Haiku
4.5) / 92.6% (Gemini 3 Pro), LoCoMo 89.9%, "median 5% of context tokens used."
It now ships coding-agent front-ends too (Claude Code/Codex/Cursor/OpenCode
plugins + an **MCP server** with exactly three tools: `honcho_search`,
`honcho_chat`, `honcho_remember`), so it **collides with us on the shelf** — but
the thing remembered is fundamentally different: **Honcho remembers the *user*;
ai-memory remembers the *project*.** A coding team wanting cross-harness project
continuity would not switch to it (opaque DB, LLM-mandatory, user-centric,
multi-service), and an app builder wanting end-user personalization would not use
us — **adjacent, not a migration target either way**. Two ideas are worth
carrying forward as *optional LLM layers over* our zero-LLM core, never
requirements: the **dialectic/oracle query** (ask-a-question → synthesized
answer, which maps onto our retrieval as an opt-in LLM step) and the
**reasoning-tier ladder** (minimal→max reasoning per query as a clean
cost/quality knob). **Both now ship on 2.4** as opt-in, off-by-default LLM
layers (`memory_query answer=true` #782; `reasoning: {minimal..max}` #783),
and the Dreamer's *scheduling shape* — surprisal-first ordering, idle
trigger, cancel-on-activity — is the grounding for 2.4's opt-in "dream"
consolidation pass (#816, [`design-memory-aging.md`](design-memory-aging.md)
B3/B4), which stays off by default and never deletes a source. Deliberately
**reject** the rest as off-mission: LLM-mandatory
ingestion, opaque-Postgres-as-truth, the Postgres+Redis+worker operational
weight, and the heavy theory-of-mind engine itself — a coding agent needs project
facts, decisions, and conventions, not a psychological model of the developer
(the lightweight "standing user preferences" slice we already have via global
scope is enough). Positioning note: preempt the inevitable "but Honcho scores 90%
on LongMemEval" by clarifying those are *user-recall* benchmarks on chat
transcripts, not *coding-project* recall — a different task we are not competing
on. Per the research-doc convention, Honcho has no standalone deep-dive; this
section is its record.

### Popularity and maintenance signal (as of 2026-09-18)

None of the tracked competitors is stale or abandoned — every repo below had a
commit within the last ~10 days and grades **ACTIVE**. Star counts are GitHub's
raw figures (approximate; the biggest few were sanity-checked against prior
research). This is a healthy, crowded, fast-moving field: "we picked a dead
space" is not a claim we can make, and none of these can be dismissed as
unmaintained. (The five issue-#810 new-entrant rows — TencentDB Agent Memory,
memU, EverOS, Engram, Caura — were verified against the GitHub metadata endpoint
on 2026-09-22.)

| Project | Stars (~) | Last commit | Latest release | Status |
|---|---|---|---|---|
| Mem0 | 65.6k | 2026-09-18 | openclaw-v1.2.0 (2026-09-18) | ACTIVE |
| OpenViking | 38.0k | 2026-09-18 | v0.4.20 (2026-09-14) | ACTIVE |
| Zep/Graphiti | 31.0k | 2026-09-17 | v0.30.2 (2026-09-08) | ACTIVE |
| Cognee | 30.8k | 2026-09-18 | v1.5.4rc1 (2026-09-15) | ACTIVE |
| Supermemory | 30.1k | 2026-09-18 | server-v0.0.8 (2026-08-17) | ACTIVE |
| agentmemory | 28.6k | 2026-09-14 | v0.9.29 (2026-08-16) | ACTIVE |
| TencentDB Agent Memory | 27.2k | 2026-09-22 | v2.0.1 (2026-08-25) | ACTIVE |
| Letta | 24.8k | 2026-09-10 | 0.16.8 (2026-05-14) | ACTIVE (releases lag code) |
| Hindsight | 23.9k | 2026-09-18 | v0.10.0 (2026-09-14) | ACTIVE |
| memU | 14.4k | 2026-09-21 | v1.5.1 (2026-03-23) | ACTIVE (releases lag code) |
| EverOS | 13.1k | 2026-09-09 | v1.3.1 (2026-09-08) | ACTIVE |
| Honcho | 7.2k | 2026-09-18 | tag v3.2.0 (no GH release) | ACTIVE |
| Engram | 6.8k | 2026-09-22 | v2.0.0 (2026-09-18) | ACTIVE |
| basic-memory | 4.0k | 2026-09-16 | v0.23.2 (2026-08-25) | ACTIVE |
| mcp-memory-service | 2.0k | 2026-09-18 | v11.12.0 (2026-09-14) | ACTIVE |
| LangMem | 1.7k | 2026-09-09 | none (PyPI-versioned) | ACTIVE |
| Caura | 0.5k | 2026-09-22 | backend-v3.17.2 (2026-09-19) | ACTIVE |
| LiquidLM | n/a (closed) | n/a (engine closed) | `@liquidlm/cli` 0.1.5 (2026-09-17) | ACTIVE (young, pre-1.0, solo) |

Reading it honestly: raw stars track **funding and app-developer reach**, not
coding-agent fitness — the leaders (Mem0, OpenViking, the KG/cloud entrants) are
the app-personalization and hosted-context players, a different buyer from ours
(see §5 and [`competitive-parity.md`](competitive-parity.md)). Our true
architectural siblings sit lower on the star curve (basic-memory ~4.0k,
mcp-memory-service ~2.0k) precisely because the self-hosted, file-first,
coding-continuity niche is smaller and less VC-amplified — that is the segment we
lead, not the whole chart. Two mild flags: Letta ships commits actively but its
last *tagged* release is from May 2026 (formal releases lag the code), and
LangMem/Honcho publish no GitHub *releases* (PyPI/tag-only versioning); and
LiquidLM is **closed-source**, so it has no repo signal at all — its only public,
open artifact is the MIT npm client `@liquidlm/cli` (v0.1.5, 2026-09-17), which is
what dates it as active but young.

## 4. Research developments worth knowing

- **"Rethinking How to Remember: Beyond Atomic Facts in Lifelong LLM
  Agent Memory" (TriMem, arXiv:2605.19952)** - argues atomic-fact stores
  lose the relational/causal context agents need, proposes a
  document/page/narrative hierarchy, and shows it outperforming
  fact-baselines on retrieval and multi-step reasoning. This is direct
  academic support for pages-over-facts - the bet that separates us from
  the Mem0 camp. Its "narrative" layer (temporal storylines connecting
  events) is the one level we only partially have (sessions and
  handoffs are narrative-ish; nothing stitches them across weeks).
- **"From Storage to Experience" survey (arXiv:2605.06716)** - frames
  the field's evolution as Storage (preserve trajectories) → Reflection
  (refine them) → Experience (abstract *across* trajectories, proactive).
  We are solidly through Storage and Reflection (capture, consolidation,
  curator, lint, feedback-driven salience). The frontier it names -
  cross-trajectory abstraction - is where our auto-improve is a
  beginning, not an answer.
- **LongMemEval-V2 (arXiv:2605.12493)** - the benchmark generation has
  moved from "recall the fact" to "behave like an experienced colleague"
  over long horizons, and finds every evaluated system far from it. Two
  implications: our product framing matches where evaluation is going,
  and the eval-harness recommendation from the May research (#8 in
  `research-agentmemory.md`, never implemented) is now more pressing -
  the field publishes audited numbers and we publish none.
- **Bi-temporal validity (Zep paper, arXiv:2501.13956, now mainstream
  via Graphiti)** - the one *specific* mechanism from the graph camp
  worth studying: every fact/edge carries event-time and
  ingestion-time, so "what did we believe on June 1" and "what was true
  on June 1" are both answerable, and corrections are supersessions
  rather than overwrites. Our pages already supersede; our `links` and
  `entities` rows do not carry validity intervals.
- Also active, lower relevance for us: MIRIX (multi-agent memory
  specialization), MemOS/EverMemOS (scheduling memory like an OS),
  A-TMA (state-aware memory failure taxonomy), sleep-time compute
  (Letta: consolidate between sessions, not during) - the last is
  something we already do by shape (session-end consolidation jobs,
  scheduled maintenance) without the branding.

## 5. What this means for ai-memory - analysis and recommendations

> Companion audit: [`competitive-parity.md`](competitive-parity.md) turns this
> "what to borrow" lens around and asks the harder one - in the goals where we
> *overlap* a competitor, are we actually better or did we copy without
> improving, and is ai-memory worth migrating *to*? It is the self-critical
> record behind this section's recommendations.

The May research led us to build: versioned supersession, retention
formulas, hybrid RRF retrieval, opt-in LLM consolidation, handoffs as a
protocol, single self-contained binary, typed scope isolation. Every one
of those choices looks *better* in September than it did in May - the
substrate competitors struggled (mempalace's metaphor, agentmemory's
KV), the file-first camp got standardized, and pages-over-facts got a
paper. Nothing in this pass argues for re-architecture. The
recommendations are additive and ranked:

**R1 - OKF conformance (small, high leverage).** Map our frontmatter
conventions onto OKF v0.1 (`type` field plus its small vocabulary) and
add an OKF export - possibly just documentation plus a thin
`ai-memory export --okf` view of `wiki/`. Being the *server-grade*
implementation of the format Google standardized is a positioning gift:
interop with every OKF-aware consumer, at conventions-mapping cost.
Evaluate whether native conformance (wiki pages *are* OKF files) beats
an export step; the closer to native, the stronger the story.

**R2 - Reproducible eval harness against LongMemEval-V2 (medium).** The
oldest open recommendation, now with a sharper reason: benchmark claims
are being audited (mempalace) and honest numbers are being published
(mcp-memory-service). We cannot say where we stand, and "experienced
colleague over long horizons" is literally our pitch. Harness in-repo,
runnable on demand like `writer_throughput`, numbers in docs with the
run command - never a marketing claim without the harness. Hindsight
(`research-hindsight.md`) sharpens this further: a funded competitor now
ships a *paper* with per-category LongMemEval sub-scores (91.4%), so the
bar is no longer "publish a number" but "publish a harness and a metric
someone else can re-run" - fix the split and the metric (theirs is
accuracy, agentmemory/mcp-memory-service quote R@5; pick one and state
it) so our result is comparable, not just present. **Now load-bearing on
2.4:** the two aging features that can move quality — belief-strength folded
into ranking (#815) and the LLM "dream" pass (#816) — ship **off by default
and gated on this harness before they may default on**; no R2 delta has been
run yet, so both remain opt-in and unproven (see
[`design-memory-aging.md`](design-memory-aging.md) R2 acceptance).

**R3 - Typed relation edges (small-medium).** `causes` / `fixes` /
`contradicts` on our existing `links`/`entities` model, from the
doobidoo playbook. Cheap because the tables exist; valuable because
`contradicts` feeds the lint pass we already run, and `fixes` makes
bug-page chains retrievable as chains.

**R4 - Temporal validity on entities/links (medium, evaluate first).**
Bi-temporal-lite: `valid_from` / `superseded_at` on entity-page links so
"what was the database choice in June" is answerable. Pages already
supersede; this extends the same idea one level down. Worth a design
doc before any code - the Graphiti paper is the reference.

**R5 - Cross-session abstraction pass (medium-large, the "Experience"
gap).** A periodic consolidation that reads *across* recent sessions
per project and rewrites pattern/preference pages - the cross-trajectory
abstraction both the survey and TriMem's narrative layer point at.
auto-improve is the natural host; today it is per-session-triggered.
Hindsight's **mental-model tier + reflect loop** (`research-hindsight.md`)
is the shipped reference implementation for exactly this - a background
job that rewrites standing-answer pages across accumulated evidence, that
an agent boots from - and it is worth reading before designing ours. Its
consolidation-as-belief-strength (evidence count + quotes, strengthen/
weaken/extend) also points at how R3/R4 could carry *confidence*, not
just supersession order.

**R6 - Local embeddings via ONNX (medium; already reserved).**
`models/` has been reserved for exactly this since M9.5 planning.
Competitors ship all-MiniLM locally by default; it removes the provider
dependency from vector search and makes hybrid retrieval a zero-config
default rather than an opt-in. The `ort` crate is the known path.

**R7 - Progressive-disclosure retrieval/brief tiers (medium; evaluate
first, design doc before code).** From OpenViking's L0/L1/L2 model, whose
reported 34-91% input-token cut is almost entirely deferring full content.
We already have the pieces: `memory_query` returns ~24-word snippets (an
L0), `memory_read_page` returns the full body (an L2), and V61 abstract
embeddings + the session brief exist. The gap is an explicit **L1
"overview" tier** and, more concretely, a **tiered on-start brief** that
leads with per-page abstracts and expands to full bodies only on demand
instead of loading core-page bodies up front (`render_session_brief`
today spends its whole char budget on L2). This is a token-efficiency
win on the exact hot path — every opted-in session start — with no new
storage: reuse the abstract already embedded at V61. Worth a design doc
that decides where the L1 overview comes from (frontmatter summary,
first-paragraph extraction, or the existing abstract) before any code;
it composes with R5 (the cross-session abstraction pass produces good
L1 summaries) rather than competing with it. **Partly realized on 2.4:**
extractive tier-down (A2, #808) already persists the L0 abstract + L1
summary + L2 keep-token shape for cold episodic pages
([`design-memory-aging.md`](design-memory-aging.md)); the remaining R7 work
is the *tiered on-start brief* that leads with abstracts, still a design
doc before code. Directory-scoped retrieval
(OpenViking's TrieHI) is the lower-priority half — a path-prefix filter
on `memory_query` within a project — worth noting but not scheduling
until R7's tiering lands.

**R8 - Queryable metadata/tag filters + a published eval triple (small-
medium; from Supermemory).** Two borrowable ideas from the entrant that leads
on ergonomics. First, **structured metadata/tag filtering on `memory_query`**:
we already key every row by the `(workspace, project, path)` 3-tuple, but
Supermemory's `containerTags` + arbitrary metadata filters let a caller narrow
recall by attributes (kind, tier, author, tag) *before* ranking. A bounded,
optional filter argument on `memory_query` - composing with FTS5/RRF, not
replacing it - closes a real query-ergonomics gap at low cost; most of the
plumbing (page frontmatter, kinds/tiers, `_slots`) already exists. Second,
**publish an accuracy / latency / context-tokens triple** from the in-repo
`recall_eval.rs`, mirroring Supermemory's open **MemScore / MemoryBench**
philosophy: honest engineering discipline and the right answer to a field now
competing on self-reported single numbers - it composes with R2 (the
reproducible-harness recommendation) rather than competing. A per-project or
per-user **"profile" page** auto-surfaced on start (Supermemory's standing
profile) is a lighter third idea that maps onto our durable pages + the session
brief - worth noting, below the two above. Deliberately *not* borrowed from this
camp: managed cloud connectors and LLM-required extraction in the **core** - if
connectors ever land they belong in the companion importer as opt-in, outside
the trusted boundary, so the file-first / zero-LLM model is preserved (the same
line drawn for multimodal/PDF import: OCR/transcribe to markdown pages, never an
opaque store). On the other two axes the verdict is blunt: **security - nothing
to borrow** (Supermemory is weaker on every axis we chose - opaque store,
cloud-first, LLM-required, `containerTag` isolation vs our 3-tuple + invariant
#16 - so this pass is confirmation of our posture, not a source of borrowings;
the only security-relevant note is defensive: any future importer connector must
cross the existing sanitizer boundary), and **performance - only f16/
half-precision embedding storage is worth *evaluating*** (a stored-format change
with a real recall-vs-size tradeoff, gated on `recall_eval.rs`), while their
"sub-300ms p50" is a publish-a-number contract, not a change we need. Ranked, the
only thing worth actually building is the metadata/tag filter; the eval triple is
worth doing because it de-risks measuring anything else; f16 is evaluate-only;
the rest is hold-or-reject. No implementation is scheduled here.

**Deliberately not recommended:** joining the memory-OS camp (agent
self-editing its memory - token-expensive, and Letta itself is hedging);
adopting a graph database (bi-temporal-lite on SQLite covers the useful
part); spatial/metaphor architectures (mempalace); publishing any
benchmark number before R2 exists; chasing agentmemory's tool-count
(the May conclusion stands - a sharp small surface is the advantage).

## 6. Sources

- OKF: Google Cloud blog "How the Open Knowledge Format can improve data
  sharing"; document360.com and mindstudio.ai explainers (spec v0.1,
  2026-06-12).
- agentmemory: github.com/rohitg00/agentmemory (README, CHANGELOG,
  docs/benchmarks/2026-05-20-coding-agent-life-v1.md).
- mcp-memory-service: github.com/doobidoo/mcp-memory-service (README,
  Wiki, v11.10.0 release notes, issue #27).
- mempalace: github.com/MemPalace/mempalace (BENCHMARKS.md, issue #29);
  arXiv:2604.21284 "Spatial Metaphors for LLM Memory: A Critical
  Analysis of the MemPalace Architecture".
- Hindsight: github.com/vectorize-io/hindsight (README, MIT);
  arXiv:2512.12818 (Latimer, Boschi, Neeser, Bartholomew, Srivastava,
  Wang, Ramakrishnan - "Hindsight is 20/20", preprint 2025-12-14);
  vectorize.io/blog "Introducing Hindsight"; PRNewswire "Vectorize
  Breaks 90% on LongMemEval"; VentureBeat 91%-accuracy coverage.
  LongMemEval reproduction attributed to Virginia Tech Sanghani Center +
  The Washington Post (co-developing collaborators, not arms-length).
  Full analysis: [`research-hindsight.md`](research-hindsight.md).
- OpenViking: github.com/volcengine/OpenViking (README, `ov` CLI docs,
  AGPLv3 core / Apache-2.0 examples); blog.openviking.ai benchmark-results
  post + `./benchmark/` reproduction scripts (LoCoMo, tau2-bench,
  in-house); arXiv:2605.29640 (VikingMem, VLDB 2026) plus Directory-Aware
  Query and VikingRAG papers (submitted/unreviewed). Analyzed inline in §3
  per the no-standalone-doc convention.
- Supermemory: github.com/supermemoryai/supermemory (README, MIT, ~30K
  stars); supermemory.ai/docs (how-it-works, graph-memory, self-hosting,
  connectors, filtering); github.com/supermemoryai/{memorybench,
  opencode-supermemory, supermemory-mcp}; supermemory.ai/blog (memory-engine,
  supermemory-vs-mem0 - vendor self-reported benchmarks); DeepWiki
  code-derived architecture; TechCrunch/Dataconomy 2025-10 (seed funding,
  founder). Hosted backend is closed-source, so its storage internals,
  latency, and benchmark numbers are vendor claims, not primary-verifiable.
  Analyzed inline in §3 per the no-standalone-doc convention.
- Honcho: github.com/plastic-labs/honcho (README, AGPL-3.0, ~7K stars,
  coding-agent integrations + MCP tools) and its in-repo CLAUDE.md
  (authoritative architecture: Postgres+pgvector/FTS, Redis, Deriver/
  Dreamer/Dialectic, peer `(observer, observed)` collections, LanceDB/
  Turbopuffer); honcho.dev/docs + docs.honcho.dev (peers, representations,
  dialectic endpoint); plasticlabs.ai/blog/research/Benchmarking-Honcho +
  evals.honcho.dev (vendor self-reported LongMemEval/LoCoMo/BEAM numbers,
  2025-12-19); panews.io $5.35M pre-seed coverage (+ decentralized-identity
  roadmap, absent from the repo); andrew.ooo independent review;
  hermes-agent.nousresearch.com Honcho integration. Benchmarks are
  vendor-reported and not independently reproduced. Analyzed inline in §3
  per the no-standalone-doc convention.
- LiquidLM: liquidlm.com (+ /about, /docs, /docs/assistants, /docs/mcp — Vaults,
  MCP tool surface, per-harness support with test dates), github.com/liquidlm
  (org: an archived empty `cli` repo + a homebrew tap, no engine source), and the
  npm registry metadata for `@liquidlm/cli` (MIT, v0.1.5 published 2026-09-17).
  Solo maker (Carlos Souza), closed-source cloud engine, no benchmarks; the
  backend internals, model, and any numbers are undisclosed/vendor framing.
  Analyzed inline in §3 per the no-standalone-doc convention.
- Engram (issue #810): github.com/Gentleman-Programming/engram (README —
  SQLite+FTS5, Go single binary, MCP stdio, operating contract /
  `mem_session_summary`, git-sync, Engram Cloud; MIT) and the GitHub metadata
  endpoint (~6.8K stars, commit 2026-09-22, release v2.0.0). No benchmark
  published. Analyzed inline in §3 per the no-standalone-doc convention.
- EverOS (issue #810): github.com/EverMind-AI/EverOS (README — Markdown source
  of truth + SQLite/LanceDB indexes, user/agent tracks, Knowledge Wiki, offline
  consolidation, minimal-tier LLM-required, OpenRouter default; Apache-2.0) and
  the metadata endpoint (~13.1K stars, commit 2026-09-09, release v1.3.1). No
  benchmark published. Analyzed inline in §3 per the no-standalone-doc convention.
- memU (issue #810): github.com/NevaMind-AI/memU (README — shared LLM wiki,
  Markdown skill distillation, SQLite/Postgres+pgvector, host-instruction-file
  sidecars, no MCP; LICENSE.txt = Apache-2.0) and the metadata endpoint (~14.4K
  stars, commit 2026-09-21, release v1.5.1 2026-03-23 so releases lag); managed
  cloud at memu.so. No named benchmark. Analyzed inline in §3 per the
  no-standalone-doc convention.
- Caura (issue #810): github.com/caura-ai/caura (README — governed multi-agent
  fleet memory on Postgres+pgvector, visibility scopes, four trust tiers, audit,
  PII flagging, KG, MCP `/mcp`; Apache-2.0) + managed caura.ai, and the metadata
  endpoint (~0.5K stars, commit 2026-09-22, release backend-v3.17.2). Benchmarks
  (LoCoMo 77.6% / LongMemEval 92.2%, "23 ms p50," eToro "300+ agents") are
  README-stated and vendor self-reported, not independently verified. Analyzed
  inline in §3 per the no-standalone-doc convention.
- TencentDB Agent Memory (issue #810): github.com/TencentCloud/TencentDB-Agent-
  Memory (README — Chat Memory/Skill/Wiki/CodeGraph assets, proxy fan-in base-URL
  interception, Node/TS, SQLite default + experimental MongoDB, harness list;
  LICENSE = MIT despite the GitHub NOASSERTION detection) and the metadata
  endpoint (~27.2K stars, commit 2026-09-22, release v2.0.1). PersonaMem
  48%→76% is vendor self-reported. The name does **not** imply a Tencent DB
  dependency (SQLite by default). Analyzed inline in §3 per the no-standalone-doc
  convention.
- Zep/Graphiti: arXiv:2501.13956; getzep.com temporal-KG explainer;
  Neo4j "Graphiti: Knowledge graph memory for an agentic world".
- Letta: "Is a Filesystem All You Need?" (letta.com blog, Aug 2025).
- Claude Code auto memory: blog.memoryplugin.com/claude-code-memory,
  thepromptshelf.dev guide (v2.1.59 default-on; ~/.claude/projects/
  <project>/memory layout).
- Papers: arXiv:2605.19952 (TriMem), arXiv:2605.06716 (Storage→
  Experience survey), arXiv:2605.12493 (LongMemEval-V2), MIRIX
  (Semantic Scholar), MemOS/EverMemOS listings.
- Landscape comparisons: cognee.ai, vectorize.io, atlan.com,
  particula.tech, mnemoverse.com 2026 roundups (read as marketing;
  cross-checked against the projects' own repos where load-bearing).
- In-repo design of record for the 2.4 memory-aging borrowings (per-tier
  decay, extractive tier-down, cold-cluster dedup, contradiction band,
  belief-strength, the dream pass, access-weighting):
  [`design-memory-aging.md`](design-memory-aging.md) (with a sourced
  competitor-grounding table) and
  [`design-hindsight-borrowings.md`](design-hindsight-borrowings.md) §3
  (belief-strength). Migration verdicts:
  [`competitive-parity.md`](competitive-parity.md).
