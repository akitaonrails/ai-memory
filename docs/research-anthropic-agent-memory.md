# Anthropic's "The components of agent memory": research report

> Primary source: Anthropic, *The components of agent memory*, by Lance
> Martin, published 2026-11-06,
> <https://www.anthropic.com/research/components-of-agent-memory>. Read in
> full on 2026-11-14, including the figure data, which is in the figures' alt
> text and not in the body. For Claude Code's own memory the report uses the
> official page the article links, <https://code.claude.com/docs/en/memory>,
> fetched the same day as markdown (`/docs/en/memory.md`); its CLAUDE.md
> loading, imports, AGENTS.md, auto memory, `/memory` and troubleshooting
> sections were read. The page shows no last-updated date. The two Managed
> Agents pages in §2 were fetched the same way.
>
> Scope: §1 what the article says, §2 Claude Code's native memory per the
> official docs, §3 an audit of what this repository records about it, §4 a
> map of the article's memory hierarchy onto ai-memory, §5 open design
> questions for later review, §6 sources. Nothing here records a design
> decision.

## 1. What the article says

### Definition and scope

The article defines agent memory as knowledge Claude gains from experience
(interactions with users and assigned tasks) and keeps between sessions as
context it can read later. Three things are out of scope: weight changes,
in-context learning within one conversation, and compaction, which it
describes as Claude's summaries replacing older turns so work can continue in
a new context window.

Its framing is a human-memory analogy. The hippocampus captures experiences
quickly, and offline replay during sleep and rest consolidates a selection of
them into long-term knowledge. For agents, transcripts play the capture role,
and an offline reflection pass over them, called **dreaming**, plays the
consolidation role. It credits Letta and UC Berkeley's sleep-time compute
paper (arXiv:2504.13171) as the precedent.

### The memory hierarchy

Figure 1 draws four layers between one session and the next, bottom to top:

1. **Transcripts**: a searchable event log.
2. **Memory notes**: salient observations Claude writes during a session,
   either tagged in the session log or written to a store such as a folder of
   notes.
3. **Memory representations**: for example a wiki, meaning pages Claude
   writes and maintains about one subject, linked to other notes. The article
   links Karpathy's LLM-wiki gist here, the same pattern this project
   implements (see [`research-karpathy-llm-wiki.md`](research-karpathy-llm-wiki.md)).
4. **Injected context**: loaded into every session.

Dreaming reads transcripts and moves content up to each layer, and in both
directions between neighbouring upper layers. The rest of the article is a
set of evaluations, one per arrow in that diagram.

### Findings, by component

All evaluations use Anthropic's own harnesses, models and corpora. The article
names Claude Managed Agents with a persisted memory directory for the
CL-Bench runs and the Mt. Moon dreaming run. Figures in brackets come from the
figure alt text.

**Memory as prediction (CL-Bench 1.0 database task).** Writing a rule down is
a prediction that it will matter again. Each question ran in a fresh session
with only a persisted memory directory carried over. Model generations differ
in whether they close the learning loop (record the failure, form theories,
test them, keep a validated rule). Sonnet 4.6 left untested guesses scattered
across files. Opus 4.7 consolidated into one reference and marked checked
facts. Sonnet 5 marked graded answers as confirmed and wrote corrections. Opus
5 was the first to close the loop: entries hold a rule, the evidence for it,
and warnings about traps. Opus 5.5 and Sonnet 5.5 also close it.
[Mean reward, stateless vs memory store: Opus 5.5 0.655 → 0.838; Sonnet 5.5
0.644 → 0.833; Opus 5 0.564 → 0.828; Opus 4.7 0.383 → 0.700; Sonnet 5 0.306 →
0.610; Sonnet 4.6 0.330 → 0.364.]

**Salience (Pokémon Red, about 10,000 steps).** Sonnet 3.7 saved nearly
everything: 413 files in 225 folders, one badge, and at one point 17 steps
spent creating 6 files and deleting 9 with no game action. Fable 5 saved
9 files in two folders plus a top-level `global.md` (10 files, about 43 K
characters, against Sonnet 3.7's about 978 K) and earned four badges. The
lesson the article draws: state the environment shows every turn is redundant
to store, while strategy, plans and tested rules exist only if the agent
writes them down. Fable 5's `global.md` worked as a dashboard (current goal,
plan, its own rules for using memory). Opus 5.5 kept a small, constantly
rewritten folder of goal and next steps and finished the game.

