# Hindsight (Vectorize) - Research Report

> Source project: `vectorize-io/hindsight` (Python/Node/Go, PostgreSQL+pgvector,
> MCP-native, LLM-required). MIT, ~23.5K stars, ~2,970 commits, production at
> named enterprises, a preprint paper (arXiv:2512.12818), and a LongMemEval
> claim of 91.4%. Studied as the **best-funded, paper-backed, benchmark-leading
> competitor to date** - and, importantly, as an *independent* implementation
> of two bets we already made (pages-over-facts; auto-rewritten wiki pages of
> settled knowledge). A follow-up to the September 2026 landscape
> ([`research-2026-landscape.md`](research-2026-landscape.md)); the landscape's
> camps table and its R2/R5 recommendations are updated there to name this
> project. Same lens as the rest: what it does, what is worth borrowing, what to
> deliberately avoid. Every load-bearing claim was checked against the repo, the
> paper, or a primary announcement in September 2026, not remembered.

## 1. Purpose & Scope

Hindsight is an **agent memory system built "to create smarter agents that
learn over time"** - the pitch leans hard on *learn*, not *store*. Its
organizing metaphor is biomimetic: four "logical networks" that separate what
the world is, what the agent did, what it has concluded, and what it now
believes. It is MCP-native, LLM-required, and packaged for everything from a
2-line Docker run to a managed SaaS with a 99.9% SLA. Vectorize (the company)
positions it enterprise-first (Fortune 500 references, Kubernetes Helm charts,
Oracle 23ai parity), which is the opposite end of the deployment spectrum from
our single self-contained homelab binary.

The reason it matters to us is not the enterprise packaging. It is that
Hindsight independently arrived at **markdown "mental model" pages that a
background process continuously rewrites as the agent learns** - which is our
`wiki/` + auto-improve loop under a different name - and then went further into
the cross-session abstraction layer our own landysc research flagged as the
open frontier (R5). It is the strongest existing prior art for where we said we
want to go, from a team that published a paper and a benchmark to back it.

## 2. Storage Model

**A database, not files, as source of truth** - the inverse of our substrate
and of basic-memory's. Storage backends:

- **PostgreSQL + pgvector** (primary).
- **Oracle AI Database 23ai** (enterprise-parity option).
- **Embedded `pg0`** (a bundled Postgres so the Python `hindsight-all` package
  has no external DB dependency - their answer to our "single binary, no
  services" property, achieved by embedding Postgres rather than SQLite).

Memory is represented as **"entities, relationships, and time series with
sparse/dense vector representations"** - i.e. a temporal, entity-aware layer
over Postgres, with both BM25-sparse and dense vectors materialized. On top of
that raw layer sit the **four biomimetic tiers**, which are the actual product
abstraction:

1. **World facts** - static, provider-independent knowledge ("Paris is in
   France").
2. **Experiences** - the agent's own interaction trajectories (what happened,
   turn by turn). This is the *narrative/trajectory* tier the "Storage →
   Reflection → Experience" survey (`research-2026-landscape.md` §4) names, made
   first-class.
3. **Observations** - *consolidated, evidence-backed beliefs*. Each carries
   provenance: quotes and a **proof count**, so a belief's strength is the
   weight of evidence behind it, not a single write.
4. **Mental models** - *learned understanding synthesized from observations*,
   stored as **living markdown "knowledge pages" that a background process
   auto-rewrites** as the bank accumulates evidence. These are "standing
   answers": an agent "can boot with a page of settled knowledge instead of
   rediscovering it every session."

Tier 4 is the one to sit with. Structurally it is *our wiki page*, and the
"auto-rewritten as the bank learns" mechanism is *our auto-improve/consolidation
loop*. Two independent teams, from opposite substrates (they DB-first, us
file-first), converged on "the durable unit of agent memory is a
continuously-maintained markdown page of settled knowledge, not a pile of atomic
facts." That convergence is the strongest external validation of our core bet in
this whole research series - stronger than a listicle or a star count, because
it is a competitor's *architecture* agreeing, backed by a benchmark.

## 3. Capture / Ingestion

Hindsight offers a spectrum, anchored on an explicit `retain()` API call and
climbing toward automatic:

- **`retain()`** - the manual API. Behind it, **an LLM extracts "key facts,
  temporal data, entities, and relationships" through a normalization
  pipeline.** Every retain is therefore an LLM call.
- **LLM Wrapper** - transparently wraps the OpenAI / Anthropic SDKs (and 100+
  models via LiteLLM); once wrapped, "memories are then stored and retrieved
  automatically on every call." This is a **fundamentally different capture
  surface from ours**: they intercept the *model API call itself*; we intercept
  the *agent harness lifecycle* (hooks). Theirs captures any app that routes
  through the SDK, at the cost of being in the inference path and coupled to the
  LLM provider. Ours captures tool/session/prompt events harness-side, works
  with zero LLM, and never sits in the model call path.
- **IDE / coding-agent auto-ingestion** from **git history and past sessions** -
  a coding-agent specialization that seeds a bank from the repo's history.
- **MCP server** exposes `retain` as a tool.

The load-bearing difference: **capture requires an LLM.** There is no rule-based
or zero-LLM path. Every memory written pays an extraction-LLM call. Our
zero-LLM default (capture + FTS + rule-based summaries with no provider) has no
analogue here - which is precisely the homelab/offline segment Hindsight cedes.

## 4. Retrieval

Four strategies run **in parallel**, then fuse:

- **Semantic** - dense-vector similarity.
- **Keyword** - BM25 exact matching.
- **Graph** - entity / temporal / causal link traversal.
- **Temporal** - explicit time-range filtering (`recall("What happened in
  June?")`).

Merged via **reciprocal rank fusion + a cross-encoder reranking model.** This is
the same shape as ours (RRF over multiple streams, optional rerank) with two
streams we treat differently: their "graph" stream traverses typed
entity/causal edges as a first-class retriever, and their "temporal" stream is a
dedicated time-range filter rather than a decay weighting. Our recent
bi-temporal work (`docs/temporal.md`, the `as_of` FTS stream on `release/2.2`)
is the closest we have to their temporal retriever; their causal-graph stream is
the direction our typed-edges thread (`fixes`/`causes`/`contradicts`) points at.

## 5. Memory Lifecycle - Consolidation over Replacement

This is Hindsight's sharpest idea and its clearest lesson for us.

**Memory does not overwrite. "New information strengthens, weakens or extends an
existing belief instead of silently replacing it."** An observation is an
evidence-backed belief with a proof count; corrections adjust the belief's
weight and lineage rather than deleting the prior version. Mental-model pages are
**rewritten continuously "as the bank learns more"** by a background process -
consolidation is a standing job, not a per-write action.

Compare to us: our pages **supersede** (the loser stays reachable in a
supersession chain; invariant #16), which is corrections-as-versions. Hindsight
adds a dimension we do not have: **belief *strength* as an accumulating,
evidence-weighted quantity**, with the supporting quotes and a proof count
attached. "The team decided against a queue" is not just the latest page; it is a
belief with N pieces of evidence behind it, and a contradicting sighting
*weakens* it rather than replacing the page. That is a richer model than binary
supersession, and it is exactly the substrate a `contradicts` edge + a lint pass
wants (our R3). Worth noting the failure mode too: an evidence-weighted belief
store can entrench a popular-but-wrong belief (many weak confirmations
outvoting one correct correction) - a known risk their "disposition traits"
(below) gesture at but do not obviously solve.

## 6. Isolation Model - "Banks" (and why #708 should read this)

A **bank** is "an isolated memory store - one 'brain' for one user, agent, or
project. Isolation is strict: no cross-bank leakage." This is the **opposite of
our core bet.** Our invariant #16 makes pages *shared* within a project across
sessions and users on purpose (team continuity); owner-scoping applies only to
handoff batons. Hindsight chose strict per-bank isolation and gave up the
in-project team-sharing story to get a simpler tenancy boundary.

The timely connection: **issue #708** ("multi-user servers have no per-project
authorization") is asking us to add exactly a bank-like isolation boundary
*above* our shared-within-project model. Hindsight's bank is the prior art for
that design - one brain per (user | agent | project), strict no-leakage - and
its choice is instructive: they get tenant isolation for free but cannot do
in-project multi-user collaboration, which is our differentiator. The #708 design
should be "grants that gate *which projects a user can see at all*, above the
shared-within-a-granted-project model," i.e. a bank boundary layered on top of
our sharing, not a replacement for it. The identity substrate #709 tried to add
(project identity that is not the folder name) is the same prerequisite
Hindsight's per-bank keying needs.

## 7. LLM Dependency & Providers

**LLM required** for retain (extraction) and reflect (synthesis). 25+ providers:
hosted (OpenAI, Anthropic, Gemini, Groq, Bedrock, Vertex, Deepseek, Meta),
local (Ollama, LM Studio, llama.cpp), gateways (LiteLLM/Router, any
OpenAI-compatible endpoint), and - notably - **existing subscriptions with no
API key** (ChatGPT Plus, Claude Pro, GitHub Copilot). That last one is a real
onboarding advantage worth borrowing conceptually: we already support Copilot
and Anthropic-OAuth tokens; framing "use the subscription you already pay for,
no API key" as a first-class install path (rather than a buried option) is a
low-cost positioning win.

But the hard requirement is the story: **there is no zero-LLM Hindsight.** Turn
the provider off and it does not capture. Our default path (capture, FTS search,
rule-based summaries, all with no provider) is the segment they structurally
cannot serve.

## 8. Deployment, Packaging, Privacy

- **Deployment (five):** Docker single-container, Docker Compose (external PG),
  bare-metal pip, **Kubernetes Helm**, and **Hindsight Cloud** (managed SaaS,
  usage billing, 99.9% SLA). Clients: Python (`hindsight-all` embedded /
  `hindsight-api` server), Node (`@vectorize-io/hindsight-client`), Go, a CLI,
  and a built-in MCP server at `http://localhost:8888/mcp/{bank_id}/` exposing
  `retain`/`recall`/`reflect`.
- **60+ integrations** - coding agents (Claude Code, Cursor, Copilot, Cline,
  Aider), frameworks (LangGraph, LlamaIndex, CrewAI, Pydantic AI), no-code (n8n,
  Zapier, Dify, Flowise). This breadth is a company-scale investment we will not
  and should not match; our integration story is depth on the coding-agent
  lifecycle, not breadth across no-code platforms.
- **Memory Defense** - an *optional, per-bank* policy that scans retains against
  **45 patterns** for secrets/PII and can **redact** (`[REDACTED:github_token]`,
  a *typed* redaction label) or **block** outright. This is a more configurable
  version of our sanitizer (invariant #6): ours is a fixed built-in trust
  boundary on the hook path; theirs is per-scope-configurable with a
  redact-vs-block choice and typed redaction labels. The **typed label**
  (`[REDACTED:<what>]` instead of a blanket mask) is the borrowable detail - it
  preserves that *a* github token was there without leaking it, which is more
  useful for later reading than an anonymous `***`.
- **Multilingual by default** - input language detected and preserved end to
  end.

## 9. The Benchmark Claim - Read Skeptically (per the mempalace lesson)

The paper - **"Hindsight is 20/20: Building Agent Memory that Retains, Recalls,
and Reflects"** (Latimer, Boschi, Neeser, Bartholomew, Srivastava, Wang,
Ramakrishnan; arXiv:2512.12818, **preprint**, submitted 2025-12-14) - reports:

- **LongMemEval: 91.4%** accuracy (scaled backbone) vs **39%** full-context
  baseline. Sub-scores show where structure helps most: multi-session
  **21.1% → 79.7%**, temporal reasoning **31.6% → 79.7%**, knowledge-update
  **60.3% → 84.6%**.
- **LoCoMo: 89.61%** vs 75.78% for the strongest prior open system.

Two things are genuinely better than the mempalace pattern the landscape flagged
(§2, the ~47K-star system whose headline number reproduced with the architecture
*inactive*): Hindsight published a **paper with per-category sub-scores** (not a
single cherry-picked figure), and the gains concentrate exactly where a
structured temporal memory *should* help (multi-session, temporal,
knowledge-update) rather than uniformly, which is the signature of a real
mechanism rather than a lucky default.

But apply the same skepticism the landscape earned:

- **"Independently reproduced by Virginia Tech and The Washington Post" is not
  arms-length.** Wang and Ramakrishnan (two of the seven authors) are Virginia
  Tech Sanghani Center faculty; the Post is a named development collaborator.
  The reproduction is by the *co-developing* institutions, which is more than
  self-report but is not third-party. The claim should be cited as "reproduced
  by the collaborating labs," not "independently verified."
- It is a **preprint**, not peer-reviewed.
- LongMemEval numbers here are **accuracy**, not the R@5 retrieval metric other
  systems in our series report (agentmemory 0.967 R@5, mcp-memory-service 80.4%),
  so the 91.4% is not directly comparable to those - a reason our own R2 harness
  must fix a metric and a split, not quote a competitor's.

Net: it is the most *credible* memory-benchmark claim in this research series
(paper + sub-scores + a plausible mechanism), and simultaneously a reminder that
the serious players now ship papers - which raises the bar our R2 gap sits under.

## 10. Disposition Traits - Flagged as Possible Metaphor-Driven Cruft

Banks carry **"disposition traits" (skepticism, literalism, empathy)** that
"shape how reflect reasons." Treat this with mempalace-grade suspicion until the
paper's ablation says otherwise. It is exactly the kind of anthropomorphic
flourish that reads well in a launch post and may or may not move a retrieval
number; the burden of proof is an ablation showing traits-on beats traits-off on
a held-out split. Absent that, it is a prompt-flavoring knob dressed as an
architecture, and the landscape's mempalace conclusion applies: metaphor is not
retrieval architecture. (The one place it *could* be load-bearing is the
entrenchment failure mode of §5 - a "skeptical" disposition that discounts weak
repeated confirmations - but the docs don't claim that mechanism explicitly.)

## 11. Strengths Worth Borrowing

1. **"Mental models" as continuously-rewritten markdown pages of settled
   knowledge.** This is our wiki + auto-improve, validated by an independent,
   benchmark-backed competitor. The specific thing to study: they make the
   *standing-answer page* a first-class tier an agent boots from, and rewrite it
   as a background job across accumulated evidence. That is our open R5
   (cross-session abstraction) with a shipped reference implementation - read
   their reflect loop before designing ours.
2. **Consolidation as belief-strength, not just supersession.** Evidence-backed
   observations with quotes + a proof count, where new evidence
   strengthens/weakens/extends rather than replaces. Richer than our binary
   supersession, and the natural substrate for the `contradicts` lint (R3) and
   temporal validity (R4). Consider a "confidence/evidence-count" field on
   pages or entity-links.
3. **Typed redaction labels in the sanitizer.** `[REDACTED:github_token]`
   instead of an anonymous mask - preserves *what kind* of secret was present
   for the later reader without leaking it. A one-line refinement to our
   sanitizer boundary.
4. **Four parallel retrieval streams with an explicit causal-graph retriever.**
   Their graph stream traverses `causes`/typed edges as a retrieval path, not
   just for display. Validates the typed-edges direction (R3) as *retrieval*
   value, not only lint value.
5. **"Use the subscription you already pay for" as a headline install path.** We
   support Copilot/Anthropic-OAuth already; promoting "no API key, use your
   Claude Pro / Copilot" to a first-class onboarding line is free positioning.
6. **Bank isolation as prior art for #708.** A clean, documented per-(user |
   agent | project) tenancy boundary to design our authorization against -
   adopting the *boundary* while keeping our shared-within-project collaboration
   (which they gave up).

## 12. Weaknesses / Friction (Avoid)

1. **LLM-required capture.** No zero-LLM path; every write costs an extraction
   call. This is the segment we own by construction - do not follow them into
   provider-mandatory capture.
2. **Postgres-primary, DB-as-source-of-truth.** Heavier substrate (pgvector, or
   an embedded Postgres, or Oracle), enterprise/K8s-shaped. Our single
   self-contained binary with files as source of truth stays the homelab
   differentiator; the "embedded pg0 so there's no external dependency" move is
   them working to recover a property we get for free.
3. **Strict bank isolation loses in-project team memory.** Their simplicity
   costs the multi-user-within-a-project collaboration that is our invariant
   #16. When #708 adds authorization, layer a bank-like boundary *above*
   sharing; do not replace sharing with isolation.
4. **Disposition traits** - unproven metaphor-as-architecture until an ablation
   says otherwise (§10). Do not add "personality" knobs without a held-out
   number.
5. **Benchmark framing** - "independently reproduced" by co-developers, a
   preprint, accuracy-not-R@5. A caution for how *we* report R2: fix the metric
   and split, publish the harness, never a bare number.
6. **60+ integration breadth** is a company-scale surface. Depth on the coding
   agent lifecycle beats chasing no-code-platform breadth we cannot maintain.

## 13. Bottom Line

Hindsight is the most serious competitor in this series and, usefully, it agrees
with us where it matters: **the durable unit of agent memory is a
continuously-maintained markdown page of settled knowledge, not a bag of atomic
facts.** Two teams from opposite substrates converged there, and theirs has a
paper and a benchmark. That is the strongest external validation of our core bet
we have logged.

Borrow four concrete things: **belief-strength/evidence-count consolidation**
(richer than our supersession, feeds R3/R4), **typed redaction labels**, the
**causal-graph retriever** as evidence that typed edges pay off in retrieval not
just lint, and their **reflect loop as the reference implementation for our open
R5** (cross-session abstraction). Read their **bank model as prior art for
#708**, adopting the isolation boundary while keeping our shared-within-project
collaboration. And let their **paper sharpen R2**: the serious players now
publish audited-ish numbers, we publish none, and "experienced colleague over
long horizons" is still literally our pitch.

Deliberately do **not** follow them into: LLM-required capture (we own zero-LLM),
a Postgres/enterprise substrate (single binary is the homelab win), strict
isolation *instead of* sharing (invariant #16 is the team story), or
personality/disposition knobs without an ablation (the mempalace lesson holds).

## 14. Sources

- Repo: `github.com/vectorize-io/hindsight` (README, main branch, MIT;
  ~23.5K stars / ~2,970 commits as of Sept 2026).
- Paper: "Hindsight is 20/20: Building Agent Memory that Retains, Recalls, and
  Reflects," Latimer, Boschi, Neeser, Bartholomew, Srivastava, Wang,
  Ramakrishnan - arXiv:2512.12818 (preprint, 2025-12-14).
- Vectorize announcements: vectorize.io/blog "Introducing Hindsight"; PRNewswire
  "Vectorize Breaks 90% on LongMemEval with Open-Source AI Agent Memory System";
  VentureBeat "With 91% accuracy, open source Hindsight ...".
- Independent-reproduction claim: attributed to Virginia Tech Sanghani Center
  for AI and Data Analytics + The Washington Post - **noted as co-developing
  collaborators, not arms-length third parties** (two paper authors are VT
  Sanghani faculty).
- Cross-references: `research-2026-landscape.md` (camps table, R2/R5),
  `research-basic-memory.md` (manual-write model contrast),
  `docs/temporal.md` (our bi-temporal `as_of` work), issues #708 (per-project
  authorization) and #709 (project identity).
