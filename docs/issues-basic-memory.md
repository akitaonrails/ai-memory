# basic-memory - Issue & PR Pain-Point Synthesis

> Source: GitHub `basicmachines-co/basic-memory`. Captured 2026-05-21.
> Tracker is unusually high-signal: small team, deep technical replies.
> Dominant themes: sync correctness, multi-project routing, embedding install hell.

## Top recurring pain points (ranked)

### 1. Sync correctness & file-watcher reliability - the single biggest theme

- **#580**: watch service can go stale while process stays alive (no heartbeat, no liveness signal, `awatch()` blocks forever on macOS FSEvents buffer overflow). Closed with partial fix.
- **#758**: watch service ignored the `--project` constraint, N concurrent MCP processes produced N overlapping watchers and races. The fix turned up *three independent bugs*.
- **#798**: watch service silently dropped events under hidden-directory parents (e.g., projects under `~/.claude/...`) because gitignore-style globs treated *every* component starting with `.` as hidden.
- **#765**: "Stale FTS index entries persist after `reset --reindex`": fixed by detecting **zombie MCP processes holding the old SQLite inode** after `unlink`. > *"That process keeps its connection to the old `memory.db` inode (now unlinked but not freed)... newly-spawned MCP processes attach to the new file. But any tool call that gets routed to a zombie MCP process queries the **old** inode."* Now `bm reset` refuses to run while MCP processes are alive (PR #776).
- **#763**: `write_note` returns *before* semantic indexing completes. Maintainer defends this as intentional architecture but admits the CLI path loses embeddings on process exit.
- **#839** (OPEN): CLI `write-note` prints `CancelledError` traceback because `_log_task_failure` doesn't handle task cancellation on process exit. Same root cause as #763.
- **#578**: *new entities silently skip embedding generation* after a single sqlite-vec load failure. Background task errors fire-and-forget.
- **#634**: schema-validate uses stale `entity_metadata` when files edited externally.
- **#481**: `alembic/env.py` was unconditionally setting `BASIC_MEMORY_ENV="test"` at module import time, which silently disabled the watch service in production.

### 2. Multi-project / multi-workspace routing - the v0.20 trauma cluster

A wave of nearly identical bugs in April–May 2026: **#782, #783, #788, #793, #799, #800, #802, #803, #804, #805, #810, #820, #834**. All have the same shape: an MCP tool ignores or mis-resolves a project/workspace identifier. Maintainer on #783: > *"Permalinks are unique per project but project is not unique per workspace... any tool that holds only a permalink string cannot distinguish between them."* Permalinks were designed as a 2-tuple (project, path) and later had to grow a third dimension (workspace) under load.

### 3. SQLite-vec / embedding provider install hell

- **#735 / #767**: duplicates: "no such module: vec0" on Windows in worker connections. Fixed by ensuring `_ensure_sqlite_vec_loaded` is called on *every* session that touches vec0.
- **#829 / #658**: still-open variants.
- **#741 / #681**: FastEmbed cache defaulted to `/tmp/fastembed_cache`, wiped in sandboxed runtimes like Codex CLI, causing `ONNXRuntimeError: NO_SUCHFILE` on every subsequent semantic search.
- **#830** (OPEN): `docker-compose-postgres.yml` ships plain `postgres:17` instead of `pgvector/pgvector:pg17`; semantic search silently fails.
- **#831** (OPEN): `IndexError: pop from an empty deque` during async engine dispose on Postgres/asyncpg.

### 4. Parser fragility (markdown-as-source-of-truth)

- **#738**: Parser captured Obsidian callout syntax (`> [!note]`) as observation categories.
- **#721**: `edit_note` fails on notes with long text around inline wikilinks because `relation_type` exceeds `MaxLen`.
- **#528**: Cloud sync prepended duplicate frontmatter to files that already had YAML.
- **#408**: YAML frontmatter parsing fails with unquoted colons in title.
- **#256**: *"Editing notes causes them to disappear from index"*. The search index DELETE was missing a `project_id` filter, so an edit in one project nuked search rows in every project with the same permalink.

### 5. Search quality / pagination

- **#693**: `read_note` pagination params were ignored at the API endpoint.
- **#354**: `tag:tagname` syntax silently treated as literal text.
- **#686** (OPEN): user hitting MCP response size limits at 57 pages.
- **#666, #618, #603** (all OPEN): reranking, time-decay, length normalization. Maintainers admit search ranking is weak.

### 6. Backup / undo / git

- **#124** (OPEN since June 2025): git-based undo. Maintainer wrote design spec but issue remains open: > *"there hasn't been an obvious way to handle when to commit. Commit on every change? Commit every so often, how often? Push to a remote? How to handle conflicts?"*
- **#59**: use `git diff` to prevent knowledge corruption/deletion. OPEN since March 2025.

### 7. Manual capture friction - present but indirect

**Nobody filed "I'm tired of telling the agent to remember."** The friction shows up indirectly:
- **#297**: Cursor + basic-memory produce sensationalized progress logs (`BREAKTHROUGH - Live Data Packets Detected!`) that pollute search results. Maintainer closes it as **the LLM's fault, not basic-memory's**: > *"It is just the tools. Your LLM is in charge of how to use it."* That position drives the manual `write_note` workflow and moves all the noise problems onto the user.
- **#669, #730, #687** (all OPEN): three separate proposals to add a *sidecar that watches session transcripts and builds the knowledge graph automatically*. **This is the strongest signal that manual capture is friction.** The maintainer is interested but hasn't started.