**Steering.** Telling Claude what memory is for works better than telling it
how to use it. On CL-Bench, saying only that a memory folder exists was enough
for Opus 4.7 and Opus 5. A fixed rule, "record only failures", removed much of
their gain [change in memory gain: Opus 5 −0.234, Opus 4.7 −0.150, Sonnet 4.6
−0.020]. Steering helps older models most: prompting Sonnet 4.6 to read its
memory first and test its theories nearly doubled its score [0.364 → 0.641].
Sonnet 5 read memory before answering 9 times in 10 unprompted, Sonnet 4.6
under half the time. For frontier models Anthropic's default guidance is to
create notes when a person asks or when something is clearly worth
remembering, and to leave the rest to the model's judgment.

**Dreaming corrects notes against transcripts (Mt. Moon).** A note written in
session can be wrong from the start or drift, and a later session reads it and
trusts it. A wrong note placed a landmark on the wrong side of a cave. With the
raw note the agent fell off a one-way ledge three times, said it would stop
looping, was told by the harness to continue, and backtracked a town. A
dreaming pass checked the note against the session transcript, which acted as
ground truth, and rewrote it. [The corrected run reached Cerulean City at
action 1,935 of 3,000. Neither the raw-memory run nor the no-memory run
reached it.]

**Dreaming builds representations (wikis).** On a corpus of about 350,000
internal files (about 420M tokens) from Stainless and 25 questions, Opus 5
agents built a linked wiki over 128 dreaming sessions coordinated by a shared
log: explore files, write or correct pages, merge duplicates, check claims
against sources. Wiki-only answers rose from 13% to 48% of graded criteria.
Wiki plus search rose from 54% to 61%, against a flat 53% for search alone,
and saved about 20,000 tokens per question. [The example wiki grew from 103
pages and 766 links to 222 pages and 12,893 links. The chart's x-axis is
labelled in dreaming tokens, 2M to 128M, while the body text says sessions.]

**Where wikis help and where they don't (merger questions, Harvey LAB).** A
separate set of 24 merger questions sat beside the LAB brief tasks. On it,
more than half needing several documents and many needing judgment or math,
the wiki scored 87% against 73% for search alone on those items. On a task
where most criteria sat in one document, the wiki showed no clear gain. On
another, the wiki outgrew its source [10 documents, 115 KB → 68 pages, 255
links, 203 KB, 1.77×, some pages unlinked]. The article compares that wiki to
the map in Borges's "On Exactitude in Science", drawn as large as the empire
it showed.

**Promotion to injected context.** Dreaming can float notes to the top layer
so the harness loads them at every session start, and Claude then needs no
search and need not know there is something to search for. The article says
this must be done carefully: injected notes cost tokens, a wrong note reaches
every session, and loading a note does not guarantee Claude acts on it (it
cites the Claude Code docs' warning that longer instruction files reduce
adherence). So much of dreaming's work at this layer is keeping it small,
moving detail down and leaving links behind.

**Goal-directed dreaming (contract terms).** Several agents wrote one shared
`provisions.md` concurrently and overwrote each other: 41 of 57 graded facts
survived (72%). A reflection loop rewrote the build instructions: one page per
contract, one writer per page, terms quoted verbatim. Then 57 of 57 survived.

**Gap filling across sessions.** A session sees only its own context. A
second dreaming pass found nine links to a page that did not exist, repointed
them to the existing page for the same document, and added three pages
including an index [38 entries and 225 links → 41 and 278].

### Open questions the article leaves

- **When to write.** Writing in session captures salience at the source but
  takes attention from the task. Dreaming moves creation to the background,
  but its output is not available right away.
- **Keeping memory current.** Dreaming fixes wrong or drifted notes but costs
  compute, so checking every memory continuously may be impractical. The
  article suggests focusing dreaming on information likely to change, or
  tuning its cadence by how often memories are accessed.
- **Transcript search as the foundation**, with representations such as wikis
  layered on top. Representations need compute to keep them free of
  accumulated errors and hallucinations.

