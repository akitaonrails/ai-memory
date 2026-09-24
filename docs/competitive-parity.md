# Competitive parity & migration-worthiness

A self-critical internal audit, September 2026. Sibling to the public
[`comparison.md`](comparison.md) (the fair pitch) and
[`research-2026-landscape.md`](research-2026-landscape.md) (what each competitor
does + what to borrow). This doc asks the harder, honest question the pitch
cannot:

> **In the goals where we overlap a competitor, are we actually *better* — or did
> we copy their ideas without improving on them? If I already use competitor X,
> is ai-memory worth migrating *to*? What do I gain that X doesn't have, and what
> do I give up?**

The premise being tested: *we do the same basic stuff and have extra to justify
the switch.* Where that premise holds, migration is real; where we only matched
(or fell behind), it isn't. Every ai-memory claim below was verified against the
code, not the marketing; competitor claims against their primary sources. Goals
that simply differ from ours are called out as "different buyer — not a migration
target," not forced into a comparison.

## The migration bar

A competitor's user should switch only if ai-memory (a) does the basic thing they
rely on **at parity or better**, and (b) adds something they can't get where they
are. So each verdict rests on two honest ledgers: **our verified moat** (the
"extra") and **our real gaps** (where "better" is not yet true).

## ai-memory's verified moat (the "extra") — real in code

None of the competitors below has more than one or two of these; none has all.

