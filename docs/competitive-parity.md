# Competitive parity & migration-worthiness

A self-critical internal audit, September 2026, refreshed 2026-10-18 with
the research and gap decisions below. Companion to the public
[`comparison.md`](comparison.md) (the fair pitch) and
[`research-2026-landscape.md`](research-2026-landscape.md) (what each competitor
does + what to borrow). This doc asks the question the pitch
cannot:

> **In the goals where we overlap a competitor, are we actually *better*, or did
> we copy their ideas without improving on them? If I already use competitor X,
> is ai-memory worth migrating *to*? What do I gain that X doesn't have, and what
> do I give up?**

The premise being tested: *we do the same basic stuff and have extra to justify
the switch.* Where that premise holds, migration makes sense; where we only matched
(or fell behind), it doesn't. Every ai-memory claim below was verified against the
code rather than the marketing, and competitor claims against their primary
sources. Goals that simply differ from ours are labeled "different buyer, not a
migration target" instead of being forced into a comparison.

## The migration bar

A competitor's user should switch only if ai-memory (a) does the basic thing they
rely on **at parity or better**, and (b) adds something they can't get where they
are. So each verdict rests on two ledgers: **our verified moat** (the
"extra") and **our real gaps** (where "better" is not yet true).

## ai-memory's verified moat (the "extra") — real in code

None of the competitors below has more than one or two of these; none has all.