### What the article says about Claude Code specifically

The article makes two claims about Claude Code. Auto memory keeps its injected
layer small the same way, with Claude maintaining it during the session, and
loads only the first 200 lines or 25 KB of `MEMORY.md`, whose header acts as an
index of what Claude knows and where to find it. The Claude Code docs warn
that longer instruction files reduce adherence. Everything else in §2 comes
from the docs page.

## 2. Claude Code native memory: the official model

From <https://code.claude.com/docs/en/memory>:

- **Two systems, both loaded every session, both context and not enforced
  configuration.** CLAUDE.md files (written by the user; managed-policy, user,
  project and local scopes; concatenated root to cwd; subdirectory files and
  path-scoped `.claude/rules/` load on demand; `@` imports up to four hops;
  target under 200 lines per file) and auto memory (written by Claude). For
  hard enforcement the docs point to PreToolUse hooks and managed settings.
- **Auto memory** is on by default in local sessions and off by default in
  self-hosted-environment sessions (Claude Tag excepted). It can be switched
  off with the `/memory` toggle (`autoMemoryEnabled`), per project, or with
  `CLAUDE_CODE_DISABLE_AUTO_MEMORY=1`. The page names no introducing version.
- **Note types:** frontmatter `type` of `user`, `feedback`, `project` or
  `reference`. Claude skips what it can derive from the code or what CLAUDE.md
  already says, and does not save something every session. Explicit "remember
  X" requests go to auto memory. A write with frontmatter gets a `modified`
  ISO 8601 timestamp.
- **Layout:** `~/.claude/projects/<project>/memory/` holds a `MEMORY.md` index
  (one line per memory) plus one topic file per memory. `<project>` is derived
  from the git repository, so worktrees and subdirectories share one
  directory; outside git the project root is used. `autoMemoryDirectory`
  (any settings scope, absolute or `~/`) relocates it, and
  `CLAUDE_CODE_PROJECT_DIR_NAME` beside `CLAUDE_CONFIG_DIR` (v2.1.234+) gives
  every project under that config dir one shared directory.
- **Loading:** the first 200 lines or 25 KB of `MEMORY.md`, whichever comes
  first, at session start. Topic files are read on demand with ordinary file
  tools. After a write Claude Code checks the index against both limits,
  reminds Claude to shorten it near a limit, and returns an error telling it
  to rewrite the index when over, because anything past the limit is dropped
  on the next load.
- **Locality:** machine-local; not shared across machines or cloud
  environments. Transcript cleanup (`cleanupPeriodDays`) never deletes memory
  files.
- **Subagents:** the main conversation's auto memory is not loaded into
  subagents, except forks, which inherit the parent. A subagent can keep its
  own auto memory in a separate directory (subagent `memory` field).
- **Compaction:** project-root CLAUDE.md is re-read from disk and re-injected
  after `/compact`. Nested CLAUDE.md files and path-scoped rules reload on
  demand. Instructions given only in conversation can be lost.

Two related Anthropic platform features the article relies on, for context
(from <https://platform.claude.com/docs/en/managed-agents/memory> and
<https://platform.claude.com/docs/en/managed-agents/dreams>, both beta or
research preview): a Managed Agents **memory store** is a workspace-scoped set
of text documents mounted under `/mnt/memory/`, with `content_sha256`
optimistic concurrency and an immutable version per change. A **dream** reads
one store plus 1 to 100 session transcripts and writes a *new* store; the
input store is never modified, and the output is reviewed and then used or
discarded. Both serve agents built on the Managed Agents API and do not plug
into an existing coding harness, so they have no row in the
[`comparison.md`](comparison.md) camp table. Two of their mechanisms have
ai-memory counterparts. Hash-precondition writes work like `expected_page_id`.
A dream that writes a new store for review resembles the dream pass's
supersede-and-keep-sources rule and auto-improve's pending-review queue.

## 3. Audit of what this repository records

Verdicts: **confirmed** (matches the primary source), **corrected** (fixed in
this change), **unverified** (the primary source does not settle it),
**consistent** (the article does not address the claim, but its findings
agree with it).

