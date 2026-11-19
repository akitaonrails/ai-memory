# Agent-Memory Landscape, October 2026 - Research Report

> Follow-up to the May 2026 research pass (`research-agentmemory.md`,
> `research-basic-memory.md`, `research-cognee.md`,
> `research-karpathy-llm-wiki.md`, and the `issues-*.md` tracker mining).
> Same lens as then: what the competition does, what is worth borrowing,
> what to deliberately avoid. This is analysis only: no implementation decisions
> are made here, and none of the recommendations below imply drastic
> change. Sources at the end; every load-bearing claim was checked against
> a live page in September 2026, not remembered.
>
> **Refreshed 2026-10-18.** Every tracked competitor was checked against its
> official repository, releases, package metadata, or product documentation.
> The [2026-10-18 delta ledger](#refresh-2026-10-18-one-by-one-delta) records
> both material changes and explicit no-change findings. Older corrections
> remain in place so the report keeps its audit trail.

## 1. The headline: our architectural bet got standardized

On June 12, 2026, Google Cloud published the **Open Knowledge Format
(OKF) v0.1**: organizational knowledge as a plain directory of markdown
files with YAML frontmatter, one concept per file, a single required
frontmatter field (`type`), no SDK, no runtime, vendor-neutral. It is an
explicit formalization of Karpathy's "LLM wiki" (the same gist our
project was built from), and it is already an ecosystem signal: new
memory servers advertise themselves as "OKF-backed", and the format is
positioned as the interop layer that makes agent memory portable across
tools.

Our `wiki/` is now a native **OKF v0.2 bundle**: markdown, YAML frontmatter,
one page per concept, with generated `index.md` and `log.md` projections and
an `ai-memory okf check` conformance command. The interoperability work is
already shipped.