## Design choices that caused the most issues

| Design choice | Cited in | Symptom |
|---|---|---|
| Background async `create_task` for embedding sync | #763, #578, #839 | Lost embeddings on CLI exit; silent skips; CancelledError tracebacks |
| Single permalink space per project (not per workspace) | #783, #802, #834 | Teams launch blocker; cross-workspace name collisions |
| sqlite-vec extension loaded per-session, not globally | #735, #767, #829, #658 | Vector ops fail on connections that didn't load it |
| Defaulting FastEmbed cache to `/tmp` | #741, #681 | Models re-downloaded every run in sandboxed environments |
| File-watcher with no liveness heartbeat | #580, #758, #798 | Silent watcher death, no recovery, missed events under hidden dirs |
| Schema-validation `MaxLen` on relation_type | #721 | Valid markdown fails to edit |
| FastMCP `AliasChoices` on `bool \| None` params | #818 | Broken JSON schema; external clients silently drop the bool |
| `alembic/env.py` setting `BASIC_MEMORY_ENV` at import | #481 | Watch service silently disabled in production |
| Inline DDL `ALTER TABLE` at runtime instead of migration | #727 | Postgres deadlocks under concurrent vector sync |
| Search index DELETE without `project_id` filter | #256 | Edits in one project erase another project's index rows |

## What the maintainers' fixes reveal

- **Defaults flipped under fire**: FastEmbed cache (#741), `bm reset` behavior with live MCP processes (#765 → PR #776 adds psutil guard), `force_full=True` removed from cloud sync (#706, then again #804).
- **Multi-project params added retroactively to nearly every tool**: PR #777, #789, #803, #807. The MCP tool surface grew a workspace dimension after launch.
- **Aliases were added to make tools "training-data-friendly"** (#766: `find_text`/`old_text`/`search` aliases) and then immediately *broke* `overwrite` (#818). Reverted in #841.
- **Docs heavily expanded after confusion**: Postgres setup (#830 still open), `bm cloud setup` (#779 was pointing users at a non-existent command).
- **Out-of-scope back-outs**: #720 (visible_project_ids filter) closed not-fix because *"would leak multi-tenant visibility concerns into a local-first single-user product"*.

## Still-unsolved open issues - and why they're hard

- **#124 git-based undo**: open 11+ months. Hard because commit cadence is ill-defined and conflicts in markdown trees are messy.
- **#834 local project_id routing in mixed cloud mode**: same root cause keeps spawning new symptoms.
- **#382 / #686 large-context handling**: pagination of search results into LLM context windows is still unsolved.
- **#740 startup time**: 4.6s for `--help` because FastMCP/onnxruntime/fastembed are imported eagerly. Multi-file lazy-import refactor not shipped.
- **#830 / #831 / #829 Postgres + sqlite-vec footguns**: install path still surprises users.
- **#669 / #687 transcript-watching sidecar**: a long-wanted feature; nobody has built it.

## Concrete "do not repeat" lessons for the Rust rewrite

1. **Do not background-task the indexing pipeline behind the tool reply.** `write_note → return → embed later` makes the tool report success too early; the next `search_notes` may miss the entity. (#763, #578, #839, #685). Either make indexing synchronous and bounded, or return a structured `index_status: pending|complete` so the caller can `--wait`.

2. **Bake the identity dimension in from day one.** `(workspace, project, permalink)` is a 3-tuple. Retrofitting it caused 12+ bugs (#782/#783/#788/#793/#799/#800/#802/#803/#804/#805/#810/#820/#834). The Rust schema should encode the full coordinate at every layer, even when you only ship single-workspace mode.

3. **Liveness > correctness assumptions for the watcher.** A long-lived file watcher *will* go stale. Build heartbeats, watchdog timers, and "did we miss any events" reconciliation passes from the start. (#580, #758, #798). Treat every notify-rs-style loop as suspect.

4. **Treat the embedding/vector backend as a fallible plugin, not a default-on assumption.** sqlite-vec, pgvector, FastEmbed, ONNX: every one of them has bitten basic-memory (#735, #767, #741, #681, #830, #831, #658, #829, #578). In Rust, isolate the embedder behind a trait, fail loudly at startup if the backend can't load (don't silently degrade like #578), and don't default cache paths to `/tmp`.

5. **Make `reset` safe.** Unlinking SQLite while a sibling process holds the inode = mystery phantom search results (#765). Acquire an exclusive advisory lock, or psutil-style live-process check before any destructive op.

6. **No inline DDL at runtime, ever.** #727 was a Postgres deadlock from runtime `ALTER TABLE`. Keep schema changes in migrations; never let "ensure table" code paths execute schema changes.

7. **For the manual-capture problem**: the absence of *explicit* complaints in the tracker is misleading. The complaints show up as (a) repeated proposals for a transcript-watching sidecar (#669, #687, #730), (b) Cursor-pollution complaints maintainers close as not-our-problem (#297), and (c) the "what should we commit" indecision in #124. **A Rust rewrite that listens to a Claude Code/Codex transcript directory and writes notes without an `@-mention` would turn the loudest implicit pain into the headline feature.**
