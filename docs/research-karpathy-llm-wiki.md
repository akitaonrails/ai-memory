# Karpathy's "LLM Wiki" - Research Report

> The pattern this project is trying to implement faithfully. Primary source
> below; related/competing ideas listed for honest contrast.

## 1. What Karpathy Actually Said

The canonical primary source is Karpathy's April 2026 gist [`llm-wiki.md`](https://gist.github.com/karpathy/442a6bf555914893e9891c11519de94f), which he calls an **"idea file"** - explicitly *not* a library or app, but a pattern designed to be copy-pasted into an agent (Claude Code, Codex, OpenCode) so the agent can instantiate it for the user's domain.

The original framing came from an X thread on April 2, 2026, paraphrased as: *"using LLMs to build personal knowledge bases for various topics of research interest"*. He followed up two days later with the gist. He then [boosted "Farzapedia"](https://x.com/karpathy/status/2040572272944324650) as a good example of the pattern in the wild.

### The core argument (verbatim from the gist)

> "Most people's experience with LLMs and documents looks like RAG: you upload a collection of files, the LLM retrieves relevant chunks at query time, and generates an answer. This works, but the LLM is rediscovering knowledge from scratch on every question. There's no accumulation."

> "Instead of just retrieving from raw documents at query time, the LLM **incrementally builds and maintains a persistent wiki** — a structured, interlinked collection of markdown files that sits between you and the raw sources. When you add a new source, the LLM doesn't just index it for later retrieval. It reads it, extracts the key information, and integrates it into the existing wiki — updating entity pages, revising topic summaries, noting where new data contradicts old claims, strengthening or challenging the evolving synthesis. The knowledge is compiled once and then *kept current*, not re-derived on every query."

> "The wiki is a persistent, compounding artifact. The cross-references are already there. The contradictions have already been flagged."

> "The tedious part of maintaining a knowledge base is not the reading or the thinking — it's the bookkeeping... LLMs don't get bored, don't forget to update a cross-reference, and can touch 15 files in one pass."

He explicitly links the idea to Vannevar Bush's 1945 Memex - "a personal, curated knowledge store with associative trails between documents" - arguing the part Bush couldn't solve was *who does the maintenance*; LLMs solve that.

## 2. The Core Principles

From the gist itself (Karpathy's, not paraphrased):

1. **Compilation, not retrieval.** Knowledge is compiled at ingest time, not re-synthesized at query time. The wiki is the artifact; raw sources are the source of truth.
2. **Three-layer architecture.**
  - **Raw sources** - immutable; LLM reads only.
  - **Wiki** - markdown files; LLM owns and maintains entirely.
  - **Schema** (CLAUDE.md / AGENTS.md) - conventions that turn "a generic chatbot into a disciplined wiki maintainer."
3. **Three operations: Ingest / Query / Lint.**
  - *Ingest*: one source typically touches **10–15 wiki pages**.
  - *Query*: "good answers can be filed back into the wiki as new pages... explorations compound in the knowledge base just like ingested sources do."
  - *Lint*: periodic health check for contradictions, stale claims, orphan pages, missing cross-references, data gaps.
4. **Cross-linking is the synthesis.** The wiki is interlinked like Wikipedia or a fan wiki (he cites [Tolkien Gateway](https://tolkiengateway.net/wiki/Main_Page)); the graph *is* the consolidated knowledge.
5. **Two navigation files: `index.md` (content catalog) and `log.md` (chronological append-only ledger).** The log uses a fixed prefix so unix tools (`grep "^## \["`) can parse it.
6. **Division of labor.** Human curates sources and asks good questions; the LLM does "the summarizing, cross-referencing, filing, and bookkeeping." Or in his metaphor: *"Obsidian is the IDE; the LLM is the programmer; the wiki is the codebase."*

### What Karpathy did *not* explicitly say (honest caveats)

The community frequently attributes a few additional ideas to him that are **paraphrase / extension**, not in his gist:

- **Episodic vs. semantic memory tiers** - neuroscience framing. Not in the gist. This comes from extensions like [LLM Wiki v2](https://gist.github.com/rohitg00/2067ab416f7bbe447c1977edaaa681e2) and the broader memory-research literature.
- **"Sleep-like" consolidation passes** - also extension framing, not Karpathy's. His closest analog is the *Lint* operation (periodic health-check), which is rule-based rather than dream-like.
- **Confidence scoring, Ebbinghaus decay, supersession semantics** - these are LLM Wiki v2's additions, not the original.
- **The numbered "1. Explicit. 2. ..." Farzapedia points** - community summaries say Karpathy listed several advantages of *explicit memory artifacts* over the "AI that allegedly gets better the more you use it" status quo. Flagged as **community paraphrase**.

## 3. Implementation Hints from the Gist

- **Trigger**: human-in-the-loop on ingest ("I prefer to ingest sources one at a time and stay involved"), though batch ingestion is allowed.
- **Format**: plain markdown in a git repo. Optional YAML frontmatter for Dataview queries.
- **Retrieval at small scale**: `index.md` is "surprisingly good... at ~100 sources, ~hundreds of pages" - no embeddings needed.
- **Retrieval at larger scale**: shell out to a local hybrid search tool (he names [`qmd`](https://github.com/tobi/qmd), BM25 + vector + LLM re-rank, available as CLI and MCP).
- **Tooling**: Obsidian as the viewer (graph view to spot orphan/hub pages), Web Clipper to capture sources, version-controlled in git.

## 4. Related / Competing Ideas

- **MemGPT / Letta** ([letta.com](https://www.letta.com/blog/benchmarking-ai-agent-memory)): treats the context window as virtual memory; the *agent itself* decides what to page in/out across core, recall, and archival tiers. Stronger on long-horizon episodic coherence; higher lock-in (owns the agent loop).
- **Mem0** ([tokenmix.ai comparison](https://tokenmix.ai/blog/ai-agent-memory-mem0-vs-letta-vs-memgpt-2026)): lightweight memory layer with `extract / store / retrieve`. Extracts memories *passively* from conversations rather than letting the agent self-edit. Low lock-in.
- **A-MEM** ([arXiv 2502.12110](https://arxiv.org/abs/2502.12110), NeurIPS 2025): explicitly Zettelkasten-inspired. Each memory is an atomic note with structured attributes, keywords, tags; new memories trigger *evolution* of existing notes' representations. This is the closest published research analog to Karpathy's wiki - atomic notes + automatic linking + revision propagation.
- **ReadAgent** (Google DeepMind, 2024): "gist memory" - compresses long contexts into a tree of summaries with pointers back to detail. Different angle (long-document reading), but shares the "compile, don't re-retrieve" instinct.
- **LLM Wiki v2** (Rohit Ghumare): explicit extension with confidence scores, supersession, Ebbinghaus decay, four consolidation tiers (working → episodic → semantic → procedural), event-driven hooks, audit trails. This is basically the agentmemory model.
- **Rowboat / knowledge-graph extension** ([dailydoseofds.com](https://blog.dailydoseofds.com/p/the-next-step-after-karpathys-wiki)): argues the wiki of summaries breaks down for evolving work contexts (deadlines, commitments) and proposes a *typed-entity knowledge graph* (decisions, people, projects as nodes).

## 5. Design Implications for a Rust MCP Server for Coding Agents

Translating Karpathy faithfully - a "Karpathy-style" backend looks very different from naive vector RAG:

**What it is, concretely:**

- **Storage = markdown files in a git repo**, not opaque vector blobs. The wiki must be human-inspectable and grep-able. Embeddings can index it but never replace it.
- **Three directories enforced by the MCP server**: `raw/` (append-only, immutable), `wiki/` (LLM-writable, structured), and a schema doc (`AGENTS.md`-style) the server injects into every session.
- **MCP tools mirror the three operations**: `memory_ingest`, `memory_query`, `memory_lint` - plus low-level primitives (`wiki_read`, `wiki_write`, `wiki_link`, `wiki_supersede`). Not `vector_search` as the headline tool.
- **Ingest must be a *write fan-out*, an insert.** A new observation should *touch ~10–15 existing pages* - updating an entity page, a concept page, a decisions log, a gotchas page. This is the single biggest deviation from vector RAG, which only ever appends.
- **`index.md` and `log.md` as first-class files.** The log is the audit trail and the consolidation trigger source. Use the prefix convention (`## [YYYY-MM-DD] action | title`) so it's grep-able.
- **Retrieval is hierarchical, nearest-neighbor.** Read `index.md` → narrow to candidate pages → read them → optionally fall back to hybrid search (BM25 + vector, RRF-fused) for novel queries. The index *is* the synthesis; the embeddings are a backstop.
- **Consolidation is an explicit, scheduled MCP operation**, not a side-effect. `memory_consolidate` is invoked on true session-end hooks where the client exposes them, on the manual `ai-memory finalize-session` flow for clients such as Codex and Antigravity CLI, on compaction events, or on a timer. It is LLM-driven (needs a provider key); if absent, it runs no-op as agentmemory does.
- **Cross-agent shared state.** Because the wiki is plain text, Claude Code, Codex, and OpenCode all read/write the *same* artifact. The MCP server is the gatekeeper; the markdown is the contract. No vendor lock.
- **Coding-specific page types**: library gotchas, architectural decisions (ADR-style), failed approaches, repo conventions, environment quirks. Karpathy's example domains were personal/research; for coding agents the high-value pages are *failure modes* and *decisions*, because those are exactly what gets dropped on context compaction.

**What it deliberately is *not*:**

- Not a vector database with a chat wrapper. Vectors are a retrieval *aid* over markdown, not the source of truth.
- Not a chronological transcript. The log exists, but it's metadata. The semantic content lives in synthesized pages.
- Not opaque. Every memory the agent has must be openable in Obsidian, diff-able in git, and explainable in prose.

**Honest tension** worth resolving in design: Karpathy's gist is optimized for *human-curated research wikis* ingested one source at a time with the user watching. A coding agent ingests *continuously and unsupervised* from tool calls. This project inherits Karpathy's structure but needs the lifecycle layer (decay, supersession, confidence) that LLM Wiki v2 proposes - otherwise the wiki will fill with stale, low-signal observations from autonomous runs.

## Sources

- [Karpathy - `llm-wiki.md` gist (primary source)](https://gist.github.com/karpathy/442a6bf555914893e9891c11519de94f)
- [Karpathy - Farzapedia tweet](https://x.com/karpathy/status/2040572272944324650)
- [Yuchen Jin's summary tweet quoting Karpathy](https://x.com/Yuchenj_UW/status/2040482771576197377)
- [AkitaOnRails - AI Agent Memory: Karpathy LLM Wiki and agentmemory in Practice](https://akitaonrails.com/en/2026/05/18/ai-agent-memory-karpathy-llm-wiki-agentmemory/)
- [Rohit Ghumare - LLM Wiki v2 (gist)](https://gist.github.com/rohitg00/2067ab416f7bbe447c1977edaaa681e2)
- [A-MEM: Agentic Memory for LLM Agents (NeurIPS 2025)](https://arxiv.org/abs/2502.12110)
- [Mem0 vs Letta vs MemGPT comparison (TokenMix, 2026)](https://tokenmix.ai/blog/ai-agent-memory-mem0-vs-letta-vs-memgpt-2026)
- [Benchmarking AI Agent Memory: Is a Filesystem All You Need? (Letta)](https://www.letta.com/blog/benchmarking-ai-agent-memory)
- [The Next Step After Karpathy's Wiki Idea - Avi Chawla](https://blog.dailydoseofds.com/p/the-next-step-after-karpathys-wiki)
- [Gamgee: Why the Future of AI Memory Isn't RAG](https://gamgee.ai/blogs/karpathy-llm-wiki-memory-pattern/)
- [Beyond RAG: How Karpathy's LLM Wiki Pattern Builds Knowledge That Compounds (Plaban Nayak, Level Up Coding)](https://levelup.gitconnected.com/beyond-rag-how-andrej-karpathys-llm-wiki-pattern-builds-knowledge-that-actually-compounds-31a08528665e)
- [Analytics Vidhya - LLM Wiki Revolution](https://www.analyticsvidhya.com/blog/2026/04/llm-wiki-by-andrej-karpathy/)
- [Agentpedia - Karpathy's LLM Wiki: Complete Guide to His Idea File](https://agentpedia.codes/blog/karpathy-llm-wiki-idea-file)
- [Tolkien Gateway (the fan-wiki Karpathy cites)](https://tolkiengateway.net/wiki/Main_Page)
- [qmd - local hybrid markdown search (referenced in the gist)](https://github.com/tobi/qmd)