The second validation came from an unexpected direction: **Letta**
(formerly MemGPT) published "Is a Filesystem All You Need?" arguing that
agents post-trained for iterative file search perform well against
specialized memory systems (74.0% on LOCOMO with plain files). Letta has
since gone further. Since February 2026 ("context repositories") every
Letta agent's memory is a git repository of Markdown files with YAML
frontmatter and a `MEMORY.md` index, with no vector index by default. That
is the same substrate we chose, so it is no longer a concession. (Updated
2026-10-13; see the Letta entry under [the 2026-10-13 refresh](#refresh-2026-10-13-what-changed).)

## 2. How the original research subjects evolved

**agentmemory (rohitg00)** is actively maintained and maturing along the
trajectory the May report predicted. Notable since then: an in-house
benchmark, "coding-agent-life-v1" (15 sessions and 15 queries; hybrid R@5
1.000 after a 2026-06-03 correction, grep 0.967), `AGENT_ID` multi-agent
isolation with an opt-in isolated recall scope, and runtime cost warnings
when a premium model is configured for compression. Its LongMemEval-S figure
is still 95.2% `recall_any@5`, measured on whole raw sessions in a component
harness with the reranker disabled; a 2026-10-09 baseline adds a stricter
metric and archived receipts. Still on the iii-engine + JSON-in-KV
substrate the May report identified as its structural constraint, now with
an opt-in Redis backend, vectors kept in buckets with a pending log, and the
BM25 index rebuilt at every boot. The tool surface grew from 53 to 54, and
all 54 are now visible by default. Version 0.9.30 (2026-10-06) turned auth
on by default with a generated secret and added a durable capture inbox
with an offline hook spool. (Corrected 2026-10-10: this entry used to give
the benchmark as 635 requests / 888K tokens / 35 hours with hybrid R@5 =
0.967. Those workload figures could not be found in agentmemory's
repository, and 0.967 is now the grep baseline.) The 2026-10-09 baseline,
on the cleaned LongMemEval-S set (470 answerable questions), reports BM25 at
84.68% strict recall-all@5, 91.72% fractional recall@5 and 452/470 (96.2%)
any-hit; the authors state it supports no competitive claim, and its
harness differs from ours. As of 2026-10-13 there is no release after
v0.9.30; fixes for a cross-project leak in smart search (found by its own
test suite, fixed 2026-10-10) and for session summaries cut at turn 1 are on
`main`, unreleased. Practices worth noting: a client-side spool for hook
events, archived run receipts, a strict recall-all@5 metric, and a release
gate that installs the packaged artifacts and runs capture and recovery.

**basic-memory** is alive, with the same positioning (structured markdown both
humans and LLMs edit; local files). No architectural surprises. Its
niche overlaps most with OKF, which may absorb its differentiation. Its
unreleased v0.24 changelog now describes native OKF v0.2, optimistic writes,
structured and cross-project filters, document extraction, temporal
qualifiers, a local reranker, locked notes, and cloud team collaboration.

**cognee** has moved into content-led growth ("best memory
framework" listicles on its own blog rank for every comparison query)
and advertises a broad integration matrix (an earlier "14 retrieval modes"
note here could not be re-verified on 2026-10-13). Technically still the
triple-store (relational + vector + graph) ECL pipeline the May report
described, but no longer heavy to stand up: v1.6.0 runs without an LLM key
on embedded SQLite + LanceDB + Kuzu files, records the embedding model per
dataset, and adds a `cognee-mcp` package. v1.6.1 added Google Drive/Gmail
sync, v1.6.2 Slack sync and chunk sizing to the embedding model's limit,
v1.6.3 temporal search windows and ranking. It now has a Claude Code plugin
(`cognee-integrations`, v1.6.6): a local server on `localhost:8011` by
default or Cognee Cloud, `claude -p` on the user's Claude subscription when
no key is set, a statusline, and a repo code-graph indexer. Per its 1.6.6
changelog the plugin is listed in Anthropic's official plugin directory. Its
BEAM report scores 0.79 on 20 questions from one conversation and 0.67 at
10M, tuned and scored on the same questions, with gpt-5 for ingestion,
answering and judging.

**mempalace** is the cautionary tale of the year. It reached ~47K stars within two
weeks of its April 2026 launch on the strength of a claimed 96.6% R@5 on
LongMemEval; then an independent audit (its issue #29, corroborated by
`mcp-memory-service`'s issue #27 analysis and a critical-analysis paper,
arXiv:2604.21284) showed the number reproduces with a **minimal ChromaDB
default setup with the palace architecture inactive**. The spatial
metaphor (wings/rooms/closets/drawers) added nothing measurable.
Lessons: (a) benchmark claims are now audited by the community, quickly
and publicly; (b) metaphor-driven architecture is not retrieval
architecture; (c) stars measure virality, not substance. Update 2026-10-13:
MemPalace is at v3.10.0 and ~59.5K stars, and its README now splits the
numbers: 96.6% R@5 on the raw setup and 98.4% on a held-out set of 450
questions. Those are retrieval-only numbers, the closest in kind to our
`hit@5`, though from a different harness. It now ships hooks for Claude
Code, Codex and Cursor, a team server, and a temporal graph, which puts it in
the coding-agent hook-capture camp below.

## 3. The 2026 landscape beyond the original subjects

The market has consolidated into recognizable camps:

| Camp | Representatives | Core idea |
|---|---|---|
| Coding-agent hook capture | **claude-mem**, **MemPalace**, agentmemory; coding plugins from Mem0, Supermemory, Hindsight, OpenViking, Cognee | Lifecycle hooks in the coding harness capture sessions and inject context at start (some on every prompt); the most-starred part of the field as of 2026-10 (see the refresh below) |
| Temporal knowledge graphs | **Zep/Graphiti** (~31.7K stars as of 2026-10-13), Cognee | Facts as bi-temporal graph edges: *when true in the world* vs *when observed*, superseded rather than deleted |
| Memory OS / self-editing | **Letta** (now Letta Code), MemOS, MIRIX | The agent edits its own tiered memory via tools; "sleep-time compute" does consolidation off the hot path. Letta's memory is now a git repo of markdown files (see the refresh below) |
| Fact extractors | **Mem0**, LangMem | LLM extracts atomic facts; lightweight personalization. Mem0 now fuses semantic + BM25 + entity retrieval and ships coding-agent plugins |
| Hosted memory API (hybrid) | **Supermemory**, **LiquidLM** | Chunk-RAG + LLM temporal fact-graph + per-user profiles, answered in one query; managed connectors + multimodal; cloud-first (see below) |
| User-modeling / theory-of-mind | **Honcho** (Plastic Labs) | Memory as a *reasoning* problem: derive what each "peer" knows/believes about another over time; model the human, not the project (see below) |
| **File-first wiki memory** | **us**, basic-memory, OKF, Letta's context repositories, Supermemory's memoryrepo | Markdown source of truth, derived indexes, human-editable |
| File-first wiki (LLM-leaning) | **memU**, **EverOS** (formerly listed separately here as EverMemOS) | Markdown source of truth + derived vector/SQLite(+LanceDB) indexes, but LLM-required for the capture/distill flow; skill-distillation and offline consolidation (see below) |
| Self-hosted coding-agent store | **Engram** | Single Go binary, SQLite+FTS5, MCP, project scope, session-summary handoffs; a structured-row store captured via agent MCP calls, not lifecycle hooks (see below) |
| Governed multi-agent / fleet memory | **Caura** | Shared fleet memory with visibility scopes, trust tiers, audit, governance on Postgres+pgvector; org-scale, not project-scale (see below) |
| Shared memory server (proxy fan-in) | **TencentDB Agent Memory** | One proxy in front of many harnesses distilling Chat Memory / Skill / Wiki / CodeGraph; proxy interception, not MCP/hooks (see below) |
| Code intelligence | **DeusData/codebase-memory-mcp** (~46.5K stars as of 2026-10-13) | Index the *codebase* (162 languages, tree-sitter → SQLite graph, static C binary) rather than the *session*. Adjacent, not competing: it remembers what the code is, not what you did (see `research-codebase-memory-mcp.md`) |
| Agent-harness OS | **ECC** (~247K stars), plus the skills/agents-pack ecosystem | Install a whole plan→test→implement→review→**remember**→improve loop into the agent; memory is one thin pillar ("optimize the context window, persist everything else"), deliberately kept as *context, not policy*. Adjacent, not competing (see `research-ecc.md`) |

**Fact-row store with hook capture: `doobidoo/mcp-memory-service`** (1.9K
stars, 3.2K commits, active through August 2026). It is what we would be
if we had chosen fact-rows over wiki pages: SQLite(+vec) storage, local
ONNX embeddings (all-MiniLM-L6-v2, no provider dependency), hook-driven
capture for Claude Code, autonomous scheduled consolidation
(decay + LLM compression + DBSCAN/hierarchical clustering + "belief
derivation" over typed edges), a **typed knowledge graph**
(`causes` / `fixes` / `contradicts` edges), multi-backend sync
(local-first with optional Cloudflare replication), and inter-agent
messaging implemented as tagged memories on the shared pool. Its issue
tracker repeats patterns from our own history: v11.10.0 fixed
consolidation time horizons that *silently ignored their configured
window*, the same class of quiet-lifecycle bug our `#526`/`#528` work
addressed with observable outcomes. Published honest numbers: 80.4% R@5
on LongMemEval turn-level, 86.0% session-level. **Where we now stand
(2.4):** its consolidation set is type-dependent TTL (365/180/90/30),
extractive compression, DBSCAN adaptive-eps clustering, access/connection
boosts, and 0.4–0.75-band contradiction detection. That set is the direct
grounding for the 2.4 memory-aging program
([`design-memory-aging.md`](design-memory-aging.md)), which now ships each
at parity but **zero-LLM by default, reversible** (supersede-not-delete +
`restore-page`), and **off by default** (per-tier `[decay.half_life_days]`,
extractive tier-down, cold-cluster dedup, access reinforcement on all read
paths, and a `memory_lint` contradiction finding). The difference is
posture, not mechanism: theirs runs autonomously; ours preserves the
zero-LLM default path and evicts nothing on upgrade.

**Platform development: Claude Code native "auto memory"** shipped
default-on (v2.1.59 per third-party guides; the official docs name no
version): the agent keeps a `MEMORY.md` index plus one typed topic file per
memory, per git repository, loading the first 200 lines or 25 KB of the
index each session and reading topic files on demand (checked against the
official docs on 2026-11-14, see
[`research-anthropic-agent-memory.md`](research-anthropic-agent-memory.md)).
Its documented limits are our differentiators: machine-local
with **no sync**, single-agent (nothing carries to Codex/Cursor/others,
and subagents other than forks do not load it), repo-scoped, no search
beyond reading files, no capture of tool
lifecycle, no team story. Two readings, both true: the basic solo
use case ("remember my project between sessions on this laptop") is
being absorbed by the platforms; and the platforms are training users
to *expect* persistent memory, which makes the cross-machine,
cross-agent, multi-user version (what v1.39.0 hardened) the durable
value. Native memory is the funnel, not the competitor.

**Paper-backed pages: `vectorize-io/hindsight`** (~23.5K stars in
September, ~49.1K on 2026-10-13; MIT, a company behind it, a preprint paper
arXiv:2512.12818, and a LongMemEval claim of 91.4% at the time, 94.6% on its
own harness as of 2026-10). It is useful because it *agrees with our core
bet from the opposite substrate*: Hindsight is Postgres/pgvector-primary
and needs an LLM (local models work; see the update below), yet its top memory tier is **"mental models": living
markdown pages of settled knowledge that a background process
continuously rewrites as the bank learns**, which an agent "boots" from
instead of rediscovering context each session. That is our `wiki/` +
auto-improve loop under another name, arrived at independently and backed
by a paper. It is the strongest external validation of pages-over-facts in
this whole research series. Two more things to carry forward: its
consolidation is **belief-strength, not binary supersession** (evidence-
backed observations with quotes + a proof count; new evidence
strengthens/weakens/extends rather than replaces), a richer model than
ours; and its **"bank" = per-(user|agent|project) isolation, no
cross-bank leakage**, which as of 2026-10 also carries user/team/company
scopes *inside* one bank, so teams can share within a bank. It was the
natural prior art for issue #708
(per-project authorization) and #709 (project identity), where the design
should layer a bank-like boundary *above* sharing rather than replacing
it. Read skeptically where it earns it: the "independently reproduced"
LongMemEval number is from the *co-developing* labs (two of seven authors
are Virginia Tech Sanghani faculty; the Post is a named collaborator), it
is a preprint, and it reports accuracy rather than the R@5 others quote.
It is still the most credible benchmark claim here (a paper with per-category
sub-scores, gains concentrated where structured temporal memory should
help), and a reminder the serious players now publish numbers. **Where we
now stand (2.4):** the two ideas flagged to carry forward (belief-strength
over binary supersession, and the background rewrite loop) now ship, as
opt-in layers over the zero-LLM core, per
[`design-hindsight-borrowings.md`](design-hindsight-borrowings.md) §3 and
[`design-memory-aging.md`](design-memory-aging.md) B1–B4. Belief-strength is
a read-time, zero-LLM `confidence` (distinct-session breadth + recency +
live `contradicts`, capped) exposed inertly in `explain`/`status` and
foldable into ranking behind a **default-0.0, R2-gated** weight; the
"dream" pass is an idle-scheduled, cancel-on-activity, surprisal-first LLM
rewrite of cold clusters that is **off by default and never deletes a
source**. We designed *against* Hindsight's own documented failure,
belief *entrenchment* (raw proof-count pins a wrong page), with
breadth-not-count weighting, a confidence cap, and supersession always
winning over evidence. Honest caveat: no R2 delta has shown a gain yet, so this is
parity of *mechanism*, not a measured accuracy win. Full analysis in
[`research-hindsight.md`](research-hindsight.md).

*Update 2026-10-13 (v1.0.0, released that day).* A `hindsight-coding-agents`
package installs hooks in 16+ coding agents and ingests the repository's
git history; every agent in a repo shares one bank
(`coding-agent::{gitProject}`), with entries tagged by agent.
`hindsight fs mount` gives a one-way markdown mirror of the knowledge pages
held in the database. A reranker is on by default, there is image and file
ingestion and secret/PII redaction, and the LLM can be local (Ollama,
llama.cpp) or come from a Claude, ChatGPT or Copilot subscription, so
"LLM-required cloud" no longer describes it. The headline is now
LongMemEval-S 94.6% (was 91.4%) on its own AMB harness
(github.com/vectorize-io/agent-memory-benchmark): Gemini answers and judges
(the exact model is unverified; the harness defaults to
gemini-2.5-flash-lite), with about 43.6K context tokens per question. It also
reports LoCoMo 92.0% and BEAM-10M 64.1%. Its own coding benchmark shows no
gain from memory (vanilla 0.98-1.0, the same with Hindsight).

**Context database: `volcengine/OpenViking`** (~37.5K stars in September,
~39.8K on 2026-10-13; ByteDance/Volcano-Engine, AGPLv3 core + Apache-2.0 CLI/examples, three
papers incl. **VikingMem** at VLDB 2026, arXiv:2605.29640). Positioned as a
"self-evolving context database" that unifies memory + RAG + skills. It needs a
VLM for extraction/compilation (Ollama works) over a vector store fronted
by a `viking://` virtual filesystem, so it sits in a different corner from
our zero-LLM default. (Corrected 2026-10-13: embeddings are now local and
keyless by default, per its 2026-10-03 blog; the core is a self-hostable
AGPL server; and its memories are markdown you can inspect and edit.) Like
Hindsight, it validates our substrate from there: memory is
document/directory-scoped, not atomic facts, and a background pass
(*commit session → extract → compare candidates for create/merge/skip*, and
optional VikingBot "compile" into wikis/KGs) is our consolidation +
auto-improve loop under other names. Two ideas are worth carrying
forward. First, **progressive-disclosure tiers**: every memory is loadable
at **L0 (one-sentence abstract, for relevance checks)**, **L1 (overview, for
planning)**, or **L2 (full original, read only when needed)**. The reported
34-91% input-token reduction (LoCoMo) is almost entirely this deferral, and
it maps directly onto work we already have (V61 abstract embeddings, the
session brief) rather than a new architecture. 2.4's **extractive
tier-down** (A2, [`design-memory-aging.md`](design-memory-aging.md)) now
persists exactly this shape for cold episodic pages: an L0 abstract, an L1
summary, and an L2 keep-token set, with the full prose one supersession-
restore away. Second, **directory-scoped
retrieval**: in September this was a "TrieHI" prefix-tree recursion; v0.4.23
replaced the recursion with one global recall, one rerank pass and subtree
filters, which is still a cheaper cousin of our per-project scoping, one
level down at the `_rules/` / `norms/` path prefix. Read the numbers
with the same skepticism the rest of this report earns: the LoCoMo lifts
(24->82%, 33->83%, 57->80%) are **in-house** (Volcengine) against
agent-native/stateless and simple-RAG baselines, not against other memory
systems, and the papers are VLDB-2026/submitted, not yet peer-reviewed.
What to avoid: VLM-required capture, AGPLv3 on the core, and the
SaaS/enterprise-licensing weight that pulls against a single self-contained
binary.

*Update 2026-10-13.* v0.5.0 (2026-10-09) adds a Gateway (#5625): a proxy on
port 1935 that speaks the Anthropic Messages, OpenAI Chat Completions and
Responses APIs, injects recalled memory, replays requests byte for byte so
prompt caches stay valid, compacts at 90% of context, and runs MCP tools
server-side, read-only by default. It ships plugins for Claude Code, Codex,
Cursor, TRAE, OpenCode, Kimi and pi, plus an `ov-kanban` skill that hands
tasks between agents as markdown files (with no claim-once guard). Working
Memory is off by default, and the plugin commits every message to the
archive (#5459). v0.4.23 also added BM25 (#5456), removed hotness weighting
(#5454), added event time-decay (#5214) and unified capture sanitizing across
plugins (#5359); v0.4.22 added ACLs and user groups, per-account models and
vector DB, a Kimi Code plugin and OpenCode v2 support. No new benchmark has
appeared since the 2026-05-29 blog (v0.3.22), and its LoCoMo lifts are
measured only against each agent's built-in memory. Per the research-doc
convention, OpenViking has no standalone deep-dive; this section is its
record.

**Hosted memory API: `supermemoryai/supermemory`** (~30K stars in
September, ~31.2K on 2026-10-13; MIT repo, ~$2.6-3M seed from Susa/Browder/SF1.vc plus angels incl. Jeff Dean,
Cloudflare's CTO, and Logan Kilpatrick; founder Dhravya Shah). Positioned as
"context infrastructure for AI agents": a **hosted memory API** (the paid
`api.supermemory.ai`) with a self-hostable server binary as the on-ramp.
(Corrected 2026-10-13: the repository is MIT, but its self-hosting docs say
"the server binary is not open source"; it is built from non-public code,
is "free within its lite license limit", lacks the hosted MCP server, and
has telemetry on by default.)
This is the entrant our own docs most **mislabeled**: it is *not* a Mem0-style
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
leads is **ingestion breadth**: managed connectors (Drive, Gmail,
Notion, OneDrive, S3, GitHub, web crawler), multimodal PDF/image/audio, and
metadata + `containerTag` query filters, plus a first-class user profile. But
every one of those is **cloud-only and LLM-required**; the self-hosted binary
(embedded graph store, local `bge-base` embeddings, offline) explicitly "lacks
connectors and the proprietary extraction models." It sits opposite us on every
axis we chose: by default an **opaque graph is the source of truth** (no git,
no markdown, nothing to `grep` or diff; the separate memoryrepo project below
is the exception), the quality path is **LLM-required** (no zero-LLM
default), and the best features **pull toward the paid Cloudflare cloud**. It is
also not really in our lane: it is a general **memory-API for apps and user
personalization**. Its coding-agent plugins now capture through lifecycle
hooks (corrected 2026-10-13; an earlier note here said turn-batch polling):
the Claude Code and Codex plugins use SessionStart, UserPromptSubmit (which
searches and injects matches itself), PreToolUse and Stop, and Claude Code,
Codex and OpenCode share one memory container per repository, keyed on the
normalized git remote. There is still no handoff protocol, and isolation is
a tag rather than a scoped identity. A separate memoryrepo project keeps
git-backed markdown memory and rejects a stale write by returning the
current version. Read the numbers with the standard skepticism: "95% LongMemEval_s at
Recall@15 with aggregation, ~720 tokens, sub-300ms p50, first on LoCoMo" are all
**vendor self-reported**, and they do not agree with each other: the README
says 95% Recall@15, the research page says 97% "Recall@20 with aggregation"
while gpt-4o judges the answers and the same 97% appears in its LLM-judge
table (it reads like a judged answer score labelled as recall), "#1 on LoCoMo
and ConvoMem" comes without numbers, and the smfs file-mount token claim is
3x fewer tokens on 110 questions in the README but -36% (Claude) and -83%
(Codex) on a 20-task run at smfs.ai. Their **MemoryBench** harness and the
accuracy / latency / context-tokens "MemScore" triple are open and worth
borrowing (see R8). Per the research-doc convention, Supermemory has no
standalone deep-dive; this section is its record.

**Solo-built Supermemory-style hosted API: `liquidlm.com`** (**LiquidLM**,
closed-source cloud SaaS, no funding, built by a single independent maker, Carlos Souza).
Its own tagline is "persistent AI agent memory and RAG as a service." It is the
same *camp* as Supermemory (a proprietary hosted memory API with entity/
relationship extraction, temporal metadata, and an MCP surface on top), but a
smaller, indie, individual-priced take on it (Free / $20 Pro / $200 Ultra,
usage-based by ingestion volume). You ingest files/notes/links/media into cloud
**"Vaults"**; it extracts entities, relationships, timestamps, and **staleness/
`supersedes` signals**, then serves hybrid semantic+FTS retrieval with rerank and
grounded, cited chat. It is on **every axis we deliberately chose against**: the
**vault is an opaque hosted store** (no git, no markdown, no self-host, nothing to
`grep`/diff), **LLM is intrinsic and not optional** (Gemini-class extraction/
embeddings, no zero-LLM mode), and the memory engine is **closed-source** (the
only open artifact is the MIT npm client `@liquidlm/cli`, stable v0.1.6, with
v0.2.0-alpha.1 as a prerelease; the GitHub `cli` repo is archived and empty).
Where it is
interesting is **breadth of ingestion and a clean MCP tool surface**: heavy
multimodal capture (video/audio/image/PDF/Office, auto-transcription, a
`yt-dlp`-fetch-locally pattern), a GitHub repo-sync connector, and eight native
MCP tools worth studying by name: `put/get/list/search_knowledge`,
`list_entities`, `get_related`, **`follow_references`** (walk the citation graph),
**`pin_knowledge`** (standing context surfaced *before* search), and
`forget_knowledge`. Tested harnesses: Claude Code, ChatGPT, Codex CLI, OpenCode
over a hosted MCP endpoint (OAuth or scoped PAT). **No retrieval benchmarks** are
published (only ROI marketing). It is **not in our lane and a weak migration
target either way**: it captures by *explicit upload/sync* into a general "second
brain," where we capture *automatically via lifecycle hooks* compiled into
per-project coding pages you own. It is a different job for a different buyer. Two ideas are
worth borrowing conceptually (never the substrate): the explicit **`pin_knowledge`
= "seen before they search"** contract over our existing `pinned` pages, and
**`follow_references`/`get_related` as first-class graph-walk MCP tools** over the
link-neighbor RRF we already compute internally. Per the research-doc convention,
LiquidLM has no standalone deep-dive; this section is its record.

### New entrants raised in issue #810 (verified 2026-09-22)

Five projects flagged in issue #810, each researched from its own repo/README
(and, where the name could mislead, the LICENSE and the metadata endpoint) rather
than from a name search, a failure mode that has burned this series before.
None displaces the camp taxonomy; they slot into it, and two are close enough to
be honest migration conversations.

**Self-hosted coding-agent store: `Gentleman-Programming/engram`** (~6.8K stars, MIT, single
Go binary, commit 2026-09-22, release v2.0.0). Engram works in our lane:
persistent memory for AI coding agents over **SQLite + FTS5** in a
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
It is a structured store in the mold of mcp-memory-service, written in Go
and captured through MCP calls rather than hooks. **No retrieval benchmark is published.** Per the
research-doc convention, Engram has no standalone deep-dive; this section is its
record.

**Local-first, file-first: `EverMind-AI/EverOS`** (~13.1K stars,
Apache-2.0, Python 3.12+, commit 2026-09-09, release v1.3.1). Architecturally
it is a **local-first memory runtime** with **canonical
Markdown as the source of truth** ("readable, editable, diffable, and
Git-versioned") over derived **SQLite + LanceDB** indexes, direct file edits
cascading through watchers. That is the same files-as-truth + derived-index split we
chose. It adds dual first-class tracks (a user `episodes/profile` and an agent
`cases/skills`), an editable **Knowledge Wiki** with taxonomy + CRUD APIs, and
**offline memory evolution** that merges episode clusters and refines profiles,
which is our consolidation/auto-improve loop under other names. It validates the substrate,
but from the LLM-leaning corner: the minimal tier still **requires an LLM for the
core flow** (embedding/reranking optional, OpenRouter the default; a zero-LLM
demo exists but is not the operating default), it is a Python stack rather than a
single binary. *Update 2026-10-18 (v1.4.2, ~13.5K stars):* native Windows
support, an ANN vector index and an optional Milvus backend. v1.4.2 also
separates foreground query embedding from background writes, batches
embeddings, self-repairs index damage, and hardens full-scope keys and path
containment. An official
`everos-mcp` server (EverMind-AI/EverOS-MCP) was documented on 2026-10-12,
so the earlier "MCP support is not stated" no longer holds. Benchmarks are
now published: `benchmarks/README.md` (2026-09-08) reports LoCoMo 94.42 and
LongMemEval-S 94.00, end to end with an LLM in the retrieval loop, and
records the exact models and versions per result file; the judge model is
unverified, and the website still says 83.00% on LongMemEval, which does
not match. Per the research-doc convention, EverOS has no standalone
deep-dive; this section is its record.

**LLM-wiki with skill distillation: `NevaMind-AI/memU`** (~14.4K stars,
Apache-2.0, Python, commit 2026-09-21). (Corrected 2026-10-13: an earlier
note here said releases lag the code. The product ships on PyPI as
`memu-cli`, 0.10.0 on 2026-08-05 and 0.11.0b3 on 2026-08-21; the GitHub
v1.5.1 release is the retired `memu-py` package.) Positioned as a **shared LLM wiki across sessions,
agents and devices** whose signature is **automatic skill distillation**
(converting agent session history into reusable **Markdown** workflows), plus
scheduled memorization and standing retrieval-instruction injection into host
instruction files. Storage is SQLite (default) or Postgres+pgvector with
brute-force-cosine or pgvector search and pluggable embedding providers
(OpenAI/Jina/Voyage/Doubao/OpenRouter); the core logic is advertised at ~500 LOC.
It is **file-first-adjacent** (Markdown output) but **LLM-leaning**: the host
agent does the judgment/synthesis (the MemoryService itself makes no LLM calls, so
the *product* still depends on a provider), there is **no MCP** (it ships as
sidecar binaries that patch host instruction files, with adapters for
Cursor/Claude Code/Codex/OpenClaw/Hermes/WorkBuddy), and a managed cloud exists at
memu.so. Adapters exist for Cola and pi, and a generic `memu-agent detect`
finds unknown agents' transcripts and instruction files. **No named
benchmark** (no LoCoMo/LongMemEval figure) is in the README, and there is
still no MCP server as of 2026-10-13.
Its skill-distillation-into-Markdown is the idea worth noting against our
experience pass. Per the research-doc convention, memU has no standalone
deep-dive; this section is its record.

**Governed fleet memory (different buyer): `caura-ai/caura`** (~0.5K stars,
Apache-2.0 core + managed platform at caura.ai, Python/FastAPI, commit
2026-09-22, release backend-v3.17.2). Caura is a new camp for this
report: **governed shared memory for multi-agent fleets**. On **PostgreSQL 16 +
pgvector** (Redis optional) it layers row-level multi-tenant isolation, three
visibility scopes (`scope_agent`/`scope_team`/`scope_org`), **four trust tiers**
governing cross-fleet read/write/delete, a full **audit trail** ("every write,
delete, and transition logged"), automatic PII flagging, an auto-extracted
knowledge graph, and contradiction detection with supersession, plus a native MCP
surface at `/mcp` (12 tools) for Claude Desktop/Code, Cursor, Windsurf. Its
benchmark claims are **self-reported** and, per its own README, single-agent
proxies for a fleet product it says those benchmarks "can't measure": **LoCoMo
77.6% accuracy / 96.6% token savings** and **LongMemEval 92.2% accuracy / 79.2%
token savings** (with a "23 ms p50 · 27 ms p95" latency figure and a stated
production deployment of "300+ AI agents" at eToro; vendor-reported, not
independently verified). It sits opposite us on substrate (opaque Postgres, LLM
extraction, hosted-leaning) and orthogonal on goal: **org-scale fleet governance**
rather than one project's git-backed wiki. Its **trust-tier governance is a real
capability we do not have** (noted honestly in `competitive-parity.md`), but it is
a different buyer, not a coding-continuity migration target. Per the research-doc
convention, Caura has no standalone deep-dive; this section is its record.

**Proxy fan-in memory server (misleading name): `TencentCloud/TencentDB-Agent-
Memory`** (~27.2K stars in September, ~28.6K on 2026-10-13; MIT,
Node/TypeScript, release v2.0.1 then, stable v2.0.2 on 2026-10-09 and
v2.0.3-beta.1 on 2026-10-13). Despite the "TencentDB" name it **requires no Tencent database product**:
storage is **SQLite by default** (an experimental, off-by-default MongoDB
backend exists), so the name-search trap here would be to file it as a managed
cloud DB feature; it is not. It is a **team-level memory hub** that distills
conversations, documents and code into four reusable assets (**Chat Memory,
Skill, Wiki, CodeGraph**), and its distinctive mechanism is a **proxy fan-in**: a
three-service deployment (Memory Core + Memory Hub + Proxy) where agents point
their API **base URL** at the proxy ("One Proxy, unchanged protocol, zero-code
integration"), so capture is **proxy interception, not MCP or OS lifecycle
hooks**. Documented harness support spans Claude Code, Codex, DeepSeek Harness,
CodeBuddy, WorkBuddy, Hermes and OpenClaw. Its benchmark claim is **self-reported
PersonaMem** (48% without memory → 76% with it, "+59% relative"). Its **CodeGraph**
is a codebase-intelligence feature adjacent to, not overlapping, our session
memory. *Update 2026-10-13:* there are two product lines. `main` (v1) is an
OpenClaw/Hermes plugin (not the proxy), with self-reported agent-task
results rather than memory-retrieval ones: WideSearch 33→50%, SWE-bench
58.4→64.2% (50 tasks per session), AA-LCR 44→47.5%, plus token cuts. The v2
team hub on the default branch needs an LLM and has per-asset access control
(private / team / restricted / agent visibility, admin and member roles);
v2.0.2 imports external tasks, private repositories and wikis, and
v2.0.3-beta adds per-turn on-demand skill selection and session recording,
both off by default. Per the research-doc convention, TencentDB Agent Memory
has no standalone deep-dive; this section is its record.

**Adjacent, not head-to-head: `plastic-labs/honcho`** (~7K stars, AGPL-3.0
core + managed cloud, **$5.35M pre-seed** led by Variant/White Star/Betaworks).
Honcho is the archetype of a **new camp** this report had no bucket for:
**user-modeling / theory-of-mind memory**. Its unit of memory is a **"peer"**
(any entity that "persists but changes over time — users, agents, objects") and
its differentiator is treating memory as **"a reasoning problem, not a retrieval
problem"**: it models *what one peer knows and believes about another* over
time. The substrate is the opposite of ours on every axis we chose:
**PostgreSQL is the source of truth** (Postgres FTS/GIN + HNSW vectors, Redis
cache, pluggable LanceDB/Turbopuffer; **no git, no markdown, nothing to `grep`
or diff**), it is **LLM-required** (a **Deriver** does one structured-output LLM
call per message batch; a scheduled **Dreamer** runs surprisal-prioritized
"dream-time" consolidation into premise→conclusion reasoning trees, with no zero-LLM
path), and it runs as a **multi-service stack** (FastAPI API + async worker +
Postgres + Redis), cloud-leaning with a self-hostable core. Its flagship is the
**Dialectic endpoint** (`peer.chat()`): a natural-language *oracle*: you ask a
question about a peer and a synchronous tool-using agent (`search_memory`,
`grep_messages`, `get_reasoning_chain`, temporal search, five reasoning tiers)
synthesizes an answer rather than returning raw hits. Benchmarks are strong but
**vendor self-reported** (blog dated Dec 19 2025): LongMemEval-S 90.4% (Haiku
4.5) / 92.6% (Gemini 3 Pro), LoCoMo 89.9%, "median 5% of context tokens used."
It now ships coding-agent front-ends too (Claude Code/Codex/Cursor/OpenCode
plugins + an **MCP server** with exactly three tools: `honcho_search`,
`honcho_chat`, `honcho_remember`), so it **collides with us on the shelf**, but
the thing remembered is different: **Honcho remembers the *user*;
ai-memory remembers the *project*.** A coding team wanting cross-harness project
continuity would not switch to it (opaque DB, LLM-mandatory, user-centric,
multi-service), and an app builder wanting end-user personalization would not use
us. It is **adjacent, not a migration target either way**. Two ideas are worth
carrying forward as *optional LLM layers over* our zero-LLM core, never
requirements: the **dialectic/oracle query** (ask-a-question → synthesized
answer, which maps onto our retrieval as an opt-in LLM step) and the
**reasoning-tier ladder** (minimal→max reasoning per query as a clean
cost/quality knob). **Both now ship on 2.4** as opt-in, off-by-default LLM
layers (`memory_query answer=true` #782; `reasoning: {minimal..max}` #783),
and the Dreamer's *scheduling shape* (surprisal-first ordering, idle
trigger, cancel-on-activity) is the grounding for 2.4's opt-in "dream"
consolidation pass (#816, [`design-memory-aging.md`](design-memory-aging.md)
B3/B4), which stays off by default and never deletes a source. Deliberately
**reject** the rest as off-mission: LLM-mandatory
ingestion, opaque-Postgres-as-truth, the Postgres+Redis+worker operational
weight, and the heavy theory-of-mind engine itself. A coding agent needs project
facts, decisions, and conventions, not a psychological model of the developer
(the lightweight "standing user preferences" slice we already have via global
scope is enough). Positioning note: preempt the inevitable "but Honcho scores 90%
on LongMemEval" by clarifying those are *user-recall* benchmarks on chat
transcripts, not *coding-project* recall, a different task we are not competing
on. Per the research-doc convention, Honcho has no standalone deep-dive; this
section is its record.

### Refresh, 2026-10-13: what changed

Two readings from this pass change how the field should be described.

First, coding-agent memory is now the most-starred part of the field.
claude-mem (~100.3K stars) and MemPalace (~59.5K) capture coding sessions
through harness hooks, and Mem0, Supermemory, Hindsight, OpenViking and
Cognee all ship coding-agent plugins next to their app-memory products. The
earlier reading in this report, that the self-hosted coding-continuity
segment is small and less funded, no longer holds.

Second, the substrate choices that used to set us apart are now common.
Letta keeps each agent's memory as a git repository of markdown files,
Supermemory's memoryrepo is git-backed markdown, OpenViking's memories are
editable markdown, EverOS keeps canonical markdown, and Hindsight mirrors its
knowledge pages to markdown. Lifecycle-hook capture is standard across the
plugins named above. Neither git-backed markdown nor hook capture
differentiates on its own any more. What still does, as far as this pass
found: a zero-LLM default, git-versioned markdown as the source of truth
(not a mirror), claim-once handoffs and typed cross-project messaging,
per-project multi-user auth on one self-hosted binary, and capture under
whichever harness the user already runs.

**Published numbers, with their setups.** None of these is like-for-like
with another or with ours, and no ranking is claimed.

| Tool | Number | Kind | Setup caveats |
|---|---|---|---|
| ai-memory 3.0 | LongMemEval-S hit@5 0.951 zero-LLM FTS, 0.945 local embeddings (all 470) | Retrieval through the shipped capture pipeline | 2.6 published 0.666 / 0.815; component mode (raw sessions indexed whole) FTS 0.972 |
| MemPalace | 96.6% R@5 raw, 98.4% on 450 held-out questions | Retrieval only | Closest in kind to our hit@5; different harness |
| agentmemory | 95.2% `recall_any@5`; 2026-10-09 baseline 452/470 (96.2%) any-hit, 84.68% strict recall-all@5 | Retrieval, component (whole raw sessions) | Reranker disabled; the authors say the baseline supports no competitive claim |
| mcp-memory-service | 80.4% R@5 turn-level, 86.0% session-level | Retrieval | Not re-read on 2026-10-13 |
| Mem0 | LoCoMo 92.5, LongMemEval 94.4, BEAM 64.1 (1M) / 48.6 (10M) | End-to-end LLM-judged QA | Managed platform at a top-200 budget; the open harness defaults to gpt-4o as answerer and judge at top-k 200; the headline run's models are not stated |
| Zep | LoCoMo 94.7% (1,540 q), LongMemEval 90.2% (500 q) | End-to-end LLM-judged QA | gpt-5.4 reader and judge, cross-encoder reranking, ~5K context tokens; no harness repo; LongMemEval variant unverified; single-call "auto search" 86.5% LoCoMo |
| Hindsight | LongMemEval-S 94.6%, LoCoMo 92.0%, BEAM-10M 64.1% | End-to-end LLM-judged QA | AMB harness; Gemini answers and judges (model unverified); ~43.6K context tokens per question |
| EverOS | LoCoMo 94.42, LongMemEval-S 94.00 | End-to-end, LLM in the retrieval loop | Judge model unverified; its website says 83.00% LongMemEval |
| MemOS | LoCoMo 88.83, LongMemEval 89.20 | Own harness | Not further verified |
| Memori | LoCoMo 87% | Paper (arXiv:2603.19935) | Not further verified |
| Supermemory | 95% Recall@15 (README); 97% "Recall@20 with aggregation" (research page) | Mixed | gpt-4o judges answers and the 97% repeats in the LLM-judge table; "#1 on LoCoMo and ConvoMem" without numbers |
| Cognee | BEAM 0.79 (20 questions, one conversation), 0.67 at 10M | End-to-end LLM-judged QA | Tuned and scored on the same questions; gpt-5 for ingestion, answering and judging |
| memvid | +35% LoCoMo (relative) | Relative claim | Unverified |

**claude-mem (`thedotmack/claude-mem`, ~101.6K stars, Apache-2.0, v14.0.1
on 2026-10-17).** Five lifecycle hooks feed SQLite FTS5 plus Chroma, and an
LLM "observer" compresses every observation, so capture is LLM-driven. A
paid cloud tier (CMEM Pro) exists, and the README endorses a cryptocurrency
token. Recent work: non-blocking hooks that write to a spool, to-do tools, a
three-step progressive search, a bridge that intercepts reads of Claude
Code's and Codex's native memory folders and answers them from its own
store, and surfacing past observations for a file when the agent reads it.
v14 adds the local “My Work” request/workstream dashboard and the optional
paid TeamBrain shared-work layer. No benchmarks are published.

**MemPalace (v3.11.0, ~59.6K stars).** See §2 for the benchmark history.
It now ships hooks for Claude Code, Codex and Cursor, a team server and a
temporal graph, and its README publishes retrieval-only numbers (96.6% R@5
raw, 98.4% on 450 held-out questions). v3.11 adds dry-run-first audit/repair
and optional multimodal EmbeddingGemma 2 retrieval.

**Mem0 (~67.1K stars, core v2.2.1 on 2026-09-25).** Native coding-agent
plugins for Claude Code, Codex, Cursor, Kimi, Antigravity, OpenCode and pi
live in `integrations/`, generated from one shared runtime (#7203,
2026-09-08) with a conformance suite; the Claude Code plugin reached 0.3.0
on 2026-08-31. The hooks cover SessionStart, UserPromptSubmit, PostToolUse,
PostToolUseFailure, SubagentStart/Stop, Stop, PreCompact and SessionEnd.
Capture is local with no model call; a background worker batches every 5
exchanges or 5 minutes idle. Repository identity is a hash of the git
remote; search scopes are `repo`, `dir` (a subdirectory) and `mine`, with an
optional `--category` such as `problems_and_fixes`, and failed commands are
captured with their fixes. It keeps shared team project memory plus personal
memory per user. The plugins need a Mem0 cloud API key (`api.mem0.ai` by
default; a `MEM0_API_URL` override calls the cloud v3 endpoints, and
compatibility with a self-hosted server is unverified), and telemetry is sent
under the account email. User Profiles (v2.2.0, #7340) are cloud-only. Since
April 2026 retrieval is a single-pass add-only extraction plus semantic +
BM25 + entity fusion (the OSS package needs the `[nlp]` extra), so the old
"vector-only" description is wrong.

**Zep / Graphiti (~32.0K stars, v0.31.1 on 2026-10-15).** v0.30.0 sends
Neo4j queries to the configured database and reframes the README as a
temporal knowledge graph framework (#1801); v0.30.2 uses a driver per
request and isolates concurrent groups (#1699). v0.31 released #1964 from Zep's internal fork (combined extraction per
episode in bulk, date rules for facts, stricter contradiction rules), #1778
`LLMRuntime` per-prompt model routing, and an MCP server on MCP SDK 2.x
(#1921). FalkorDBLite is an embedded option; Kuzu support is deprecated
because upstream is unmaintained. Hosted Zep has a Context MCP server with
per-user OIDC sign-in, read-only connections set by an admin, and a write
kill switch; its plugin says it is "not positioned for Claude Code, Codex".

**Letta (`letta-ai/letta` ~25.2K stars; `letta-ai/letta-code` ~3.7K).** The
V1 server is retired: commit #3430 (2026-08-16) moved its source to an
`archive` branch at 0.16.8, and
the README points to Letta Code, a coding agent whose default backend is
Letta Cloud (v0.34.16 on 2026-10-16). Since February 2026 ("context
repositories") every agent's memory is a git repository of Markdown files
with YAML frontmatter and a `MEMORY.md` index; "dreaming" subagents write in
parallel through git worktrees, with no vector index by default. Its essay
"Why Memory Isn't a Plugin" (2026-04-03) argues memory belongs inside the
harness. The `claude-subconscious` plugin (~2.9K stars) is a demo that needs
a Letta key and keeps one memory across all projects by default. Letta Code
also has a `/doctor`-style memory audit that includes the token cost of the
session brief. The ADE references in older notes describe the retired line.

**Memori (~17.2K stars)** sits in the app-memory camp; its paper
(arXiv:2603.19935) reports LoCoMo 87%. **MemOS (~11.9K stars, v2.0.34)**
belongs to the memory-OS camp and reports LoCoMo 88.83 and LongMemEval
89.20 on its own harness. **memvid (~16.6K stars)** has had no commits since
2026-07-14 and publishes only relative claims (+35% LoCoMo), unverified.

**Adjacent tools, not memory systems:** codebase-memory-mcp (~46.5K, a code
graph), qmd (~30.4K, markdown search), context-mode (~27.9K, survives
compaction with a per-project SQLite FTS5 store fed by hooks) and beads
(~27.8K, an issue tracker with `bd remember`).

### Refresh, 2026-10-18: one-by-one delta

This pass used the 2026-10-13 refresh as its baseline. `Released` means the
capability is in a tagged release; `main` means it is visible in the default
branch but is not yet a stable release. The table records quiet projects
explicitly.

| Project | Change since 2026-10-13 | What it means for ai-memory |
|---|---|---|
| **claude-mem** | **Released v14.0.0/v14.0.1.** “My Work” turns captured requests into a local dashboard with workstream cards, evidence drill-down, live attention states, and an explicit human “Checked” state. TeamBrain adds selected shared work, collision awareness, decisions, and delegation as a paid service. | The local work dashboard is the clearest new product gap. Build a read-only operational view over ai-memory's existing managed-workstream ledger, with derived attention states and a minimal human review marker. Do not copy the paid cloud or create a second task tracker. |
| **Mem0** | No material memory change. The repository added event promotion; the latest core release remains v2.2.1. | No action. Its category and cloud-first tradeoffs are unchanged. |
| **MemPalace** | **Released v3.11.0.** Adds a dry-run-first audit/repair workflow, optional EmbeddingGemma 2 multimodal retrieval, embedder identity checks, large-palace performance work, and security fixes. v3.12 work is on `main`, not released. | ai-memory already records embedding identity, rejects stale vectors, and has `doctor`, lint, reindex, backup, and health tools. A unified audit UX is worth evaluating; native image/audio/video memory remains outside the text-only core. |
| **Hindsight** | No new stable release after v1.0.0. `main` adds decision-model triage before consolidation, decision-routed reflection/ranking, scheduled full mental-model refreshes, operation-specific request extensions, more providers, and richer source exports. | Measure per-operation model profiles and a cheap structured decision tier before adopting either. Any periodic full refresh must produce reviewed proposals, never silently rewrite source truth. |
| **OpenViking** | No new stable release after v0.5.0. `main` adds mixed image/text queries, scoped one-use downloads, Redis-backed idle commit scheduling, multi-replica account stores, privacy commands, and bot steering. | Most changes target a multimodal distributed context platform. The resource drill-down pattern validates ai-memory's progressive disclosure; the scaling and opaque-media stack does not fit the single-binary markdown core. |
| **Cognee** | **Released v1.6.5.** Adds code docstrings as graph documents, Memify APIs, batched trace processing, base64 embeddings, a Calendar connector, and session/dataset binding. | No new core work. Code graphs and connectors remain adjacent; batched ingestion reinforces the existing single-writer batching design. |
| **Zep/Graphiti** | **Released v0.31.0/v0.31.1.** Per-prompt `LLMRuntime` routing, combined extraction, temporal/dedup fixes, Decisions clients, and MCP SDK/group routing moved from unreleased work into a stable release. | This strengthens the case for a measured per-operation provider/model profile. It does not justify a graph database or LLM-mandatory path. |
| **Supermemory** | No new release after server-v0.0.9. `main` aligns the v5 API/docs and hardens MCP authorization, writable defaults, and deletion by ID. | No product gap. The authorization fixes validate ai-memory's fail-closed scope resolver and capability gates. |
| **agentmemory** | No new release after v0.9.30. `main` contains redaction, vector/compression recovery, bounded graph-index work, Redis mutation serialization, and Devin integration preparation. | No action. ai-memory already has Devin support, bounded capture, recovery, and a stricter sanitizer boundary. |
| **TencentDB Agent Memory** | v2.0.3-beta.1 remains the newest release; subsequent commits are fixes. | No material change to the proxy-fan-in comparison. |
| **Letta V1 server** | No product work. The repository is **not GitHub-archived**: its README redirects active development to Letta Code, while the retired V1 source lives on an `archive` branch. | Correct the old “repository archived” wording. The V1 server remains retired, but the repository itself is an active redirect. |
| **Letta Code** | **Released v0.34.14-v0.34.16.** Adds steering, resuming, and stopping remote Claude Code/Codex subagents, vault inheritance, listener heartbeats, and recovery fixes. | This is an agent runtime feature, not a memory-system gap. Managed workstreams remain harness-neutral. |
| **Memori** | No commits since 2026-10-03 and no new release. | No material change. |
| **memvid** | No commits since 2026-07-14 and no new release. | Remains quiet; its relative benchmark claim is still not independently useful. |
| **memU** | No feature release. Four Windows path/idempotency fixes landed. | No material change. |
| **EverOS** | **Released v1.4.2.** Fixes an FD leak, separates query embedding capacity from background writes, batches embeddings, keys indexes by the full scope tuple, self-repairs damaged indexes, and hardens path containment. | Adopted for the in-process local embedder after measurement: eight background workers raised query p50/p95 from 36/41 ms to 74/76 ms on the reference host; one document slot plus one reserved query slot brought them to 39/51 ms. Hosted providers and the reranker retain their existing policies. |
| **MemOS** | No commits or release after the prior cutoff. Its current headline remains DeepSeek Harness integration, multimodal memory, memory cubes, asynchronous ingestion, and feedback/correction. | No material change. The memory-OS and multimodal tradeoffs remain different from ai-memory's scope. |
| **Honcho** | **Released v3.3.0.** Adds an optional Chroma backend, scoped custom instructions, cursor pagination, prompt-budget handling, and performance fixes; stable object `uid` work followed on `main`. | No core work. Its theory-of-mind buyer remains different. Operation-scoped instructions are a useful reference if per-operation LLM profiles are designed. |
| **Engram** | No new tag after v3.4.0. `main` adds local aliases/routing preview, project provenance, compact search/history clients, capture guards, tombstones, and sync isolation. | No new gap. ai-memory already has stronger repository identity, routing diagnostics, tombstone/supersession semantics, and conflict-aware wiki sync. |
| **basic-memory** | No new stable release after v0.23.2. The unreleased v0.24 changelog describes optimistic checksummed writes, native OKF v0.2 export/check, cross-project and structured filters, document extraction with provenance, temporal qualifiers, a local reranker, locked notes, more harnesses, and cloud team collaboration. | ai-memory already ships native OKF, conditional writes, temporal queries, attribution, and lifecycle capture. Structured filters are a real gap. Document ingestion belongs in an optional importer; local reranking stays measure-first; live co-edit/cloud is a different product. |
| **mcp-memory-service** | **Released v12.1.0/v12.1.1.** Security fixes include stored-XSS handling and graph-write scoping; automatic supersession is now conservative. `main` adds Codex hooks, store-partitioned SSE, tombstone protection, and BM25 fixes. | No new feature work. The release validates conservative supersession, output encoding, and strict write scoping already required by ai-memory's invariants. |
| **LangMem** | No repository or package release since the prior cutoff. | No material change. |
| **Caura** | **Released backend-v3.34.5.** `main` also carries a beta collaboration extension: tenant-scoped peer discovery, requests, correlated replies, presence, leases, progress, and recovery for running Claude/Codex hosts. External effects remain at-least-once. | Treat the bus as adjacent fleet orchestration. ai-memory messaging is durable project-to-project continuity, not a live peer runtime. Do not add presence or wake orchestration to the memory core. |
| **LiquidLM** | Stable CLI is v0.1.6; v0.2.0-alpha.1 is prerelease. Public client work adds scoped tokens, MCP installation, safer upload reconciliation, search/upload commands, and local media fetching; the engine remains closed. | Correct the version. Its managed multimodal second-brain UX remains outside ai-memory's core. |
| **Claude Code native memory** | Releases 2.1.299-2.1.302 improve compaction and runtime stability but add no material project-memory capability. | No change to the comparison: excellent zero-setup single-harness memory, weak portability and team continuity. |

The adjacent codebase-memory, search, context-compaction, and issue-tracking
projects were not promoted into the competitor set. They remain useful
integration references, but they solve a different primary job.

### Popularity and maintenance signal (as of 2026-10-18)

Star counts, releases and last commits come from official GitHub or package
metadata checked on 2026-10-18; the "Sept" column preserves the 2026-09-18
figure where this report tracked one. Memori, memvid, LangMem, and MemOS had
no commit after the prior cutoff. Letta's original server remains retired in
favour of Letta Code, but its repository is a redirect rather than an archived
GitHub repository.

| Project | Stars (~) | Sept | Last commit | Latest release | Status |
|---|---|---|---|---|---|
| claude-mem | 101.6k | not tracked | 2026-10-18 | v14.0.1 (2026-10-17) | ACTIVE |
| Mem0 | 67.4k | 65.6k | 2026-10-17 | core v2.2.1 (2026-09-25); TS v3.3.1 | ACTIVE |
| MemPalace | 59.6k | not tracked | 2026-10-18 | v3.11.0 (2026-10-14) | ACTIVE |
| Hindsight | 49.1k | 23.9k | 2026-10-13 | v1.0.0 (2026-10-13) | ACTIVE |
| OpenViking | 40.3k | 38.0k | 2026-10-18 | v0.5.0 (2026-10-09) | ACTIVE |
| Cognee | 32.5k | 30.8k | 2026-10-16 | v1.6.5 (2026-10-16) | ACTIVE |
| Zep/Graphiti | 32.0k | 31.0k | 2026-10-17 | v0.31.1 (2026-10-15) | ACTIVE |
| Supermemory | 31.3k | 30.1k | 2026-10-18 | server-v0.0.9 (2026-10-12) | ACTIVE |
| agentmemory | 29.7k | 29.3k | 2026-10-18 | v0.9.30 (2026-10-06) | ACTIVE |
| TencentDB Agent Memory | 29.6k | 27.2k | 2026-10-15 | v2.0.3-beta.1 (2026-10-13) | ACTIVE |
| Letta (V1 server) | 25.2k | 24.8k | no V1 code after retirement | 0.16.8, frozen | RETIRED (source on `archive` branch) |
| Letta Code | 3.7k | not tracked | 2026-10-18 | v0.34.16 (2026-10-16) | ACTIVE |
| Memori | 17.2k | not tracked | 2026-10-03 | v3.3.6 (2026-05-28) | QUIET |
| memvid | 16.6k | not tracked | 2026-07-14 | v2.0.140 (2026-05-27) | QUIET |
| memU | 14.6k | 14.4k | 2026-10-17 | PyPI `memu-cli` 0.10.0 (stable) | ACTIVE |
| EverOS | 13.5k | 13.1k | 2026-10-16 | v1.4.2 (2026-10-14) | ACTIVE |
| MemOS | 11.9k | not tracked | no change after cutoff | v2.0.34 (2026-09-23) | ACTIVE |
| Honcho | 7.7k | 7.2k | 2026-10-17 | v3.3.0 (2026-10-14) | ACTIVE |
| Engram | 7.4k | 6.8k | 2026-10-18 | v3.4.0 (2026-10-12) | ACTIVE |
| basic-memory | 4.3k | 4.0k | 2026-10-18 | v0.23.2 (2026-08-25); v0.24 unreleased | ACTIVE |
| mcp-memory-service | 2.0k | 2.0k | 2026-10-18 | v12.1.1 (2026-10-16) | ACTIVE |
| LangMem | 1.7k | 1.7k | 2026-10-02 | PyPI 0.0.30 (2025-10-27) | QUIET |
| Caura | 0.5k | 0.5k | 2026-10-18 | backend-v3.34.5 (2026-10-18) | ACTIVE |
| LiquidLM | n/a (closed) | n/a | n/a (engine closed) | `@liquidlm/cli` 0.1.6; 0.2.0-alpha.1 prerelease | ACTIVE |

Adjacent, not memory systems (not ranked above): codebase-memory-mcp ~46.5k,
qmd ~30.4k, context-mode ~27.9k, beads ~27.8k.

Raw stars still measure reach more than fitness for coding continuity, but
the top of the chart is now coding-agent memory: claude-mem leads, MemPalace
is third, and most of the app-memory leaders (Mem0, Hindsight, OpenViking,
Supermemory, Cognee) now ship coding-agent plugins (see
[`competitive-parity.md`](competitive-parity.md)). Flags: Letta's original
server is retired and its work continues in Letta Code; memU ships through
PyPI rather than GitHub releases (an earlier "releases lag code" reading here
was a misread); memvid has had no commits since 2026-07-14;
LangMem and Honcho publish no GitHub *releases* (PyPI/tag-only versioning);
and LiquidLM is **closed-source**, so it has no repo signal at all; its only
public, open artifact is the MIT npm client `@liquidlm/cli` (stable v0.1.6).

## 4. Research developments worth knowing

- **"Rethinking How to Remember: Beyond Atomic Facts in Lifelong LLM
  Agent Memory" (TriMem, arXiv:2605.19952)** argues that atomic-fact stores
  lose the relational/causal context agents need, proposes a
  document/page/narrative hierarchy, and shows it outperforming
  fact-baselines on retrieval and multi-step reasoning. This is direct
  academic support for pages-over-facts, the bet that separates us from
  the Mem0 camp. Its "narrative" layer (temporal storylines connecting
  events) is the one level we only partially have (sessions and
  handoffs are narrative-ish; nothing stitches them across weeks).
- **"From Storage to Experience" survey (arXiv:2605.06716)** frames
  the field's evolution as Storage (preserve trajectories) → Reflection
  (refine them) → Experience (abstract *across* trajectories, proactive).
  We are solidly through Storage and Reflection (capture, consolidation,
  curator, lint, feedback-driven salience). The opt-in experience pass now
  covers cross-trajectory abstraction, although its quality gain remains an
  evaluation question rather than a settled default.
- **LongMemEval-V2 (arXiv:2605.12493)**: the benchmark generation has
  moved from "recall the fact" to "behave like an experienced colleague"
  over long horizons, and finds every evaluated system far from it. Two
  implications: our product framing matches where evaluation is going, and
  the in-repo evaluation harness should keep expanding beyond retrieval. We
  now publish reproducible retrieval-through-capture numbers and ship an
  answerer/judge path with context, latency, and exact-model receipts. The
  remaining evidence gap is a published run over the full 470-question set.
- **Bi-temporal validity (Zep paper, arXiv:2501.13956, now mainstream
  via Graphiti)** is the one *specific* mechanism from the graph camp
  worth studying: every fact/edge carries event-time and
  ingestion-time, so "what did we believe on June 1" and "what was true
  on June 1" are both answerable, and corrections are supersessions
  rather than overwrites. ai-memory now ships ingestion-time temporal
  validity and `as_of` reads on SQLite; true world-time extraction remains a
  deliberate LLM-dependent boundary.
- Also active, lower relevance for us: MIRIX (multi-agent memory
  specialization), MemOS (scheduling memory like an OS),
  A-TMA (state-aware memory failure taxonomy).
- **Sleep-time compute / "dreaming"** (Letta and UC Berkeley,
  arXiv:2504.13171; Anthropic's "The components of agent memory",
  2026-11-06). Anthropic's article makes offline consolidation the core
  mechanism over a transcript → notes → wiki → injected-context hierarchy.
  Session-end consolidation jobs and scheduled maintenance cover only part of
  it. The article's clearest result corrects a wrong note against the
  transcript. Our passes correct forward, when a new session's observations
  contradict a page; none goes back to the observations behind an existing
  page. The Managed Agents memory stores and dreams it uses serve agents built
  on that API and do not plug into a coding harness, so they have no camp
  entry. Analysis and open questions:
  [`research-anthropic-agent-memory.md`](research-anthropic-agent-memory.md).

## 5. What this means for ai-memory - analysis and recommendations

> Companion audit: [`competitive-parity.md`](competitive-parity.md) turns this
> "what to borrow" lens around and asks the harder one: in the goals where we
> *overlap* a competitor, are we actually better or did we copy without
> improving, and is ai-memory worth migrating *to*? It is the self-critical
> record behind this section's recommendations.

The May research led us to build versioned supersession, retention
formulas, hybrid RRF retrieval, opt-in LLM consolidation, handoffs as a
protocol, single self-contained binary, typed scope isolation. Every one
of those choices still fits the field in October. The
substrate competitors struggled (mempalace's metaphor, agentmemory's
KV), the file-first camp got standardized, and pages-over-facts got a
paper. Nothing in this pass argues for re-architecture. The
original recommendations below are retained as an implementation audit;
their headings state what has shipped.

**R1: OKF conformance - shipped.** The wiki is a native OKF v0.2 bundle,
with generated index/log projections and `ai-memory okf check`. No export-only
compatibility layer is needed.

**R2: Reproducible eval harness - shipped; full-set QA run still open.** The
in-repo harness publishes retrieval-through-capture and component results with
run commands and receipts, and includes an opt-in answerer/judge QA mode with
per-question outcomes and exact provider/model provenance. It also keeps the
belief-strength and LLM dream features off by default because neither has
shown a useful retrieval delta. The remaining work is operational: run and
publish the full 470-question QA set rather than only the existing sample.

**R3: Typed relation edges - shipped.** `causes`, `fixes`, and `contradicts`
are stored and explained, related-page reads can traverse them, and
contradiction lint uses them. Edge kind does not yet affect rank by default.

**R4: Temporal validity - shipped at the evidence-backed boundary.**
Ingestion-time validity, supersession, and `as_of` reads cover historical
belief without requiring an LLM to invent world-time dates. Full bi-temporal
fact extraction remains deliberately out of scope for the zero-LLM path.

**R5: Cross-session abstraction pass - shipped, opt-in.** The experience pass
requires evidence from multiple sessions and feeds reviewed changes through
the auto-improvement path. It remains opt-in because the current eval does not
show enough quality benefit to make it a default.

**R6: Local embeddings - shipped.** A checksum-pinned pure-Rust candle path
provides local embeddings without ONNX or a provider key, with identity stored
beside every vector and stale models excluded.

**R7: Progressive-disclosure brief tiers - shipped.** The session brief now
starts with bounded extracts for all standing pages, then spends the remaining
budget on pinned and slot content. It runs without an LLM or migration. The
remaining retrieval gap was a path-prefix filter, now shipped with the other
structured MCP filters described below.

**R8: Queryable metadata/tag filters shipped; full eval run still open.** The
retrieval harness and accuracy/latency/context reporting exist. `memory_query`
now exposes kind, tier, literal path-prefix, and exact-array tag predicates
before every page-ranking stream, including global, historical, profile, and
pinned results. Author remains deferred until null root/anonymous attribution
has explicit semantics. Document extraction now stays in the importer
companion: explicit local text/Markdown, text-layer PDF, DOCX, PPTX, XLSX, and
VTT/SRT become attributed sanitized markdown through bounded
conditional writes. The server remains text-first; connectors, OCR, images,
and raw media remain out of scope.

### Current plan after the 2026-10-18 pass

This is the implementation ledger for the short list selected by that pass.
Items not marked complete still need their own design and approval.

1. **P1 - Workstream operations view. Completed.** `/web/workstreams` is a
   read-only root/operator view over bounded managed-run aggregates. It derives
   only active, interrupted, completed, and empty from trusted lifecycle
   columns and exposes no checkout path, native session id, or transcript
   content. It does not infer task states from untrusted prose or create a
   second task system.
2. **P1 - Protect interactive recall from background embedding. Completed.**
   The opt-in real-model probe found material local CPU contention, so local
   inference now admits one document embedding and reserves a second slot for
   queries. The same probe verifies the improvement; hosted providers remain
   unchanged, and provider failures still degrade to lexical/entity/graph
   retrieval.
3. **P2 - Structured query filters. Completed.** `memory_query` now exposes
   the existing pre-rank kind/tier filters plus bounded literal path-prefix and
   exact-array tag filters. The same predicate covers normal hybrid streams,
   global FTS, `as_of`, profile results, and pinned prepends; filtered misses
   omit metadata-less raw observations. `as_of` remains the valid-time control,
   and author remains deferred until null root/anonymous attribution has
   explicit semantics.
4. **P2 - Per-operation LLM profiles, telemetry completed; routing deferred.**
   Root-protected status now reports bounded process-lifetime call count,
   outcome, latency, coarse token estimates, reported usage, and health for a
   closed set of workloads. It captures no prompts or outputs, and labels do
   not affect the provider wire request. Real measurements still need to show
   a quality/cost benefit before public per-workload provider/model settings or
   a cheap decision tier are added.
5. **P3 - Optional document importer. Completed.** The companion extracts one
   explicit local text/Markdown, text-layer PDF, DOCX, PPTX, XLSX, VTT, or SRT
   file into attributed, sanitized markdown. Source, archive, extracted
   text, sheet, and cell limits fail closed; deterministic digest paths and
   conditional MCP writes make reruns idempotent without overwriting a foreign
   page. Opaque files, OCR, raw media, connectors, and native multimodal
   embeddings stay out of the server core.
6. **P3 - End-to-end QA evaluation. Deferred to an authorized paid run.** Use
   the shipped answerer/judge path for one deliberate full 470-question run and
   publish its receipt with accuracy, p50/p95 latency, context tokens, and exact
   models. This is not a release gate and needs explicit provider/budget
   approval because it makes roughly 940 paid model calls. Do not build a
   duplicate path or claim a cross-tool ranking without a shared harness.

**Deliberately not recommended:** joining the memory-OS camp (agent
self-editing its memory is token-expensive, and Letta's own successor now
keeps memory as git-backed markdown files);
adopting a graph database (bi-temporal-lite on SQLite covers the useful
part); spatial/metaphor architectures (mempalace); presenting unlike
benchmarks as a ranking; chasing agentmemory's tool-count
(the May conclusion stands: a sharp small surface is the advantage).

## 6. Sources

- OKF: Google Cloud blog "How the Open Knowledge Format can improve data
  sharing"; document360.com and mindstudio.ai explainers (spec v0.1,
  2026-06-12).
- agentmemory: github.com/rohitg00/agentmemory (README, CHANGELOG,
  docs/benchmarks/2026-05-20-coding-agent-life-v1.md). Re-checked
  2026-10-10 at commit `7ee1628`: README, CHANGELOG (0.9.27–0.9.30),
  `benchmark/longmemeval-bench.ts` and `benchmark/LONGMEMEVAL.md` (whole
  sessions, `rerankEnabled=false`), `docs/benchmarks/2026-10-09-evaluation-baseline.md`,
  `eval/README.md` and `eval/runner/adapters/`, `src/state/hybrid-search.ts`
  and `src/state/reranker.ts` (reranker opt-in via `RERANK_ENABLED`),
  `src/mcp/tools-registry.ts` (54 tools), the git history of the
  coding-agent-life-v1 report (correction in `e5c9dc6`), and the GitHub
  repository and release metadata.
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
  supermemory-vs-mem0; vendor self-reported benchmarks); DeepWiki
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
- LiquidLM: liquidlm.com (+ /about, /docs, /docs/assistants, /docs/mcp: Vaults,
  MCP tool surface, per-harness support with test dates), github.com/liquidlm
  (org: an archived empty `cli` repo + a homebrew tap, no engine source), and the
  npm registry metadata for `@liquidlm/cli` (MIT; stable v0.1.6 as of
  2026-10-18, with v0.2.0-alpha.1 as a prerelease).
  Solo maker (Carlos Souza), closed-source cloud engine, no benchmarks; the
  backend internals, model, and any numbers are undisclosed/vendor framing.
  Analyzed inline in §3 per the no-standalone-doc convention.
- Engram (issue #810): github.com/Gentleman-Programming/engram (README:
  SQLite+FTS5, Go single binary, MCP stdio, operating contract /
  `mem_session_summary`, git-sync, Engram Cloud; MIT) and the GitHub metadata
  endpoint (~6.8K stars, commit 2026-09-22, release v2.0.0). No benchmark
  published. Analyzed inline in §3 per the no-standalone-doc convention.
- EverOS (issue #810): github.com/EverMind-AI/EverOS (README: Markdown source
  of truth + SQLite/LanceDB indexes, user/agent tracks, Knowledge Wiki, offline
  consolidation, minimal-tier LLM-required, OpenRouter default; Apache-2.0) and
  the metadata endpoint (~13.1K stars, commit 2026-09-09, release v1.3.1). No
  benchmark was published as of 2026-09-22 (see the 2026-10-13 sources
  below). Analyzed inline in §3 per the no-standalone-doc convention.
- memU (issue #810): github.com/NevaMind-AI/memU (README: shared LLM wiki,
  Markdown skill distillation, SQLite/Postgres+pgvector, host-instruction-file
  sidecars, no MCP; LICENSE.txt = Apache-2.0) and the metadata endpoint (~14.4K
  stars, commit 2026-09-21, GitHub release v1.5.1 2026-03-23, which is the
  retired `memu-py`; the product ships on PyPI as `memu-cli`); managed
  cloud at memu.so. No named benchmark. Analyzed inline in §3 per the
  no-standalone-doc convention.
- Caura (issue #810): github.com/caura-ai/caura (README: governed multi-agent
  fleet memory on Postgres+pgvector, visibility scopes, four trust tiers, audit,
  PII flagging, KG, MCP `/mcp`; Apache-2.0) + managed caura.ai, and the metadata
  endpoint (~0.5K stars, commit 2026-09-22, release backend-v3.17.2). Benchmarks
  (LoCoMo 77.6% / LongMemEval 92.2%, "23 ms p50," eToro "300+ agents") are
  README-stated and vendor self-reported, not independently verified. Analyzed
  inline in §3 per the no-standalone-doc convention.
- TencentDB Agent Memory (issue #810): github.com/TencentCloud/TencentDB-Agent-
  Memory (README: Chat Memory/Skill/Wiki/CodeGraph assets, proxy fan-in base-URL
  interception, Node/TS, SQLite default + experimental MongoDB, harness list;
  LICENSE = MIT despite the GitHub NOASSERTION detection) and the metadata
  endpoint (~27.2K stars, commit 2026-09-22, release v2.0.1). PersonaMem
  48%→76% is vendor self-reported. The name does **not** imply a Tencent DB
  dependency (SQLite by default). Analyzed inline in §3 per the no-standalone-doc
  convention.
- Zep/Graphiti: arXiv:2501.13956; getzep.com temporal-KG explainer;
  Neo4j "Graphiti: Knowledge graph memory for an agentic world".
- Letta: "Is a Filesystem All You Need?" (letta.com blog, Aug 2025).
- Claude Code auto memory: official docs code.claude.com/docs/en/memory
  (read 2026-11-14: layout, 200-line / 25 KB index load, machine-local,
  per-repository keying); blog.memoryplugin.com/claude-code-memory and
  the thepromptshelf.dev guide (third-party, the only source for the v2.1.59
  default-on version).
- Anthropic, "The components of agent memory" (2026-11-06),
  anthropic.com/research/components-of-agent-memory; analyzed in
  [`research-anthropic-agent-memory.md`](research-anthropic-agent-memory.md).
- Papers: arXiv:2605.19952 (TriMem), arXiv:2605.06716 (Storage→
  Experience survey), arXiv:2605.12493 (LongMemEval-V2), MIRIX
  (Semantic Scholar), MemOS listings.
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
- 2026-10-13 refresh (stars, releases and last commits from the GitHub API
  that day):
  - claude-mem: github.com/thedotmack/claude-mem (README, Apache-2.0,
    v13.37.0).
  - MemPalace: github.com/MemPalace/mempalace (README benchmark split,
    v3.10.0).
  - Mem0: github.com/mem0ai/mem0/tree/main/integrations/claude-code-plugin
    and the rest of `integrations/` (#7203 shared runtime, #7340 User
    Profiles); mem0.ai/research; github.com/mem0ai/memory-benchmarks
    (harness defaults).
  - OpenViking: github.com/volcengine/OpenViking release notes v0.4.22,
    v0.4.23 and v0.5.0 (#5625 Gateway, #5459, #5450, #5454, #5456, #5214,
    #5359); blog.openviking.ai posts of 2026-05-29 (benchmarks, v0.3.22) and
    2026-10-03 (local keyless embeddings).
  - Zep/Graphiti: github.com/getzep/graphiti release notes v0.30.0 and
    v0.30.2 (#1801, #1699) and unreleased `main` (#1964, #1778, #1921);
    getzep.com/research (modified 2026-09-29);
    help.getzep.com/memory-mcp-server; github.com/getzep/zep-context-plugin.
  - Cognee: github.com/topoteretes/cognee release notes v1.6.0 to v1.6.3;
    github.com/topoteretes/cognee-integrations/tree/main/integrations/claude-code
    (v1.6.6 changelog, plugin directory listing);
    `cognee/eval_framework/beam/REPORT.md`.
  - Supermemory: github.com/supermemoryai/supermemory
    `apps/docs/self-hosting/overview.mdx` (server binary not open source,
    lite licence, telemetry), the Claude Code and Codex plugin hooks, the
    README and research page benchmark claims, smfs.ai.
  - agentmemory: `docs/benchmarks/2026-10-09-evaluation-baseline.md` and
    the commit history to 2026-10-13.
  - TencentDB Agent Memory: github.com/TencentCloud/TencentDB-Agent-Memory
    (`main` v1 README with agent-task results; v2 default branch; release
    notes v2.0.2 and v2.0.3-beta.1).
  - Letta: github.com/letta-ai/letta (commit #3430, 2026-08-16, README
    pointer); github.com/letta-ai/letta-code (v0.34.13); Letta's material on
    context repositories (February 2026) and its post "Why Memory Isn't a
    Plugin" (2026-04-03); the `claude-subconscious` plugin repository.
  - Hindsight: github.com/vectorize-io/hindsight (v1.0.0 release notes,
    `hindsight-coding-agents`, `hindsight fs mount`);
    github.com/vectorize-io/agent-memory-benchmark (AMB harness defaults).
  - memU: PyPI `memu-cli` release history (0.10.0, 0.11.0b3);
    github.com/NevaMind-AI/memU README (adapters, `memu-agent detect`).
  - EverOS: github.com/EverMind-AI/EverOS (v1.4.1 release notes,
    `benchmarks/README.md` dated 2026-09-08); github.com/EverMind-AI/EverOS-MCP.
  - Memori: arXiv:2603.19935 (LoCoMo 87%). MemOS: README (v2.0.34;
    LoCoMo 88.83, LongMemEval 89.20). memvid: GitHub metadata (last commit
    2026-07-14).
  - Adjacent tools: GitHub metadata for codebase-memory-mcp, qmd,
    context-mode and beads.
- 2026-10-18 delta pass (official sources only):
  - GitHub repository and release metadata for every row in the popularity
    table, plus package registries for `memu-cli` and `@liquidlm/cli`.
  - claude-mem [v14.0.0 release notes](https://github.com/thedotmack/claude-mem/releases/tag/v14.0.0)
    (`My Work`, TeamBrain) and v14.0.1.
  - MemPalace [v3.11.0 release notes](https://github.com/MemPalace/mempalace/releases/tag/v3.11.0)
    and the subsequent default-branch changelog.
  - Hindsight default-branch changelog and
    [mental-model documentation](https://github.com/vectorize-io/hindsight/blob/main/skills/hindsight-docs/references/developer/api/mental-models.md).
  - OpenViking default-branch release notes and docs for multimodal query,
    scoped downloads, Redis scheduling, accounts, privacy, and VikingBot.
  - Cognee [v1.6.5 release notes](https://github.com/topoteretes/cognee/releases/tag/v1.6.5).
  - Graphiti [v0.31.0](https://github.com/getzep/graphiti/releases/tag/v0.31.0)
    and v0.31.1 release notes.
  - Letta V1 README/branch list and Letta Code v0.34.14-v0.34.16 release
    notes. The branch list is the basis for correcting “GitHub-archived” to
    “retired source on the `archive` branch.”
  - EverOS [v1.4.2 release notes](https://github.com/EverMind-AI/EverOS/releases/tag/v1.4.2)
    and changelog.
  - Honcho [changelog](https://github.com/plastic-labs/honcho/blob/main/CHANGELOG.md)
    through v3.3.0.
  - basic-memory default-branch changelog for the unreleased v0.24 section;
    stable release metadata remains v0.23.2.
  - mcp-memory-service [v12.1.1 release notes](https://github.com/doobidoo/mcp-memory-service/releases/tag/v12.1.1)
    and subsequent default-branch commits.
  - Caura's `docs/agent-collaboration-extension.md`, collaboration client
    README, and backend-v3.34.5 release metadata. The docs explicitly call the
    collaboration extension a beta preview and external effects at-least-once.
