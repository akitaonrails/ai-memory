# Use Cases

> Moved verbatim from the README front page; the scenarios and walkthroughs are unchanged.

- **"Quit Claude Code and continue the same work in Codex."** Use the optional
  managed launcher when you want native session resume plus the portable visible
  history, not only a summary handoff:

  ```bash
  cd /path/to/project
  ai-memory run claude

  # Quit Claude Code, then continue the same workstream in Codex.
  ai-memory run codex --yolo

  # Continue in Command Code, preserving its own exact native session.
  ai-memory run command-code

  # Later, omit the name to resume the newest usable managed session here.
  ai-memory run

  # Start a new Codex session in the same workstream, keeping portable history.
  ai-memory run --fresh codex

  # Kiro defaults to v2; select its incompatible v3 engine explicitly once.
  ai-memory run kiro --v3

  # List the workstreams that can be selected from this checkout.
  ai-memory workstreams

  # Fix a name you regret; the ledger and the current selection stay put.
  ai-memory rename-workstream --from typo-nmae --to refactor-db

  # Pick a managed workstream from any linked local checkout, then resume it.
  ai-memory resume

  # List open cross-agent handoffs, oldest first, with the id
  # `memory_handoff_cancel` needs to clear a stale one.
  ai-memory handoffs
  ```

- **"Pick the project instead of remembering where it lives."** Start from a
  directory containing your checkouts and choose the checkout before the
  managed harness:

  ```bash
  ai-memory show

  # Machine-readable discovery without launching anything.
  ai-memory show --json
  ```

  Each successful `ai-memory run` saves a client-local checkout link keyed by
  the configured server plus workspace/project. `show` joins those links with
  the server's public activity and page-count metadata. A fast, bounded depth-1
  scan of the current directory also finds new checkouts carrying a project
  marker (`.git`, `Cargo.toml`, `package.json`, `go.mod`, `pyproject.toml`, and
  friends), while skipping dependency and build directories. The server never
  exposes a checkout path, so two client machines can safely use different
  local paths for the same project on a remote homeserver.

  The list always leads with **`+ New project`**: type a name and ai-memory
  validates a portable directory name, stages the new checkout privately, pins
  its workspace and project in `.ai-memory.toml`, and installs the routing block
  and managed Agent Skills for the chosen agent. The final directory appears
  only after every setup step succeeds, then `show` launches from it.

  The harness menu only offers agents actually installed on the host, using the
  same `PATH` lookup `run` enforces at launch.

  `--no-scan` uses only saved links; `--workspace` filters both sources;
  `--yolo`, `--fresh`, and trailing native arguments are forwarded unchanged.
  Non-terminal use must pass `--json`; JSON mode is discovery-only and never
  launches a harness.

  The first explicit run can offer an existing session from this exact checkout
  or start a new one. Switching harnesses starts or resumes the native session
  linked to the shared workstream, so an obsolete local session cannot replace
  newer cross-harness history. After a normal quit, the next launch waits
  briefly if the previous launcher is still finalizing; handled failures release
  the workstream immediately. If a linked native transcript was deleted,
  ai-memory detects the orphan before launch and starts fresh; `--fresh` forces
  that recovery for one harness. Managed mode currently covers Claude Code,
  Codex, OpenCode, OpenCode 2 beta, Pi, Crush, Kimi Code, Command Code, Kiro CLI v2/v3, OMP,
  Grok Build CLI, and Antigravity CLI; direct harness launches remain unchanged. See
  [Managed cross-harness workstreams](docs/managed-workstreams.md).
- **"Just put me back where I was."** From any directory, with no name to
  type and no list to read:

  ```bash
  ai-memory continue
  ```

  It picks the checkout whose managed launch is most recent, revalidates the
  path and its resolved scope, then continues there exactly as bare
  `ai-memory run` would. A link whose directory moved, was replaced, now
  resolves to a different project, or has a corrupt ordering timestamp is
  reported on stderr and skipped, so a resume never quietly lands in the wrong
  project. `--workspace` narrows the search; `--yolo` and `--fresh` are
  forwarded.
- **"Let me choose which workstream to resume."** From any directory, select
  one of the recent workstreams from your valid client-local managed checkouts:

  ```bash
  ai-memory resume
  ai-memory resume --workspace work
  ```

  The picker shows the workstream name, project scope, activity, and linked
  harnesses. Use Up/Down (or `j`/`k`) to choose a workstream and Left/Right to
  cycle its launch harness. Every row starts at `auto`, preserving normal
  session discovery; the other choices are supported harnesses currently found
  in `PATH`. Enter launches the displayed combination. The checkout is
  revalidated first, while the server continues to receive fingerprints rather
  than a local path. Use `ai-memory workstreams` when you are already in a
  checkout and only want the read-only list.
- **"Quit at 4 PM, pick up at 9 AM in a different agent."** The
  classic. SessionStart hook in the next supported hook client prepends a
  typed handoff with open questions, next steps, and a session summary. Grok
  captures lifecycle events but ignores SessionStart stdout, so ask it to call
  `memory_handoff_accept` when resuming from a handoff. Zero has the same
  no-stdout behavior and also must call `memory_handoff_accept`.
- **"What did we decide about X six weeks ago?"** Use `memory_query X` from
  the agent for FTS5 fused with entity matches and linked-page expansion (plus
  vector similarity when an embedder is configured). For a quick terminal-only
  FTS5 lookup, use `ai-memory search X`; that admin command does not run the
  hybrid streams. Pages are
  LLM-consolidated, so the hit is a coherent decision page, not a raw
  chat log. Pass `explain: true` to see why each hit ranked where it
  did in project or explicit-scope retrieval. Cross-project
  `global: true` search uses its separate FTS-only ranker and reports
  that active stream without per-hit RRF details.