The inventory came from searching README, `docs/` and `crates/` for auto
memory, `MEMORY.md`, native, built-in and harness-local memory, and the
CLAUDE.md / AGENTS.md loading rules. Release-log and changelog mentions were
not audited.

| Claim in the repo | Where | Verdict | Primary source |
|-|-|-|-|
| Auto memory loads "the first 200 lines" of `MEMORY.md` each session | `research-2026-landscape.md` §3, platform paragraph | **Corrected**: first 200 lines *or* 25 KB, whichever comes first; topic files on demand | Docs, "How it works"; article |
| Shipped default-on in v2.1.59 | same paragraph; §6 cites two third-party blogs | **Unverified**: the official page names no version. Kept, marked as third-party, official page added to §6 | Docs |
| Layout `~/.claude/projects/<project>/memory` | §6 sources | **Confirmed** | Docs, "Storage location" |
| Machine-local, no sync | landscape §3; `comparison.md` camp table and migration notes | **Confirmed** | Docs: machine-local, not shared across machines or cloud environments |
| Repo-scoped | same | **Confirmed**: keyed by git repository, shared across worktrees and subdirectories | Docs |
| Single-agent | same | **Confirmed with a nuance**: nothing reaches another harness, and subagents do not get the main memory either (forks do; a subagent can keep a separate one) | Docs, "How it works" |
| "No search beyond reading files" | landscape §3 | **Confirmed**: topic files are read on demand with standard file tools; the docs describe no search index | Docs |
| "a per-laptop `MEMORY.md`" | `comparison.md`, "Coming from Claude Code's built-in memory"; `README.md` comparison table | **Corrected** in both: an index file plus typed topic files, per repository, machine-local | Docs |
| "Real search" as a differentiator; "excellent zero-setup single-harness memory" | `competitive-parity.md` verdict row; landscape 2026-10-18 delta | **Confirmed** | Docs |
| Auto memory runs "detached and silent", like the automatic backfill | `design-boot-backfill.md`, two places | **Corrected** (comparison removed): auto memory writes in session and the UI shows "Saved N memories" / "Recalled N memories" | Docs, "How it works" |
| Claude Code 2.1.277+ reads `AGENTS.md` only when no `CLAUDE.md`, `.claude/CLAUDE.md` or `CLAUDE.local.md` exists in the cwd or above; a bare `@AGENTS.md` import covers older versions, the disabled built-in plugin, the `claude-md` setting and a personal `CLAUDE.local.md` | `AGENTS.md` routing block; `ai_memory_core` routing snippet | **Confirmed** | Docs, "AGENTS.md" |
| `doctor` finds the store by repository root; a custom `autoMemoryDirectory` is not discoverable from transcripts | `doctor.rs:399-403`; `transcript.rs` `native_memory_dir`; `cookbook.md` troubleshooting | **Confirmed** for repo keying and for `autoMemoryDirectory`. **Unverified**: `CLAUDE_CODE_PROJECT_DIR_NAME` also moves the store, `doctor` does not model it, and the cookbook does not mention it (open question Q7) | Docs, "Storage location" |
| Sleep-time compute is "lower relevance" and "already done by shape" | `research-2026-landscape.md` §4 | **Corrected**: the article makes it the core consolidation mechanism, and its strongest result (correcting a note against the transcript) is only partly covered by ai-memory's passes (§4) | Article |
| Bidirectional `MEMORY.md` bridge declined, one-way import adopted | `competitive-parity.md` | **Consistent**, with its point that a wrong note injected at the top layer reaches every session | Article |
| Letta's `MEMORY.md` index as an idea to borrow | `competitive-parity.md`, "Step up, borrow, ignore" | **Consistent**: the article describes the same small-index, detail-below pattern for auto memory | Article; docs |

## 4. The hierarchy mapped onto ai-memory