- **Zero-LLM default path.** Capture, FTS5 + entity + link RRF retrieval, local
  embeddings, and rule-based summaries all work with no API key (invariant #13).
- **Files are the source of truth.** A git-backed markdown wiki, committed every
  consolidation pass — `grep`/diff/Obsidian/rsync it. The DB is a rebuildable
  index. (Letta's own "Is a Filesystem All You Need?" concession validates this.)
- **One self-contained binary** — bundled SQLite, vendored libgit2, no sidecar,
  no external DB, no graph server.
- **Cross-harness lifecycle capture** across 20+ harnesses through a typed
  sanitizer boundary (invariants #5/#6).
- **Typed claim-once handoffs + cross-project agent messaging** — protocols, not
  "both agents read the same files."
- **Multi-user shared-within-project** ("pages shared, batons owned", invariant
  #16) — self-hosted, no paid tier.
- **A single honest, reproducible benchmark** (0.823 hit@5 local; 0.668 zero-LLM).

## Migration verdicts, by competitor

| From | Migrate to ai-memory? | You gain | You give up |
|---|---|---|---|
| **mcp-memory-service** (fact-row twin) | **Yes** if you liked hook capture + typed edges + honest numbers | Files-as-truth (not opaque fact rows), handoffs, cross-project messaging, `as_of` temporal, a marginally better + reproducible number, and — new on 2.4 — its aging set at parity done zero-LLM/reversible/off-by-default (per-tier decay curves, extractive compression, DBSCAN cold-cluster dedup, access boosts, 0.4–0.75-band contradiction flagging) | Cloud/multi-backend replication (Cloudflare/Milvus), graph viz. (Belief-strength + clustering consolidation are no longer a give-up — ai-memory now ships both, clustering as a zero-LLM default-off pass, belief-strength off/R2-gated) |
| **basic-memory** (file-first sibling) | **Yes** for multi-harness coding continuity; **no** for a personal Obsidian KB with live team co-edit | Ambient capture (no `write_note` ceremony), full lifecycle, handoffs/messaging, git-versioned truth, published numbers | Local zero-cost reranking, real-time collaborative editing, Postgres backend, `build_context` graph-walk, hosted mobile/web UX |
| **agentmemory** (ideological sibling) | **Yes** for operability + data ownership | Self-contained binary (no `iii-engine` sidecar), files-as-truth, SQL indexes committed in-txn, typed handoffs, Windows parity, fuller auth | ~13 pts raw R@5 (they rerank), a P2P mesh-sync primitive, ~31 tools of (mostly speculative) surface |
| **Claude Code built-in memory** | **Yes** for teams / multi-machine / multi-harness; **not obviously** for a solo dev on one machine in Claude only | Cross-machine, cross-harness, team sharing, tool-lifecycle capture, real search | Zero setup — it's already on with no server to run (our one honest structural disadvantage for the solo case) |
| **Hindsight** (pages-over-facts) | **Yes** for self-hosted/offline/team; **no** if you need their published accuracy + are fine with an LLM-required cloud | Zero-LLM, files you own, one binary, in-project team sharing (vs strict per-bank isolation), and — new on 2.4 — a belief-strength `confidence` + an opt-in "dream" rewrite that borrow their model but never delete a source and keep the zero-LLM default | Their published headline accuracy, VLM/LLM extraction depth, and belief-strength that *actually moves ranking by default* (ours ships but is off/R2-gated until an eval proves it) |
| **Zep/Graphiti** (temporal KG) | **Yes** for self-hosting coders avoiding Neo4j+LLM (Community Edition is discontinued); **no** for enterprise graph-query/world-time | One binary on SQLite, zero-LLM, files, coding-harness native, a supported self-host | True bi-temporal (world vs observed time), Cypher/BFS graph queries, custom entity/edge types, `minRating` |
| **Mem0** | **Mostly no — different buyer** (app end-user personalization). Yes only if you (mis)used it as coding-session memory | Zero-LLM, per-repo, cross-harness, editable pages, no API spend | Mem0's SDK/integration ecosystem + managed cloud personalization |
| **Letta / MemOS** | **No** if you're building your agent *on* Letta; **yes** if you only wanted your existing coding agent to remember | Additive memory under your existing harness, zero-LLM, no runtime adoption | The ADE + agent-framework machinery (which you don't need unless building on Letta) |
| **Honcho** (Plastic Labs) | **No — different problem.** It models the *human* (theory-of-mind peer representations); we remember the *project* | If you (mis)used it for coding continuity: file-first git truth, zero-LLM, single-binary self-host, per-project scoping, no mandatory LLM egress | Its user-psychology reasoning engine (Deriver/Dreamer/Dialectic), published user-recall benchmarks, managed cloud — none of which a coding-memory user needs |
| **LiquidLM** (solo-built hosted memory API, Supermemory camp) | **Weak — different job.** It's a cloud "second brain" with multimodal RAG; we're automatic, file-first coding memory | Data ownership (markdown+git, no opaque cloud), zero-LLM default, single-binary self-host, automatic hook capture, per-project team sharing, no API/subscription spend | Its multimodal ingestion (video/audio/PDF/Office), polished web app + grounded chat, managed hosting, GitHub-sync connector, and out-of-the-box rerank |
| **Engram** (self-hosted coding-agent store, issue #810) | **Yes** — the closest newcomer to our lane | Files-as-truth git-markdown wiki (vs SQLite-as-truth + git-synced compressed chunks), **automatic lifecycle-hook capture** (vs agent-driven `mem_*` MCP saves), typed claim-once handoffs + cross-project messaging, multi-user page sharing, published numbers | Engram's zero-config Go binary is equally lean; you give up its managed Cloud replication add-on. A near-tie on posture — the switch is capture model + wiki-vs-rows, not moat depth |
| **EverOS** (file-first sibling, issue #810) | **Yes** for zero-LLM/single-binary/team; **no** if you want its LanceDB vector tier or user-track modeling | Zero-LLM default (EverOS's minimal tier still needs an LLM for the core flow), one self-contained binary (vs a Python stack), cross-harness lifecycle capture, typed handoffs/messaging, published numbers | EverOS's LanceDB vector index, its first-class user `episodes/profile` track, and its packaged offline "memory evolution" UX |
| **memU** (LLM-wiki + skill distillation, issue #810) | **Partly** — same file-first instinct, different default | Zero-LLM capture/retrieval by default (memU's distill flow is LLM-required), one binary (vs Python + sidecars patching host instruction files), MCP + hooks, git-versioned truth | memU's turnkey **skill-distillation-into-Markdown** packaging and its hosted cloud (memu.so). Our nearest equivalent is the opt-in experience/auto-improve pass, not a one-command skill distiller |
| **Caura** (governed fleet memory, issue #810) | **No — different buyer** (org-scale multi-agent governance) | If you (mis)used it for one project: file-first git truth, zero-LLM, single-binary self-host, per-project scoping, no LLM egress | Its **fleet-governance layer** — org/team/agent visibility scopes and **four cross-fleet trust tiers** — which ai-memory does **not** match (see honest gaps); plus its managed platform and self-reported LoCoMo/LongMemEval numbers |
| **TencentDB Agent Memory** (proxy fan-in server, issue #810) | **Mostly no — different capture model** | Hook/MCP capture into a file-first wiki you own (vs a base-URL proxy intercepting traffic), zero-LLM default, one binary vs a Node three-service stack, typed handoffs | Its zero-code proxy integration and its **CodeGraph** codebase index (adjacent code-intelligence we don't do). Note: needs no Tencent DB (SQLite default) — the name misleads |
| **Mem0/Zep for apps; OpenViking, Supermemory, LiquidLM, Honcho, Caura** | different buyer (app/user personalization, hosted context DB / RAG-as-a-service, user-modeling, org-scale fleet governance) — **not migration targets** | — | — |

Net: the premise holds where it should. For a **self-hosted, multi-harness,
team, zero-LLM, data-ownership** coding-memory use case, ai-memory does the basics
and adds a moat none of them fully have — migration is justified. It does *not*
hold for app/user personalization (Mem0/Supermemory/LiquidLM), user-modeling /
theory-of-mind (Honcho), enterprise graph queries (Zep Cloud), agent-building
runtimes (Letta), or a solo dev happy with Claude's built-in — those are
different buyers, and we should say so rather than overclaim.

## Did we copy without improving? — the borrowed-ideas audit

The sharp finding: ai-memory did **not** cargo-cult this field. Of the ideas it
took, most are at parity-or-better; the exceptions are **honestly-scoped-but-
unfinished substrate**, each deliberately gated behind "prove it with an eval
first." The risk is not shallow copying — it is that borrowed ideas sit **dormant**
until the eval harness (R2) that would justify activating them gets built.

| Borrowed idea | From | Verdict | Detail |
|---|---|---|---|
| Local embeddings (all-MiniLM-L6-v2) | mcp-memory-service | **BETTER** | Pure-Rust candle (no ONNX/C++ per target), checksum-pinned, default-on with background backfill |
| Hook capture | mcp-memory-service | **BETTER** | 20+ harnesses + typed sanitizer boundary + backpressure vs Claude-Code-centric |
| Versioned supersession | agentmemory | **BETTER** | A concurrent divergent write supersedes but the loser stays reachable (invariant #16); theirs flips `isLatest` |
| Retention-as-formula (decay) | agentmemory | **BETTER** | Real SQL decay math vs in-memory O(N²) Jaccard capped at 1000 rows; 2.4 adds opt-in per-tier half-life curves (identity default) |
| Per-tier retention curves (365/180/90/30) | mcp-memory-service | **SHIPPED (parity), better default** | `[decay.half_life_days]` per tier (2.4, #807); their shape, but the omitted-key default is byte-identical to the old single-λ — no upgrade mass-evicts |
| Extractive, zero-LLM compression | agentmemory / mcp-memory-service | **BETTER** | 2.4 tier-down (#808) keeps abstract+summary+regex keep-tokens and drops prose, but is **reversible** (git + supersession chain, `restore-page`) and off by default, not a lossy in-place rewrite |
| Cold-cluster dedup (DBSCAN, adaptive-eps) | mcp-memory-service | **SHIPPED (parity), better safety** | 2.4 (#812) over the bounded cold set (no O(N²)); collapses by **supersede-not-delete** (invariant #16), conservative eps ceiling, zero-LLM, off by default |
| Access-weighted retention (access boost) | mcp-memory-service | **SHIPPED (parity+), closed a real gap** | 2.4 (#798) wired reinforcement into `read_page`, the related-walk, and `explore` — every read path, not just search; throttled/async/FTS-exempt |
| Contradiction detection (0.4–0.75 band) | mcp-memory-service | **SHIPPED (parity), advisory-only** | 2.4 (#814) surfaces likely conflicts in `memory_lint` from existing embeddings; zero-LLM, never edits/deletes/persists an edge |
| Surprisal-first + idle/cancel dream scheduling | Honcho (Dreamer) / Supermemory | **SHIPPED (parity), opt-in** | 2.4 (#816) idle-scheduled, cancels on activity, orders most-novel-first; off by default, observable `DreamReport` per run |
| Git-as-snapshot | agentmemory | **BETTER** | The wiki files *are* the truth (diffable), not a `state.json` dump |
| Pages-over-facts / living pages | Hindsight | **PARITY** (native) | Same architecture, file-first vs their Postgres-first; `settled_first` briefing shipped |
| Cross-session experience/abstraction pass | Letta/Hindsight/surveys | **REAL, ~parity** | ≥2-session evidence guard is the genuine article — but LLM-required and **off by default** |
| Typed relation edges | graph camp | **PARTLY SHIPPED** | Link-neighbours are now a first-class retrieval surface — `memory_read_page include_related` walks the `page_links` graph outward (bounded BFS, #775) — but retrieval **RANK weighting by edge type is still not done** (edge kind stays explain-only in `memory_query`) |
| Belief-strength consolidation | Hindsight / mcp-memory-service | **SHIPPED, off by default (parity of mechanism, unproven)** | 2.4 (#815) computes a read-time, zero-LLM `confidence` over `page_evidence` (distinct-session breadth, recency, live `contradicts`, capped `[0,0.95]`), exposed inertly in `explain`/`status`; foldable into ranking authority behind a **default-0.0, R2-gated** weight. Our improvement over Hindsight's documented **entrenchment** failure: breadth-not-raw-count weighting, recency shading, a confidence cap, and **supersession always wins** regardless of evidence. Not yet default-on — no R2 delta run |
| LLM merge/rewrite "dream" pass | Hindsight (Dreamer) / Supermemory ("dreaming") | **SHIPPED, opt-in/off (parity of mechanism, unproven)** | 2.4 (#816) rewrites cold clusters into one page via the gated `apply_batch` path, `dry_run` first, JSON-schema output; **never deletes a source** (superseded with a merge-note stub, `restore-page` recovers), off by default, provider-less stores keep the zero-LLM A3 path. Off until an R2 quality delta is shown |
| bi-temporal-lite | Zep/Graphiti | **BEHIND by design** | Ingestion-time only; world-time validity deferred (honestly, because dating facts needs an LLM and breaks the zero-LLM path) |

## Honest gaps (consolidated) and recommended adjustments

Documented recommendations only — **nothing here is scheduled or to be
implemented without a separate decision.** Ordered by leverage.

1. **R2 reproducible eval harness — now built (#771/#772).** It was the
   meta-blocker: belief-strength ranking, typed-edge rank weighting, and a
   published accuracy/latency/context-tokens *triple* were all gated on "prove it
   with a number." The harness now measures that triple **and** end-to-end
   QA-accuracy (LLM-as-judge), so the gate is open; what remains is running the
   full-500 matrix and then activating the dormant substrate below against it.
   Illustrative sample-20 numbers are in
   [`benchmarks/retrieval-ab-r2.md`](benchmarks/retrieval-ab-r2.md); the
   published full-500 triple is still pending.
2. **A zero-LLM / local reranker (candle cross-encoder).** The single most-cited
   weakness: our only reranker is LLM-as-judge, so the zero-LLM default path —
   our headline — has *no* reranking, while basic-memory ships a local one and
   agentmemory/Hindsight's score lead is largely reranking (0.823 vs 0.952 same
   dataset). Closes a gap on our own keyless turf. Gate on R2.
3. **Activate the dormant substrate — belief-strength now wired, off by
   default (2.4, #815).** `page_evidence` confidence is now computed at read
   time (zero-LLM) and can be folded into `PageAuthority` behind a clamped,
   **default-0.0** `[retrieval] belief_authority_weight` — the design in
   [`design-hindsight-borrowings.md`](design-hindsight-borrowings.md) §3, with
   the anti-entrenchment guards baked in. It is exposed inertly in
   `memory_query explain` and `memory_status` today; **it does not move ranking
   until an operator sets the weight, and turning it on is gated on a positive
   R2 delta that has not been run.** Still genuinely behind: **typed-edge rank
   weighting** (edge kind stays explain-only) and a `contradicts` *authority*
   cap — A5 (#814) surfaces contradictions in `memory_lint` but does not yet
   feed rank.
4. **Sharpen the cross-machine story.** `comparison.md`'s "cross-machine *by
   construction*" is overstated — it's one shared server + manual git/rsync, with
   **no automatic replication** (agentmemory ships a P2P primitive; mcp-memory-
   service has Cloudflare/Milvus). Either reframe as "one server, many machines"
   + an explicit git/rsync note, or design a bounded `ai-memory sync` over the git
   wiki (push/pull the source-of-truth dir, rebuild the index) — a cleaner,
   file-first P2P story than an opaque KV LWW.
5. **Close the setup-friction gap vs the built-in.** Our one honest structural
   weakness for the solo user. Much is already done (`run` auto-wire, `doctor`,
   `backfill`, `install-self-routing`); surface it as *the* onboarding path in the
   README hero, add a one-line `ai-memory run claude` quickstart, and ship a
   "coming from Claude built-in?" importer (the `~/.claude/.../memory/` format is
   nearly identical markdown + `type` frontmatter — a cheap, strong on-ramp).
6. **Smaller, competitor-specific closes — five now shipped on 2.4 (opt-in,
   defaults byte-identical):** the graph-walk retrieval tool (basic-memory's
   `build_context`/`memory://`, LiquidLM's `follow_references`/`get_related`)
   shipped as `memory_read_page include_related`/`related_depth` (#775), a bounded
   BFS exposing the link-neighbour walk over `page_links` we already compute; the
   **dialectic/oracle query** (Honcho) shipped as `memory_query answer=true`
   (#782) — a synthesized+cited answer over the hits, LLM-required and off by
   default (graceful `answer_unavailable` with no provider); the **reasoning-tier
   knob** (Honcho) shipped as `memory_query`/`memory_explore
   reasoning: {minimal..max}` (#783), mapping to a per-tier token budget; the
   **pin-before-search contract** (LiquidLM) shipped as `memory_query pin_first`
   + `memory_briefing.pinned` (#780); and the **"hide superseded unless asked"
   retrieval knob** shipped as `memory_query include_superseded=true` (#773) over
   our supersession chains. All five are strictly opt-in LLM/retrieval layers
   *over* the zero-LLM FTS/RRF core, never a requirement (invariant #13).
   The **memory-aging** set (A1–A5, B1–B4, C1) landed on 2.4 as well —
   per-tier decay curves, extractive tier-down, DBSCAN cold-cluster dedup,
   the contradiction band, access reinforcement on all read paths,
   belief-strength confidence, and the LLM dream pass — audited row-by-row in
   the borrowed-ideas table above; the design of record is
   [`design-memory-aging.md`](design-memory-aging.md).
   **Still unshipped:** MCP tool behavior hints
   (readOnly/destructive/idempotent) on the 23-tool surface; typed-edge **rank**
   weighting and a `contradicts` authority cap (A5 flags, it doesn't yet rank);
   a read-only graph visualization in `/web`; a first-class TS SDK story for
   ecosystem parity.
   Deliberately out of scope: Honcho's theory-of-mind user-modeling engine and
   LiquidLM's opaque-cloud / LLM-mandatory / multimodal-second-brain substrate
   (all off-mission for file-first, zero-LLM coding memory; the global-scope
   preferences page already covers the useful user-prefs slice).
7. **Fix doc staleness the audit surfaced** (accuracy, not features):
   `research-basic-memory.md` understates basic-memory's shipped cross-encoder
   reranking + Teams tier; `research-agentmemory.md` says 53 tools / 124 endpoints
   (now 54 / 130); `comparison.md` attributes agentmemory's `0.967` to
   LongMemEval-S when it's their in-house `coding-agent-life-v1` (the like-for-like
   LongMemEval-S is 0.952 vs our 0.823).
8. **Fleet-scale trust governance — a real gap vs Caura (issue #810).** Our
   multi-user story is per-project (`pages shared, batons owned`, invariant #16)
   with a root→DB-user→OIDC auth ladder and an audit log — deliberately
   project-scoped. Caura adds an axis we do not have: org/team/agent visibility
   scopes plus **four cross-fleet trust tiers** governing read/write/delete across
   hundreds of agents, with per-write PII flagging. This is genuinely off-mission
   for file-first coding memory (it belongs to the org-fleet buyer), so the honest
   move is to **say so, not to chase it** — but it should not be waved away as
   "we already do multi-user." We do project multi-user; we do not do fleet
   governance.

## Bottom line

- **The moat is real and verified:** zero-LLM default, files-as-truth, one binary,
  cross-harness capture, claim-once handoffs + messaging, and multi-user
  shared-within-project. On operability, data ownership, and cross-agent/team
  continuity, ai-memory is genuinely ahead of every overlapping competitor.
- **The borrowed conveniences now ship (opt-in), and so does the aging set.**
  The 2.4 line landed the five competitor-specific closes above — related-walk,
  dialectic answer, reasoning tier, pin-before-search, and the show-superseded
  knob — plus the memory-aging program (per-tier decay curves, extractive
  tier-down, DBSCAN cold-cluster dedup, contradiction flagging,
  access-weighted retention, belief-strength confidence, and the LLM "dream"
  pass), each opt-in with defaults byte-identical. Our consistent improvements
  over the sources borrowed from (mcp-memory-service, agentmemory, Honcho,
  Hindsight, LiquidLM): **zero-LLM by default**, **reversible** (supersede-not-
  delete + `restore-page`, never a lossy in-place rewrite or a deleted source),
  **file-first**, and **R2/opt-in discipline** on anything that touches ranking
  or calls an LLM. The R2 harness that gates the quality work now exists and
  measures the accuracy+latency+context-tokens triple plus end-to-end
  QA-accuracy (illustrative sample-20 only so far).
- **The honest weakness is still retrieval quality**, and it remains
  *self-inflicted by discipline, not by copying*: no local zero-LLM reranker
  exists, belief-strength (`page_evidence`) is now wired but **default-off**,
  typed-edge **RANK** weighting is still not built, and the published
  *full-dataset* triple is still pending (only illustrative sample-20 numbers so
  far). No R2 delta has been run, so nothing quality-affecting defaults on. The
  gate (R2) is now
  open — **activate what's already there against a full-500 run.**
- **Migration answer:** for the self-hosted, multi-harness, team, zero-LLM,
  own-your-data coding-memory user, yes — we do the basics and add what they
  can't get elsewhere. For app personalization, enterprise graph queries, agent
  runtimes, or a solo dev content with the built-in, no — and we should keep
  saying so.