- **"Remember this permanently."** When something is worth keeping
  beyond auto-captured session logs - a decision, a convention, a
  gotcha - tell the agent "save a permanent note that we standardised
  on Postgres for X" or "annotate this as a project rule" and it calls
  `memory_write_page` to write a durable, git-versioned wiki page. From
  a terminal it's `ai-memory write-page --path decisions/0007-db.md
  --body $'# Standardised on Postgres\n\n...' --pinned`. `--pinned`
  exempts it from the decay sweep; the H1 on the first line of
  `--body` becomes the page title (omit `--title` — it's still
  accepted, but LLM callers trip over JSON-escaping their way through
  it, see issue #67). Unlike a handoff (single-use) or an
  auto-synthesised session page (rewritten on consolidation), a
  write-page note is yours: it shows up in `memory_query`, renders in
  `/web`, and stays until you change it.
- **"That page you found is out of date."** The agent calls
  `memory_feedback` with the page's path and a signal: `helpful` /
  `not_helpful` tune how strongly retention keeps a sweep-eligible episodic
  page (they move its salience, which scales the decay formula's time term),
  while `stale` / `wrong` floor the salience *and* make any current page
  show up as a `feedback_flagged` finding in the next `memory_lint` report.
  Feedback never deletes anything — it lowers confidence and flags for review —
  and it attaches to the version current when feedback is recorded, so a
  later rewrite clears the flag. Retrieved page text is untrusted and never
  authorizes feedback by itself.
- **"Remember this, but only until the sprint ends."** Pass
  `expires_at` to `memory_write_page` (RFC3339 or `YYYY-MM-DD` = end of
  that day, UTC) — or put `expires_at:` in a page's frontmatter by
  hand. Past the TTL the page disappears from search/recent/briefing
  (pass `include_expired: true` to `memory_query` to still see it) and
  the next forget sweep hard-deletes the file and its rows. A TTL beats
  a pin; `memory_lint` warns about pinned+expiring combos.
- **"This new project has months of history before ai-memory."**
  `cd /path/to/my-project && ai-memory bootstrap` collects
  `git log`, README, `docs/`, module headers, project rules and
  one-shot-summarises them into seed wiki pages. Future sessions
  build on top.
- **"What durable lesson did that session teach?"**
  When an LLM provider is configured, ai-memory runs a background
  auto-improvement scheduler for newly completed sessions in every project. It
  records proposed wiki edits in the pending-writes audit trail, then approves
  them immediately through the normal wiki write path by default. Scheduler ticks
  are non-overlapping: if reviewing all projects takes longer than the interval,
  the next tick is delayed until the current one finishes. Scheduling and
  approval are separate: set `[auto_improve.scheduler] enabled = false` to stop
  automatic review, or set `[auto_improve] require_approval = true` to keep both
  scheduled and manual proposals pending for human review. `ai-memory
  auto-improve --session-id <uuid>` and MCP `memory_auto_improve` remain
  available for manual catch-up or targeted reruns. When its `session_id` is
  omitted, the MCP tool selects the newest completed session without a
  persisted auto-improvement run, so repeated calls advance past short
  preflight-skipped sessions; an explicit ID reruns that session. `ai-memory
  auto-improve-report --workspace <w> --project <p>` returns a read-only
  telemetry report for recent auto-improvement outcomes without staging or
  creating proposals; add `--stage` to create one pending report page for
  audit/approval. On deployments that distinguish operators, pending learning
  proposals are isolated by qualified operator identity, so one person's
  proposal for a page does not block another's; unattributed and single-user
  deployments retain the shared pending queue. See
  [`docs/auto-improve-eval-gates.md`](docs/auto-improve-eval-gates.md) for
  example executable eval scorers.

  Existing installs do not need per-project migration. The scheduler initializes
  a per-project first-run watermark so historical sessions are not reviewed
  automatically on upgrade, then records per-session claims so failed scheduled
  reviews do not retry forever; use manual auto-improve for old sessions or
  failed scheduled sessions you want to catch up. Older configs may still contain
  an `[auto_improve] mode = ...` line; current ai-memory ignores that legacy key,
  so you can remove it when convenient.
- **"What housekeeping should I consider?"**
  `ai-memory curator` runs a no-LLM, rule-based maintenance report over cold
  episodic pages, stale slots, duplicate exact normalized titles, and dangling
  cross-project links. It is report-only unless `--stage` is passed; staging
  queues one report page for approval and still performs no maintenance actions
  itself. Shared servers can opt into `[decay] breadth_weight` to give pages
  reinforced by several identified operators a retention bonus; the default
  `0.0` leaves existing retention scores unchanged.
- **"Run one ai-memory for the whole household."** Stand the server
  up on a homelab box at `0.0.0.0:49374` with a bearer token; every
  laptop/desktop talks to it. Per-cwd routing keeps each project's
  pages cleanly separated; the `/web` UI is reachable from a
  browser anywhere on the LAN.
- **"Audit what landed before sharing with a teammate."** Browse
  the wiki at `http://<server>:49374/web` - sign in with username and
  password when human auth is on. Per-project tree view,
  rendered markdown, supersession chain visible per page.
- **"Undo one bad page edit without rolling back the whole server."**
  `ai-memory checkpoints` shows recent wiki commits, then
  `ai-memory restore-page --path notes/foo.md --from <rev>` restores that one
  markdown file and reindexes it into SQLite. Full `backup` / `restore` is
  still the answer for DB-only state such as sessions, observations, handoffs,
  users, audit rows, and embeddings.
- **"Drop an experiment, keep the rest."**
  `ai-memory purge-project --project experimental --confirm`.
  Atomic: that project's DB rows cascade away, its wiki subdir gets
  `rm -rf`'d, every sibling project is untouched by construction.