| Article layer or arrow | ai-memory today | Where |
|-|-|-|
| Transcripts (searchable event log) | Sanitized, bounded **observations**: prompts and compaction summaries capped at 16 KiB, tool and notification excerpts at 2 KB. Searchable through `observations_fts`, the observation evidence stream, raw fallback and `memory_read_session_observations`. `backfill` imports native transcripts. Not full transcripts, by design (invariant 6). `[decay] observation_retention_days` can prune them, irreversibly, off by default | `ARCHITECTURE.md` capture caps; `config-properties.md` `[decay]` |
| Memory notes written in session | Deliberately narrow: agents write a durable page only when the user explicitly asks (routing in `AGENTS.md` and the MCP instructions). Hooks capture automatically instead of the agent choosing. Handoffs are the one routine agent-written note: written at wrap-up, claimed once by the next session | `AGENTS.md` long-term memory block; `session-management.md` |
| Representations (wiki) | The wiki *is* the representation: markdown in git, `[[links]]`, backlinks, a cross-project link graph. Session-end consolidation turns observations into pages; auto-improve reads observations and session pages to propose page edits | `ARCHITECTURE.md`; `auto-improvement-loop.md` |
| Injected context | The SessionStart handoff and the cross-project profile digest. The project brief (pinned, `_rules/`, `_slots/`) is opt-in per repository, packed into `[briefing] max_chars` (default 4000, clamp 1500 to 20000) with L0/L1 excerpts and path pointers for what does not fit | `ARCHITECTURE.md` "Session-start project brief"; `marker-file.md`; `design-tiered-session-brief.md` |
| Dreaming: transcripts → notes (correction) | **Partial, and only forward.** A new session's observations can correct existing pages: auto-improve can patch `_rules/` and `procedures/` pages from the session it reviews, consolidation updates an invariant slot only when the observations contradict it, and the experience pass flags recent sessions that contradict a recorded decision (it reads session pages, not observations). Nothing goes back later and re-reads the observations behind an existing page. The `[dream]` pass (opt-in, off) rewrites clusters of *cold pages* using only those pages; `page_evidence` records which pages fed the merge | `auto_improve.rs` patch mode; `consolidator.rs` slot rule; `experience.rs` prompt; `dream.rs` module docs; `design-memory-aging.md` B2 |
| Dreaming: → representations | Consolidation (per session), auto-improve (per session), the experience pass (across the last N session pages, opt-in) | `experience.md`; `auto-improvement-loop.md` |
| Dreaming: → injected context (promotion) | Auto-improve can propose `_rules/` and `procedures/` pages, behind an optional executable eval gate; promotion into the brief is by path, pin or `_slots/`. `memory_lint` emits a `rule_suggestion` finding for a page that reads like a durable rule. Promotion into `AGENTS.md` itself is a design proposal, not implemented, and names the same trap as the article: a shared always-loaded budget whose adherence drops as it grows | `auto-improvement-loop.md`; `auto-improve-eval-gates.md`; `design-rules-promotion.md` |
| Gap filling (broken links) | `memory_lint` reports `broken_link`, `duplicate`, `contradiction`, `stale` findings. It reports them; it does not repair them | `crates/ai-memory-consolidate/src/lint.rs` |
| Concurrent writers losing facts | Invariant 16: a divergent concurrent write supersedes and never destroys; `expected_page_id` compare-and-write and `apply_batch_if` refuse a stale write and return the current version | `AGENTS.md` invariant 16; `ARCHITECTURE.md` `memory_write_page` |
| Wiki outgrowing its source | Nothing measures wiki size against captured volume. The forget sweep, tier-down and cold-cluster dedup bound growth by age and similarity | `design-memory-aging.md` |

The article's results agree with several existing choices. The numbers come
from Anthropic's own tasks and say nothing about how ai-memory performs.

- Pages over facts, with search underneath. The article's best results combine
  a wiki with search, and the wiki helps most on multi-document questions.
  That is the Karpathy pattern this project follows, with FTS5 and RRF
  retrieval beside the pages.
- A small injected layer that points down. The brief's excerpt-first packing,
  path pointers and default 4000-character budget follow the same rule as
  auto memory's 200-line or 25 KB index.
- Lost writes. The contract-terms failure is the case invariant 16 and
  compare-and-write exist for. ai-memory keeps the losing version reachable
  instead of relying on one writer per file. The fix's other half, quoting
  terms verbatim instead of summarizing, matches the opt-in verbatim residue
  for consolidation ([`design-verbatim-residue.md`](design-verbatim-residue.md)),
  which keeps literal tokens the prompt projection would otherwise drop.
