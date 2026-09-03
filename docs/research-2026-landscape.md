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
| Fact extractors | **Mem0**, LangMem, Supermemory | LLM extracts atomic facts per turn; lightweight personalization |
| **File-first wiki memory** | **us**, basic-memory, OKF, Letta's filesystem result, mempalace (nominally) | Markdown source of truth, derived indexes, human-editable |
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
on LongMemEval turn-level, 86.0% session-level.

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
run command - never a marketing claim without the harness.

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

**R6 - Local embeddings via ONNX (medium; already reserved).**
`models/` has been reserved for exactly this since M9.5 planning.
Competitors ship all-MiniLM locally by default; it removes the provider
dependency from vector search and makes hybrid retrieval a zero-config
default rather than an opt-in. The `ort` crate is the known path.

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