- **Zero-LLM default path.** Capture, FTS5 + entity + link RRF retrieval, local
  embeddings, and rule-based summaries all work with no API key (invariant #13).
- **Files are the source of truth.** A git-backed markdown wiki, committed every
  consolidation pass, so you can `grep`/diff/Obsidian/rsync it. The DB is a rebuildable
  index. Git-backed markdown is now common (Letta's context repositories,
  Supermemory's memoryrepo, EverOS), so what still counts is markdown as the
  *source of truth* under a zero-LLM default.
- **One self-contained binary**: bundled SQLite, vendored libgit2, no sidecar,
  no external DB, no graph server.
- **Cross-harness lifecycle capture** across 20+ harnesses through a typed
  sanitizer boundary (invariants #5/#6).
- **Typed claim-once handoffs + cross-project agent messaging**: protocols, not
  "both agents read the same files."
- **Multi-user shared-within-project** ("pages shared, batons owned", invariant
  #16), self-hosted, with no paid tier.
- **Reproducible retrieval benchmarks**: LongMemEval-S hit@5 0.815 local and
  0.666 zero-LLM through the shipped 2.6 pipeline, 0.951 zero-LLM and 0.945 local with the 3.0
  observation stream (3.3.0: 0.951 and 0.945; the published local top-ten
  movement is tracked in #1854), and a published pipeline-vs-component decomposition
  ([`benchmarks/`](benchmarks/README.md)). Others publish numbers too (Mem0,
  Zep, Hindsight, EverOS, MemPalace, agentmemory), mostly end-to-end QA with
  their own reader and judge models. The edge is reproducibility; we claim no
  ranking.

## Migration verdicts, by competitor

Refreshed 2026-10-18. Coding-agent memory is now the most-starred part of the
field, and several app-memory products (Mem0, Supermemory, Hindsight,
OpenViking, Cognee) ship coding-agent plugins, so they appear here as
coding-agent competitors as well as in their original camps.

| From | Migrate to ai-memory? | You gain | You give up |
|---|---|---|---|
| **claude-mem** (hook capture, LLM observer) | **Yes** for zero-LLM, multi-harness or team use; **not obviously** for a solo Claude Code user happy with it | Zero-LLM capture (it compresses every observation with an LLM), git-markdown truth instead of SQLite FTS5 + Chroma, claim-once handoffs, cross-project messaging, per-project multi-user auth without a paid cloud tier | Its to-do tools, three-step progressive search, native-memory bridge, file observations, and v14's polished local “My Work” queue; TeamBrain adds paid shared-work/collision views |
| **MemPalace** (hook capture, team server) | **Partly**: overlapping goals, different substrate | Git-markdown truth, claim-once handoffs, typed cross-project messaging, the auth ladder | Its temporal graph and team server as shipped. Its 96.6% / 98.4% R@5 is retrieval-only like our hit@5, but on a different harness, so neither number ranks the tools |
| **mcp-memory-service** (fact rows + hook capture) | **Yes** if you liked hook capture + typed edges + published numbers | Files-as-truth (not opaque fact rows), handoffs, cross-project messaging, `as_of` temporal, a reproducible number, and (new on 2.4) its aging set at parity done zero-LLM/reversible/off-by-default (per-tier decay curves, extractive compression, DBSCAN cold-cluster dedup, access boosts, 0.4–0.75-band contradiction flagging) | Cloud/multi-backend replication (Cloudflare/Milvus), graph viz. (Belief-strength + clustering consolidation are no longer a give-up: ai-memory now ships both, clustering as a zero-LLM default-off pass, belief-strength off and eval-gated) |
| **basic-memory** (file-first markdown) | **Yes** for multi-harness coding continuity; **no** for a personal Obsidian KB with live team co-edit | Ambient capture (no `write_note` ceremony), full lifecycle, handoffs/messaging, git-versioned truth, published numbers, native OKF v0.2, conditional writes, temporal reads, structured query filters, and bounded document extraction in the optional importer | Local zero-cost reranking, real-time collaborative editing, Postgres backend, `build_context` graph-walk, hosted mobile/web UX, and its broader document/connectors surface. Its v0.24 features remain unreleased as of this comparison |
| **agentmemory** (hook capture, KV store) | **Yes** for operability + data ownership | Self-contained binary (no `iii-engine` sidecar, no ELv2 runtime), files-as-truth, SQL indexes committed in-txn, zero-LLM summaries and handoffs, typed claim-once handoffs, a multi-user auth ladder (DB tokens, OIDC), native Windows without a manual sidecar download (3.1 `install.ps1` + `setup`), and a 3.0 importer for its vault and `ExportData`. Both now default to auth on for new installs (theirs since 0.9.30, ours in 3.0) | Browser edit/forget (the 3.0 web UI adds page history/diff, a session timeline, a recall trace and a health panel, but stays read-only by design), the iii console's OTEL traces (we ship explain timings + Prometheus `/admin/metrics` in 3.0), a P2P mesh-sync primitive (`ai-memory-wikisync` is our git-based answer), the opt-in local cross-encoder reranker, an npm/`npx` install (ours is a two-command native install on Linux, macOS and Windows in 3.0/3.1, no npm package), ~31 more tools (54, all visible by default; mostly speculative). Benchmarks are not like-for-like: their 95.2% LongMemEval-S is a component measurement with the reranker disabled (their 2026-10-09 baseline adds 84.68% strict recall-all@5, which they say supports no competitive claim), ours is end-to-end |
| **Claude Code built-in memory** | **Yes** for teams / multi-machine / multi-harness; **not obviously** for a solo dev on one machine in Claude only | Cross-machine, cross-harness, team sharing, tool-lifecycle capture, real search | Zero setup: it's already on with no server to run (our one structural disadvantage for the solo case; 3.0's `install.sh` + `ai-memory setup` cut ours to two commands on Linux and macOS, but there is still a server) |
| **Hindsight** (pages-over-facts; coding-agent package) | **Yes** for zero-LLM, files-as-truth, or claim-once handoffs; **no** if you want its end-to-end QA numbers, git-history ingestion, or 16+ agent installer | Zero-LLM default (Hindsight needs an LLM, local or by subscription), markdown files as the source of truth (its markdown is a one-way mirror of Postgres), one binary, claim-once handoffs, cross-project messaging, and (new on 2.4) a belief-strength `confidence` + an opt-in "dream" rewrite that borrow their model but never delete a source and keep the zero-LLM default | Its published end-to-end QA accuracy, git-history ingestion, hooks for 16+ coding agents with one shared bank per repo, a reranker on by default, image/file ingestion, user/team/company scopes inside a bank, and belief-strength that *actually moves ranking by default* (ours ships but is off and eval-gated) |
| **Mem0** (fact extractor; coding plugins) | **Yes** for coding memory without a cloud key; **no** for app end-user personalization | Zero-LLM, no cloud API key, no telemetry, editable pages, git truth, claim-once handoffs, cross-project messaging | Native plugins for seven coding agents from one runtime, failed-command-to-fix capture with categories, a subdirectory search scope, shared team plus personal memory, cloud user profiles, and its SDK/integration ecosystem |
| **Supermemory** (hosted memory API; coding plugins) | **Yes** for data ownership and an open-source self-host; **no** for a managed multimodal second brain | Git-markdown truth, an open-source self-hosted server (its server binary is not open source), claim-once handoffs, scoped project identity (its isolation is a tag), zero-LLM default | Recall injected on every prompt, connectors and multimodal ingestion, managed hosting, memoryrepo's stale-write signal |
| **OpenViking** (context database; coding plugins) | **Partly**: different weight class | Zero-LLM binary (it needs a VLM), MIT licence vs AGPL, claim-once handoffs (its `ov-kanban` has no claim guard), per-project multi-user auth | Its gateway proxy that injects recall and keeps prompt caches valid, plugins for seven agents, ACLs and user groups, L0/L1/L2 loading as a first-class read path |
| **Cognee** (graph memory; Claude Code plugin) | **Yes** for multi-harness coding continuity; **no** if you need graph queries or data connectors | Git-markdown truth, 20+ harnesses vs one plugin, handoffs/messaging, multi-user auth | Its listing in Anthropic's plugin directory, the repo code-graph indexer, a statusline, Drive/Gmail/Slack connectors, temporal search windows |
| **Zep/Graphiti** (temporal KG) | **Yes** for self-hosting coders avoiding a graph DB + LLM (Community Edition is discontinued); **no** for enterprise graph-query/world-time | One binary on SQLite, zero-LLM, files, coding-harness native, a supported self-host | True bi-temporal (world vs observed time), Cypher/BFS graph queries, custom entity/edge types, `minRating`, hosted Zep's Context MCP with per-user OIDC and a write kill switch |
| **Letta / Letta Code / MemOS** | **No** if you're building your agent *on* Letta or want Letta Code as your harness; **yes** if you only wanted your existing coding agent to remember | Memory under your existing harness (Letta argues memory belongs inside the harness and ships its own), zero-LLM, self-hosted (Letta Code defaults to Letta Cloud) | Letta's "dreaming" subagents, agent self-edited memory, and a `/doctor`-style memory audit with brief token cost. Both keep git-backed markdown, so the substrate is no longer a difference |
| **Honcho** (Plastic Labs) | **No: different problem.** It models the *human* (theory-of-mind peer representations); we remember the *project* | If you (mis)used it for coding continuity: file-first git truth, zero-LLM, single-binary self-host, per-project scoping, no mandatory LLM egress | Its user-psychology reasoning engine (Deriver/Dreamer/Dialectic), published user-recall benchmarks, managed cloud, none of which a coding-memory user needs |
| **LiquidLM** (solo-built hosted memory API, Supermemory camp) | **Weak: different job.** It's a cloud "second brain" with multimodal RAG; we're automatic, file-first coding memory | Data ownership (markdown+git, no opaque cloud), zero-LLM default, single-binary self-host, automatic hook capture, per-project team sharing, no API/subscription spend | Its multimodal ingestion (video/audio/PDF/Office), polished web app + grounded chat, managed hosting, GitHub-sync connector, and out-of-the-box rerank |
| **Engram** (self-hosted coding-agent store, issue #810) | **Yes**, same lane | Files-as-truth git-markdown wiki (vs SQLite-as-truth + git-synced compressed chunks), **automatic lifecycle-hook capture** (vs agent-driven `mem_*` MCP saves), typed claim-once handoffs + cross-project messaging, multi-user page sharing, published numbers | Engram's zero-config Go binary is equally lean; you give up its managed Cloud replication add-on. A near-tie on posture; the switch is about capture model + wiki-vs-rows, not moat depth |
| **EverOS** (file-first, LLM-leaning, issue #810) | **Yes** for zero-LLM/single-binary/team; **no** if you want its vector tier, user-track modeling, or end-to-end QA numbers | Zero-LLM default (EverOS's minimal tier still needs an LLM for the core flow), one self-contained binary (vs a Python stack), cross-harness lifecycle capture, typed handoffs/messaging | EverOS's ANN vector index and optional Milvus backend, its published end-to-end benchmarks (LoCoMo 94.42, LongMemEval-S 94.00), its first-class user `episodes/profile` track, packaged offline “memory evolution,” and v1.4.2's explicit foreground/background embedding-capacity split |
| **memU** (LLM-wiki + skill distillation, issue #810) | **Partly**: same file-first instinct, different default | Zero-LLM capture/retrieval by default (memU's distill flow is LLM-required), one binary (vs Python + sidecars patching host instruction files), MCP + hooks, git-versioned truth | memU's turnkey **skill-distillation-into-Markdown** packaging, `memu-agent detect` for unknown agents, and its hosted cloud (memu.so). Our nearest equivalent is the opt-in experience/auto-improve pass, not a one-command skill distiller |
| **Caura** (governed fleet memory, issue #810) | **No: different buyer** (org-scale multi-agent governance and live peer orchestration) | If you (mis)used it for one project: file-first git truth, zero-LLM, single-binary self-host, per-project scoping, no LLM egress | Its **fleet-governance layer** (org/team/agent visibility scopes and four trust tiers) plus the beta bus's peer discovery, presence, leases, correlated replies, and runtime wake helpers; ai-memory intentionally does not run a live agent fleet |
| **TencentDB Agent Memory** (proxy fan-in server, issue #810) | **Mostly no: different capture model** | Hook/MCP capture into a file-first wiki you own (vs a base-URL proxy intercepting traffic), zero-LLM default (its v2 hub needs an LLM), one binary vs a Node three-service stack, typed handoffs | Its zero-code proxy integration, the v2 hub's per-asset access control, and its **CodeGraph** codebase index (adjacent code-intelligence we don't do). Note: needs no Tencent DB (SQLite default); the name misleads |
| **Mem0/Zep app SDKs; Supermemory/OpenViking hosted products; LiquidLM, Honcho, Caura** | different buyer (app/user personalization, hosted context DB / RAG-as-a-service, user-modeling, org-scale fleet governance), **not migration targets** for those products; their coding plugins are covered above | — | — |

Net: for a **self-hosted, multi-harness, team, zero-LLM, data-ownership**
coding-memory use case, ai-memory does the basics and adds things none of them
fully have, so migration is justified. The gap has narrowed: hook capture and
git-backed markdown are now common, and several tools ship things we do not
(official-directory reach, git-history ingestion, recall on every prompt,
human work queues, end-to-end QA numbers). The premise does *not* hold for app/user
personalization (Mem0/Supermemory/LiquidLM), user-modeling /
theory-of-mind (Honcho), enterprise graph queries (Zep Cloud), agent-building
runtimes (Letta), or a solo dev happy with Claude's built-in or with a
single-harness plugin. Those are different buyers, and we should say so
rather than overclaim.

## Did we copy without improving? — the borrowed-ideas audit

The main finding: ai-memory did **not** cargo-cult this field. Of the ideas it
took, most are at parity-or-better; the exceptions are **scoped-but-unfinished
substrate**, each deliberately gated behind "prove it with an eval first." The
remaining risk is that opt-in ideas stay **dormant** because the retrieval
eval has not shown enough benefit to justify activating them by default; the
next evidence gap is reproducible end-to-end QA.

| Borrowed idea | From | Verdict | Detail |
|---|---|---|---|
| Local embeddings (all-MiniLM-L6-v2) | mcp-memory-service | **BETTER** | Pure-Rust candle (no ONNX/C++ per target), checksum-pinned, default-on with background backfill |
| Hook capture | mcp-memory-service | **BETTER than the source; parity with the 2026-10 field** | 20+ harnesses + typed sanitizer boundary + backpressure vs Claude-Code-centric. The field has caught up on breadth (Hindsight hooks 16+ agents; Mem0 and OpenViking ship seven plugins each), so the remaining edge is the sanitizer boundary and zero-LLM capture, not harness count |
| Versioned supersession | agentmemory | **BETTER** | A concurrent divergent write supersedes but the loser stays reachable (invariant #16); theirs flips `isLatest` |
| Retention-as-formula (decay) | agentmemory | **BETTER** | Real SQL decay math vs in-memory O(N²) Jaccard capped at 1000 rows; 2.4 adds opt-in per-tier half-life curves (identity default) |
| Per-tier retention curves (365/180/90/30) | mcp-memory-service | **SHIPPED (parity), better default** | `[decay.half_life_days]` per tier (2.4, #807); their shape, but the omitted-key default is byte-identical to the old single-λ, so no upgrade mass-evicts |
| Extractive, zero-LLM compression | agentmemory / mcp-memory-service | **BETTER** | 2.4 tier-down (#808) keeps abstract+summary+regex keep-tokens and drops prose, but is **reversible** (git + supersession chain, `restore-page`) and off by default, not a lossy in-place rewrite |
| Progressive-disclosure on-start brief (L0/L1/L2) | OpenViking / Letta-code (MemFS directory deferral) | **BETTER on the default path** | The opt-in SessionStart brief gives every pinned / `_rules/` / `_slots/` page an extractive excerpt before any page gets a body, then deepens pinned and slot pages first; four or more unpinned excerpts of one family share a heading. Zero-LLM (OpenViking generates its tiers with a VLM), file-first (frontmatter `abstract:` / `summary:`, no sidecar files), no new MCP tool, and the A2 tier-down substrate stays reversible. Retrieval reads still have no `detail` / `path_prefix` tier (the TrieHI half of R7) |
| Cold-cluster dedup (DBSCAN, adaptive-eps) | mcp-memory-service | **SHIPPED (parity), better safety** | 2.4 (#812) over the bounded cold set (no O(N²)); collapses by **supersede-not-delete** (invariant #16), conservative eps ceiling, zero-LLM, off by default |
| Access-weighted retention (access boost) | mcp-memory-service | **SHIPPED (parity+), closed a real gap** | 2.4 (#798) wired reinforcement into `read_page`, the related-walk, and `explore`: every read path, not just search; throttled/async/FTS-exempt |
| Contradiction detection (0.4–0.75 band) | mcp-memory-service | **SHIPPED (parity), advisory-only** | 2.4 (#814) surfaces likely conflicts in `memory_lint` from existing embeddings; zero-LLM, never edits/deletes/persists an edge |
| Surprisal-first + idle/cancel dream scheduling | Honcho (Dreamer) / Supermemory | **SHIPPED (parity), opt-in** | 2.4 (#816) idle-scheduled, cancels on activity, orders most-novel-first; off by default, observable `DreamReport` per run |
| Git-as-snapshot | agentmemory | **BETTER** | The wiki files *are* the truth (diffable), not a `state.json` dump |
| Viewer: version diff, session timeline/replay, health | agentmemory | **SHIPPED (3.0), read-only by design** | Page history with server-side diffs (superseded versions included, so a concurrent write's loser stays visible), a paged session timeline, and a root-only health panel with the same fix lines as `ai-memory status`. Edit/forget in the browser is declined (#482); a `clippy.toml` forbids `Wiki` writes in the web crate |
| Recall tracing / metrics | agentmemory (via the iii console's OTEL) | **SHIPPED (3.0), different shape** | No OTEL exporter: `explain=true` reports per-stream timings, `/web/search` shows a recall trace with per-stream ranks and RRF contributions, and root-only `/admin/metrics` exposes Prometheus counters and latency histograms with no content or scope in any label |
| Auth on by default | agentmemory (0.9.30) | **SHIPPED (3.0), new installs only** | `init` on a fresh data dir creates an owner-only root token and the server requires it; existing installs keep working and opt in with `ai-memory auth enable`, which rewires hooks and MCP clients without writing the token into a harness config |
| Pages-over-facts / living pages | Hindsight | **PARITY** (native) | Same architecture, file-first vs their Postgres-first; `settled_first` briefing shipped |
| Cross-session experience/abstraction pass | Letta/Hindsight/surveys | **REAL, ~parity** | ≥2-session evidence guard is real, but the pass is LLM-required and **off by default** |
| Typed relation edges | graph camp | **PARTLY SHIPPED** | Link-neighbours are now a first-class retrieval surface: `memory_read_page include_related` walks the `page_links` graph outward (bounded BFS, #775). Retrieval **RANK weighting by edge type is still not done** (edge kind stays explain-only in `memory_query`) |
| Belief-strength consolidation | Hindsight / mcp-memory-service | **SHIPPED, off by default (parity of mechanism, unproven)** | 2.4 (#815) computes a read-time, zero-LLM `confidence` over `page_evidence` (distinct-session breadth, recency, live `contradicts`, capped `[0,0.95]`), exposed inertly in `explain`/`status`; foldable into ranking authority behind a **default-0.0, R2-gated** weight. Our improvement over Hindsight's documented **entrenchment** failure: breadth-not-raw-count weighting, recency shading, a confidence cap, and **supersession always wins** regardless of evidence. Not yet default-on; the R2 run on the zero-LLM corpus measured 0.000, because only consolidation writes the evidence it reads |
| LLM merge/rewrite "dream" pass | Hindsight (Dreamer) / Supermemory ("dreaming") | **SHIPPED, opt-in/off (parity of mechanism, unproven)** | 2.4 (#816) rewrites cold clusters into one page via the gated `apply_batch` path, `dry_run` first, JSON-schema output; **never deletes a source** (superseded with a merge-note stub, `restore-page` recovers), off by default, provider-less stores keep the zero-LLM A3 path. Off until an R2 quality delta is shown |
| bi-temporal-lite | Zep/Graphiti | **BEHIND by design** | Ingestion-time only; world-time validity deferred (because dating facts needs an LLM and breaks the zero-LLM path) |

## Prior gaps and current status

This ledger records what earlier audits asked for, what shipped, and what is
still open. Open items remain recommendations until a separate decision
schedules them.

1. **R2 reproducible eval harness, built (#771/#772) and run.** It was the
   meta-blocker: belief-strength ranking, typed-edge rank weighting, and a
   published accuracy/latency/context-tokens *triple* were all gated on "prove it
   with a number." The full-set FTS→local triple was published on 2026-09-21
   ([`benchmarks/retrieval-ab-r2.md`](benchmarks/retrieval-ab-r2.md)), and the
   2026-10-12 decomposition
   ([`benchmarks/longmemeval-s-decomposition-2026-10-12.md`](benchmarks/longmemeval-s-decomposition-2026-10-12.md))
   split the pipeline number into capture cost, ranking and harness
   artifacts, which led to the 3.0 observation stream (zero-LLM FTS hit@5
   0.662 → 0.951, local 0.800 → 0.945). What is still unrun is the dormant substrate below: the
   belief-strength weight cannot move a zero-LLM corpus (every page's
   confidence is 0.0 without consolidation evidence, so the measured delta
   was 0.000 FTS), so it needs an LLM-consolidated corpus; typed-edge rank
   weighting is not built.
2. **A zero-LLM / local reranker (candle cross-encoder).** Our only reranker is
   LLM-as-judge, so the zero-LLM default path has *no* reranking, while
   basic-memory ships a local one, agentmemory ships one opt-in (off by
   default, and disabled in its published LongMemEval-S run), Hindsight turns
   one on by default, and OpenViking and Zep rerank in their pipelines. The
   decomposition narrowed the case: FTS already ranks whole raw sessions at
   0.972 hit@5, so a reranker's headroom on hit@5 is small; what is left is
   hit@1 (0.885 on whole sessions) and recall on multi-evidence questions.
   Re-scope any reranker work to those metrics, measured with the observation
   stream on.
3. **Activate the dormant substrate: belief-strength now wired, off by
   default (2.4, #815).** `page_evidence` confidence is now computed at read
   time (zero-LLM) and can be folded into `PageAuthority` behind a clamped,
   **default-0.0** `[retrieval] belief_authority_weight`, following the design in
   [`design-hindsight-borrowings.md`](design-hindsight-borrowings.md) §3, with
   the anti-entrenchment guards built in. It is exposed inertly in
   `memory_query explain` and `memory_status` today; **it does not move ranking
   until an operator sets the weight, and turning it on is gated on a positive
   R2 delta, which the zero-LLM benchmark corpus cannot produce (no
   consolidation evidence, so the measured delta was 0.000).** Still behind: **typed-edge rank
   weighting** (edge kind stays explain-only) and a `contradicts` *authority*
   cap; A5 (#814) surfaces contradictions in `memory_lint` but does not yet
   feed rank.
4. **Sharpen the cross-machine story: partly done in 3.0.** `comparison.md`'s
   "cross-machine *by construction*" meant one shared server + manual git/rsync,
   with **no automatic replication** (agentmemory ships a P2P primitive; mcp-memory-
   service has Cloudflare/Milvus). 3.0 adds the file-first answer:
   the `ai-memory-wikisync` companion syncs a server's wiki with a git
   repository both ways through the public MCP write path, with conditional
   writes (`expected_page_id`/`create_only`, #986), conflicts that need
   `--prefer`, opt-in delete propagation, and a `sync --check` CI gate.
   `comparison.md` now says replication between servers is not automatic and
   names the companion.
5. **Close the setup-friction gap vs the built-in: narrowed in 3.0.** This is
   our one structural weakness for the solo user. Much was already done (`run`
   auto-wire, `doctor`, `backfill`, `install-self-routing`), and 3.0
   adds a two-command native install for Linux and macOS:
   `install.sh` (a release asset with its own `.sha256`, verified before
   unpacking, no root) and then `ai-memory setup`, which runs `init`, installs
   a per-user systemd or launchd service, wires MCP, hooks and Agent Skills for
   each detected agent, and checks the result; `uninstall` removes the service
   it wrote. `scripts/onboarding-acceptance.sh` measured 2 commands and about
   1.2 s from the first command to a returned memory, in a clean temporary
   home with a debug build served as a local release and no model download.
   That run drives the service step through a stand-in `systemctl`/`launchctl`,
   not a real systemd or launchd. 3.1 brings native Windows to the same two
   commands: `install.ps1` (checksum-verified, no administrator rights) and a
   `setup` service step that registers a per-user logon entry running the
   server with no console window and restarting it after a failure. Still open:
   no npm package, and a "coming from Claude built-in?" importer (the
   `~/.claude/.../memory/` format is nearly identical markdown + `type`
   frontmatter, which makes it a cheap, strong on-ramp).
6. **Smaller, competitor-specific closes: five now shipped on 2.4 (opt-in,
   defaults byte-identical):** the graph-walk retrieval tool (basic-memory's
   `build_context`/`memory://`, LiquidLM's `follow_references`/`get_related`)
   shipped as `memory_read_page include_related`/`related_depth` (#775), a bounded
   BFS exposing the link-neighbour walk over `page_links` we already compute; the
   **dialectic/oracle query** (Honcho) shipped as `memory_query answer=true`
   (#782), a synthesized+cited answer over the hits, LLM-required and off by
   default (graceful `answer_unavailable` with no provider); the **reasoning-tier
   knob** (Honcho) shipped as `memory_query`/`memory_explore
   reasoning: {minimal..max}` (#783), mapping to a per-tier token budget; the
   **pin-before-search contract** (LiquidLM) shipped as `memory_query pin_first`
   + `memory_briefing.pinned` (#780); and the **"hide superseded unless asked"
   retrieval knob** shipped as `memory_query include_superseded=true` (#773) over
   our supersession chains. All five are strictly opt-in LLM/retrieval layers
   *over* the zero-LLM FTS/RRF core, never a requirement (invariant #13).
   The **memory-aging** set (A1–A5, B1–B4, C1) landed on 2.4 as well
   (per-tier decay curves, extractive tier-down, DBSCAN cold-cluster dedup,
   the contradiction band, access reinforcement on all read paths,
   belief-strength confidence, and the LLM dream pass), audited row-by-row in
   the borrowed-ideas table above; the design of record is
   [`design-memory-aging.md`](design-memory-aging.md).
   MCP tool behavior hints now ship across the 25-tool surface (title plus
   read-only/destructive/idempotent/open-world), with the runtime auth and
   scope checks remaining authoritative because annotations are untrusted
   metadata. **Still unshipped:** typed-edge **rank** weighting and a
   `contradicts` authority cap (A5 flags, it doesn't yet rank);
   a read-only graph visualization in `/web`.
   Deliberately out of scope: Honcho's theory-of-mind user-modeling engine and
   LiquidLM's opaque-cloud / LLM-mandatory / multimodal-second-brain substrate
   (all off-mission for file-first, zero-LLM coding memory; the global-scope
   preferences page already covers the useful user-prefs slice).
7. **Fix doc staleness the audit surfaced** (accuracy, not features):
   `research-basic-memory.md` now distinguishes stable v0.23.2 from the
   broader unreleased v0.24 changelog. The agentmemory items were corrected on 2026-10-12:
   `comparison.md`, `benchmarks/README.md` and `evals/README.md` no longer
   attribute the `0.967` (the grep baseline on their 15-query
   `coding-agent-life-v1` corpus) to LongMemEval-S, where they publish 95.2%
   `recall_any@5` without a reranker; and `research-agentmemory.md` /
   `issues-agentmemory.md` now carry a dated-snapshot header (54 tools, all
   visible by default; 138 REST endpoints, per their README as of 2026-10-10).
   Their 95.2% is a component measurement and our 0.82 a pipeline one, so
   neither pairing is like-for-like. The 2026-10-13 refresh corrected another
   set across `comparison.md`, `research-2026-landscape.md`, this file and
   `README.md`: Mem0 is not vector-only (semantic + BM25 + entity fusion since
   April 2026) and its latest core release is v2.2.1, not a plugin tag;
   OpenViking is a self-hostable AGPL server with local embeddings and
   editable markdown memories, and TrieHI recursion is gone; Cognee runs on
   embedded files without an LLM key; Graphiti has ~31.7K stars, not "20K+";
   Supermemory's plugins use lifecycle hooks and its self-host server binary
   is not open source; Letta's V1 server is retired and its memory is git
   markdown (the ADE references are stale); Hindsight's headline is 94.6%,
   its banks carry user/team/company scopes, and its LLM can be local;
   EverOS publishes benchmarks and an MCP server and was listed twice (as
   EverMemOS); memU's releases ship on PyPI; and the "smaller, less
   VC-amplified niche" positioning no longer holds. The 2026-10-18 pass then
   corrected Letta's retirement wording, current releases, ai-memory's own
   marketplace/install coverage, and the recommendations that had already
   shipped.
8. **Fleet-scale trust governance: a real gap vs Caura (issue #810).** Our
   multi-user story is per-project (`pages shared, batons owned`, invariant #16)
   with a root→DB-user→OIDC auth ladder and an audit log, and it is deliberately
   project-scoped. Caura adds an axis we do not have: org/team/agent visibility
   scopes plus **four cross-fleet trust tiers** governing read/write/delete across
   hundreds of agents, with per-write PII flagging. This is off-mission
   for file-first coding memory (it belongs to the org-fleet buyer), so the right
   move is to **say so, not to chase it**. It still should not be waved away as
   "we already do multi-user": we do project multi-user, and we do not do fleet
   governance.

## Gap decisions (2026-10)

Capabilities seen across the field in the 2026-10 research, each with our
decision. The table is keyed by capability, not by tool. Verdicts:
**SHIPPED (3.0)**, **ADOPT** (planned), **MEASURE-FIRST** (build only when a
number or real demand exists), **DECLINE** (conflicts with goals or
invariants), **ALREADY** (we have it and should say so).

| Capability seen in the field | Verdict | Why |
|---|---|---|
| Human workstream operations view (claude-mem v14 “My Work”) | SHIPPED (3.2) | `/web/workstreams` shows bounded active, interrupted, completed, and empty lifecycle state to the root/operator without exposing checkout paths, native session ids, or transcript content. It does not invent attention/review states from prose or become a second task system |
| Foreground query capacity isolated from embedding backfill (EverOS v1.4.2) | SHIPPED (3.2) for local inference | A real-model probe measured eight-worker query p50/p95 rising from 36/41 ms idle to 74/76 ms. Limiting local document inference to one slot while reserving one query slot reduced loaded latency to 39/51 ms. Hosted providers keep their existing concurrency; provider failures still degrade to lexical/entity/graph retrieval |
| Structured pre-rank filters (Mem0; basic-memory v0.24 unreleased) | SHIPPED (3.2), except author | `memory_query` applies bounded kind, tier, literal path-prefix and exact-array tag predicates before every page-ranking stream, including global, historical, profile and pinned results. `as_of` is the valid-time control. Author stays deferred until null root/anonymous attribution has explicit semantics |
| Per-operation LLM provider/model profiles (Graphiti v0.31; Hindsight `main`) | TELEMETRY SHIPPED (3.2); ROUTING MEASURE-FIRST | Root-only status now measures calls, outcomes, latency, estimated tokens, reported usage and health for a fixed workload set without storing content. Labels remain routing-inert. Add public profiles only after real measurements plus quality evaluation show a benefit worth the config/auth surface |
| Document extraction (basic-memory v0.24 unreleased; hosted memory APIs) | SHIPPED (3.2) as an optional companion | One explicit local text/Markdown, text-layer PDF, DOCX, PPTX, XLSX, VTT, or SRT file becomes attributed, sanitized markdown through a bounded, dry-run-first Unix importer and conditional MCP write. OCR, images, raw media, connectors, and multimodal embeddings remain out of scope |
| Retrieval recall through capture (several tools rank whole raw sessions) | SHIPPED (3.0) | The 2026-10-12 decomposition showed our zero-LLM session page indexed only 80-character turn titles; the observation stream recovered it (FTS hit@5 0.662 → 0.951, local 0.800 → 0.945, all 470). Chasing another tool's headline number is declined: those numbers measure different things |
| Local zero-LLM cross-encoder reranker | MEASURE-FIRST | Fits the zero-LLM invariant (a local model, not an LLM). Bar before building: hit@5 at least +0.03 on both FTS and local paths over the shipped ranking, p95 at most +100 ms on the reference host and measured on a low-end ARM host; default-on only if the keyless path gains. Measured headroom is small after the stream (component-mode FTS 0.972 vs pipeline 0.951) |
| Editable browser viewer (edit / forget / bulk forget) | DECLINE | The web UI is read-only by design (`frontend-api.md` §10); wiki mutations go only through `Wiki::write_page`/`apply_batch`; agents already edit and forget via MCP (`memory_write_page` with `expected_page_id`, `memory_delete_page`) and the CLI |
| Viewer read side: version diff, session timeline/replay, recall trace, health | SHIPPED (3.0) | Page history with server-side diffs, a paged session timeline, a recall trace with per-stream ranks, a root-only health panel |
| OTEL traces / metrics | SHIPPED (3.0) as explain timings + content-free Prometheus `/admin/metrics`; OTLP export MEASURE-FIRST (demand-gated) | If built: off by default, no subscriber layer when off, OTLP over http/protobuf on the existing reqwest/rustls, exporter modules filtered out of the subscriber (invariant 15), a span-field allowlist (durations, counts, UUIDs only), ratio sampling, and `cargo deny` must pass |
| Auth on by default | SHIPPED (3.0) for new installs; requiring auth on existing loopback installs DECLINED for 2.x | Flipping existing installs would break every env-less hook and MCP config, a major change under our semver. Existing installs opt in with `ai-memory auth enable` |
| One-command installer / onboarding | SHIPPED (3.0 Linux/macOS, 3.1 Windows) for three platforms (`install.sh`/`install.ps1` + `ai-memory setup`); the agent-driven install doc is folded in | No npm package |
| Many-language README | DECLINE | Translations go stale on every release, and stale docs are defects under our quality bar |
| Large speculative tool surfaces (leases, task graphs, routines, sentinels, sketches, crystals, facets; 50+ tools visible by default) | DECLINE | No evidence of real use; each tool adds prompt-surface cost; we keep 25 tools and have run leases where needed |
| Signals between agents | ALREADY | Typed cross-project messaging with exactly-once pop |
| Mesh / P2P sync | ALREADY (3.0) via `ai-memory-wikisync` | Two-way, conflict-aware git sync over the file truth using `expected_page_id` compare-and-swap |
| Durable capture inbox / offline spool | ALREADY | Hook spool, `ai-memory-relay`, `ai-memory recover`, 202/429 backpressure |
| Larger capture caps | Keep ours (2 KB / 16 KiB) | Privacy boundary (invariant 6); the decomposition showed the caps were not the cause (pages holding exactly the captured text scored 0.968) |
| PaaS deploy templates (Fly, Railway, Render, Coolify) | DECLINE for now (a demand-gated docs recipe at most) | Non-loopback already refuses to run without auth; a hosted path is not our product |
| Redis or other external store backends | DECLINE | Invariants 2 and 3 (single-writer SQLite; indexes in the same transaction) and markdown as truth |
| Harness long tail (Droid, Warp, Cline, Kilo, Goose, Qwen Code, Aider, …) | SHIPPED as first-class Droid, Cline CLI, and Qwen Code support plus MCP recipes for Warp, Kilo Code, Goose and OpenHuman; hooks only where the harness has a lifecycle API | Any MCP client reaches us over HTTP |
| Image / vision memory | DECLINE | Off-mission; the sanitizer boundary is text-only |
| Bidirectional bridge with a harness's own `MEMORY.md` | DECLINE (bidirectional); one-way import ADOPT | Two sources of truth contradict our routing rule against harness-local stores |
| Obsidian vault export | ALREADY | The wiki is already markdown in git; open `<data_dir>/wiki/<ws>/<project>` as a vault |
| Importing old transcripts | ALREADY | `backfill` replays native transcripts through `/hook`, sanitized, on by default |
| Commit ↔ session linking | MEASURE-FIRST (demand) | Managed runs already record git checkpoints |
| Project profile resource | ALREADY | `memory_briefing` plus the cross-project profile |
| Migration importers from other tools | SHIPPED for agentmemory vault/export (3.0) and supported claude-mem SQLite families (3.4), plus a `doctor` check for agentmemory leftovers; others on demand | Importers live in the `ai-memory-importer` companion, never in core; the claude-mem route maps to an existing project and uses bounded atomic writes |
| Benchmark rigor | ADOPT | Publish pipeline and component numbers separately, keep run receipts, add strict recall-all@5 beside hit@5, report models and versions per result file; claim no cross-tool ranking without a shared harness |

### Step up, borrow, ignore

From the 2026-10-18 research. Each line names the tools that ship the
capability. Items marked *verify first* depend on our own current behavior,
which should be checked before any work is scoped.

**Step up** (gaps a coding-agent user notices):

1. **Foreground embedding isolation backed by a contention benchmark**
   (EverOS v1.4.2) is now shipped for the in-process local model. The manual
   probe remains in-tree so the capacity can be rechecked on other CPU classes.
2. **One reproducible end-to-end QA number** beside hit@5, with reader,
   judge, context tokens and latency stated, and more benchmarks than
   LongMemEval (Mem0, Zep, Hindsight and EverOS publish QA numbers; Hindsight's
   AMB harness is open, so an adapter is worth considering). Most numbers
   published in the field are QA numbers.
3. **Git-history ingestion on first run**, zero-LLM and sanitized
   (Hindsight), so a new repository starts with its commit history as context.
4. **Answer harness-native memory reads from our store** (claude-mem's bridge
   for Claude Code and Codex memory folders). Careful: this touches trust
   and routing, and the gap decisions above decline a bidirectional bridge.

A client-side hook spool (agentmemory, claude-mem) is not a gap: the native
`ai-memory hook` commands and the shell hooks (`hooks/_lib.sh`) both spool an
undelivered event for a later drain.

**Borrow** (small, fits the design):

- Failed-command-to-fix capture (Mem0): the pairs coding agents most often
  need again.
- A subdirectory search filter (Mem0 `dir`, OpenViking subtree filters):
  narrows monorepo recall.
- Show the user which memories were injected: makes recall auditable in the
  session as well as in `/web`.
- A "Claude CLI" provider so consolidation can use the user's subscription
  (Cognee's plugin uses `claude -p`; Hindsight accepts subscriptions): an
  LLM path with no API key.
- A statusline indicator (Cognee): shows capture is alive.
- An opt-in local cross-encoder reranker (Hindsight, basic-memory,
  agentmemory), gated on the recall-eval bar in the table above.
- Per-step model routing (Graphiti's `LLMRuntime`) only if the shipped workload
  telemetry plus quality evaluation demonstrate cheaper or faster operation
  without a meaningful quality loss.
- A dry-run-first unified audit/repair command (MemPalace v3.11): consolidate
  the checks already spread across `doctor`, lint, reindex, health, and backup
  only if one operator workflow is demonstrably simpler.
- A read-only MCP mode per connection (OpenViking gateway, hosted Zep):
  safer shared servers.
- Opt-in recall on every prompt (Supermemory, OpenViking gateway).
  *Verify first*: our hooks inject the brief at session start only.
- Strict recall-all@5 and archived run receipts (agentmemory): keeps our
  numbers honest.
- A memory audit that includes the token cost of the session brief (Letta
  Code's `/doctor`-style check).
- A generated per-project index page (Letta's `MEMORY.md` index).
- A stale-write signal that returns the current version (Supermemory
  memoryrepo); evaluate against invariant #16, where a divergent write
  supersedes and is never rejected silently.
- A held-out question set (MemPalace's 450 held-out questions): guards
  against tuning on the test set.
- Exact models and versions per result file (EverOS).
- Past observations for a file when the agent reads it (claude-mem).

**Ignore** (off-mission or conflicts with an invariant): user profiles,
data connectors in core, graph/ontology queries, generic kanban boards, proxy capture,
mounted filesystems, harness-owned self-editing memory, multimodal memory,
hosted clouds, ANN indexes, temporal graphs, live fleet presence/wake buses,
and to-do tools. Per-asset ACLs
(TencentDB v2, OpenViking) are also out of scope but recorded as a real gap
next to Caura's trust tiers.

## Bottom line

- **The moat is real and verified:** zero-LLM default, files-as-truth, one binary,
  cross-harness capture, claim-once handoffs + messaging, and multi-user
  shared-within-project. On operability, data ownership, and cross-agent/team
  continuity, ai-memory is ahead of the overlapping tools reviewed here.
  Several of them now lead on distribution (plugin directories, per-agent
  plugins), onboarding (detect-and-wire installers, git-history ingestion)
  and published end-to-end QA numbers; the gap decisions above say which of
  those we take on.
- **The borrowed conveniences now ship (opt-in), and so does the aging set.**
  The 2.4 line landed the five competitor-specific closes above (related-walk,
  dialectic answer, reasoning tier, pin-before-search, and the show-superseded
  knob) plus the memory-aging program (per-tier decay curves, extractive
  tier-down, DBSCAN cold-cluster dedup, contradiction flagging,
  access-weighted retention, belief-strength confidence, and the LLM "dream"
  pass), each opt-in with defaults byte-identical. Our consistent improvements
  over the sources borrowed from (mcp-memory-service, agentmemory, Honcho,
  Hindsight, LiquidLM): **zero-LLM by default**, **reversible** (supersede-not-
  delete + `restore-page`, never a lossy in-place rewrite or a deleted source),
  **file-first**, and **R2/opt-in discipline** on anything that touches ranking
  or calls an LLM. The R2 harness that gates the quality work now exists and
  measures the accuracy+latency+context-tokens triple plus end-to-end
  QA-accuracy (the QA column is still illustrative sample-20 only).
- **3.0 closes several viewer, operability and migration gaps** the same way:
  read-only page history/diff, a session timeline, a recall trace and a
  root-only health panel in the web UI; explain timings and a content-free
  `/admin/metrics`; auth on by default for new installs with `auth enable`
  for existing ones; a two-command native install for Linux and macOS
  (`install.sh` + `ai-memory setup`); MCP recipes for six more clients (Warp, Cline,
  Kilo Code, Goose, Qwen Code, OpenHuman), plus first-class Droid; an importer for
  agentmemory's vault and export plus a `doctor` check for its leftovers; and
  git-based wiki sync (`ai-memory-wikisync`).
- **The real weakness was retrieval through capture**, and it was
  *self-inflicted, not copied*: the decomposition showed the zero-LLM session
  page indexed only 80-character turn titles and no assistant text, while
  ranking whole sessions scored 0.972. The 3.0 observation stream, on by
  default after a measured R2 delta, recovered nearly all of
  that (0.662 → 0.951 zero-LLM FTS and 0.800 → 0.945 local on the full set).
  Still open: no local zero-LLM reranker, local embeddings scoring slightly
  below FTS with the stream on (the session page's vector sees only its
  title-only body), belief-strength (`page_evidence`) wired but
  **default-off** and unmeasurable without an LLM-consolidated corpus, and
  typed-edge **RANK** weighting not built.
- **Migration answer:** for the self-hosted, multi-harness, team, zero-LLM,
  own-your-data coding-memory user, yes: we do the basics and add what they
  can't get elsewhere. For app personalization, enterprise graph queries, agent
  runtimes, or a solo dev content with the built-in, no, and we should keep
  saying so.