- Ground truth kept apart from derived memory. Observations are stored
  separately from pages and stay queryable, which is the substrate the
  article's correction pass needs.

Three of the article's results point where ai-memory does not go yet.

- Correction against the log. The article's clearest dreaming result checks
  a note against the transcript. ai-memory corrects forward, when a new
  session's observations contradict a page. It has the older observations and
  the `page_evidence` link from a page to the sessions that produced it, but
  no pass goes back to them to re-verify an existing page.
- Write salience. ai-memory narrows agent-written memory to explicit asks
  and relies on automatic capture plus LLM passes for everything else. The
  article's guidance for frontier models is to write when asked *or* when
  something is clearly worth remembering, and it found that fixed salience
  rules can cut the gain on stronger models. ai-memory's rule decides where
  durable writes go and who triggers them, and has no "record only X" filter,
  so the finding does not transfer directly, although this is where the
  article and the routing differ most.
- Repair. The second dreaming pass repointed broken links and added an index;
  `memory_lint` reports the same problems and leaves the fix to someone else.

## 5. Open design questions for review

- **Q1. Transcript-grounded verification.** Should a pass (zero-LLM where it
  can be, LLM-optional otherwise) re-check high-value pages (`_rules/`,
  `gotchas/`, `procedures/`, decisions) against the observations in their
  `page_evidence` sessions, and propose a correction through the existing
  approval path? The article suggests scheduling by access frequency and
  change likelihood instead of a full sweep. Access counts already exist
  (`page_access`, bumped on every read path; `design-memory-aging.md` bucket
  C), so the cadence input is there.
- **Q2. Pruning the ground truth.** If observations become the verification
  source, should `observation_retention_days` keep the observations behind
  pages that are pinned, rules or still frequently read?
- **Q3. Write salience.** Is "durable writes only on an explicit ask" still
  right for frontier models, or should the routing allow "clearly worth
  remembering" writes into a reviewable location? This weighs against
  invariant 16, the untrusted-memory boundary and the auto-improve approval
  path, and would need evidence from the recall evals before any change.
- **Q4. Brief adherence.** The article and the docs both say loading is not
  acting. Is there a way to measure whether an injected `_rules/` page changed
  behaviour, for example through `memory_feedback` signals or the auto-improve
  reviewer, so the brief budget can be tuned on use as well as size?
- **Q5. Representation bloat.** Should `memory_lint` or the briefing report a
  project's wiki size against its captured observation volume, to catch a
  wiki that has grown larger than its source?
- **Q6. Lint repair.** Should `broken_link` findings carry a proposed repoint,
  applied through the auto-improve approval path, the way the article's second
  pass repaired them?
- **Q7. `CLAUDE_CODE_PROJECT_DIR_NAME`.** `doctor` locates the auto-memory store
  as the sibling of a transcript recorded for the repository root. When that
  variable is set beside `CLAUDE_CONFIG_DIR`, every project shares one store.
  Does `doctor` report that correctly, and should the cookbook name it next to
  `autoMemoryDirectory`?
- **Q8. Measuring the learning loop.** CL-Bench measures whether memory raises
  task reward across fresh sessions. ai-memory's published numbers are
  retrieval benchmarks (LongMemEval-S). Is a closed-loop benchmark worth adding
  to `evals/` to measure the auto-improve and experience passes?

## 6. Sources

- Anthropic, "The components of agent memory", 2026-11-06,
  <https://www.anthropic.com/research/components-of-agent-memory>, and its
  figure alt text (read 2026-11-14).
- Claude Code docs, "How Claude remembers your project",
  <https://code.claude.com/docs/en/memory> (read 2026-11-14; no date shown).
- Claude Managed Agents docs: memory stores,
  <https://platform.claude.com/docs/en/managed-agents/memory>; dreams,
  <https://platform.claude.com/docs/en/managed-agents/dreams> (read
  2026-11-14).
- Linked by the article, not read for this report: CL-Bench 1.0
  (<https://continual-learning-bench.com/news/cl-bench-1-0/>), Harvey LAB
  (<https://github.com/harveyai/harvey-labs>), sleep-time compute
  (arXiv:2504.13171), Anthropic's dreaming cookbook and the
  `managed-agents/knowledge-wiki` quickstart.
