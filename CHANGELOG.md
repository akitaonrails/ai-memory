# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

This file holds the current major line. Earlier lines are archived per major
version in [`docs/changelog/`](docs/changelog/):
[2.x](docs/changelog/2.x.md), [1.x](docs/changelog/1.x.md) and [0.x](docs/changelog/0.x.md).

## [Unreleased]

### Fixed

- Project search links now use the project's current stored name after
  resolving a canonical or legacy key. Results, breadcrumbs, and the scope
  selector stay on that project without renaming it (#1875).
- The CJK index backfill (V87) now also repairs a page or observation stored at
  rowid `i64::MIN`. Its exclusive initial cursor skipped that one valid SQLite
  rowid; the inclusive cursor now covers both rowid extremes (#1873).
- Bounded the SQLite read pool to four in-flight reads. Bursts of concurrent
  work now wait for a connection asynchronously instead of creating hundreds
  of blocking threads and long-lived read transactions that starve the server
  and hold back WAL checkpoints (#1866).
- Refreshed existing ai-memory-owned OpenCode, OpenCode 2, OMP, Pi, and
  OpenClaw generated TypeScript integrations during `ai-memory upgrade` using
  the newly installed binary's templates, while leaving absent and user-owned
  installs untouched (#1869).
- The web UI's root operator now sees every user's sessions in a project's
  Sessions, Agents and Token usage tabs, on each agent's page and on each
  session's page, and so do the `/api/v1` session list and observations
  routes. The dashboard counted every user's sessions while these drilldowns
  listed only root's own, so on a shared server most counted sessions could
  not be opened. Other users still see only their own sessions plus shared
  ones (#1867).

- `backfill --session <id>` now finds an explicitly named native session beyond
  the newest 200-session listing, instead of reporting a successful empty import.
  Exact lookups retain the adapter's checkout and top-level-session checks;
  automatic enumeration remains bounded (#1874).
- Excluded Codex's generated approval-review transcripts and background
  suggestion prompts from transcript backfill and profile harvesting, and
  kept a hand-reworded profile page attached to its ledger topic even when a
  public page write drops the harvester metadata, preventing `-2` duplicates
  (#1863).
- Matched Claude Code's UTF-16 project-folder encoding for astral characters
  and its hashed 200-character limit, so transcript discovery and token-usage
  backfill find sessions from emoji and very long checkout paths (#1864).
- Claude Code backfill-close now finds transcripts for project paths that
  contain dots, underscores, spaces, or non-ASCII characters, so an abandoned
  session's token usage is retained when its synthetic `session-end` is
  captured (#1858).
- Kept the trailing `#` of `memory_patch_page` headings such as `## C#`
  unless a space or tab comes before it, as CommonMark does. They were
  listed and matched without it, so naming one as written failed, `## C`
  and `## C#` collided, and `add_section` wrote `## F` for `## F#` (#1859).
- Documented four keys added in 3.3.0 that the shipped `config.default.toml`
  and `docs/config-properties.md` left out: `rerank_timeout_secs`,
  `[run] exit_prompt`, `[query_answer] max_context_tokens` and
  `[auto_improve.eval] builtin` (#1861).

## [3.3.0] - 2026-10-29

### Added

- Added `[ingest] record_tool_names` (`AI_MEMORY_INGEST__RECORD_TOOL_NAMES`),
  off by default. When it is on, each tool call keeps its own name, so a
  Claude Code shell call is stored as `tool non-file: Bash` instead of
  `tool non-file`. A `Skill` call also keeps the skill it ran, and an `Agent`
  call keeps its subagent type. Session pages count calls by name and
  handoffs list them by name. Only short identifiers are kept, never a tool's
  arguments, and a call that a capture policy reduced to metadata stays
  unnamed (#1851).

- `global_scope_hits` now include their workspace, project, and global scope,
  and `memory_read_page` accepts `scope: "global"`. Supplemental global search
  results can therefore be read directly without risking a same-path lookup in
  the caller's active project (#1802).

- Added `--project-id` to managed launches and checkout-local discovery to
  recover populated legacy name collisions without moving or merging history.
  An interactive launch offers authorized project choices and remembers the
  selection for that server and checkout. Resumes reuse the binding, and
  ID-selected runs keep startup context and hook capture in the selected scope
  after checking current access, run ownership and native session identity.
  Existing name selectors and legacy runs retain their behavior (#1833).

- Added `ai-memory run --no-exit-prompt`, `[run] exit_prompt = false`, and
  `AI_MEMORY_RUN_EXIT_PROMPT=false` to skip the switch/rerun menu after a
  successful interactive session. The menu remains enabled by default (#1776).

- `ai-memory merge-project --workspace <w> --from <source> --into <target>`
  losslessly joins two project identities inside one workspace. Its default is
  an exact dry run; `--confirm` re-stamps page history, sessions, observations,
  handoffs, embeddings and managed workstreams in one writer transaction, then
  removes the source project. Same-path pages get a new target version that
  appends the source body behind an `ai-memory:merged-from` marker while both
  prior histories remain restorable. The target frontmatter wins, a one-sided
  expiry is removed, and the combined page is flagged stale for consolidation.
  `restore-page --page-id <uuid>` restores either retained history even after
  the source identity is removed. Open sessions and live managed runs refuse
  the merge (#1645).

- `audit-contamination` now reports projects in one workspace whose names
  collide after Unicode compatibility decomposition, case folding, and
  removal of non-spacing marks. Findings distinguish case from accent
  differences and include page/session counts plus last activity (#1645).

- Cross-project messages can carry an optional epistemic `nature` (`unknown`,
  `measured`, `reported`, or `inferred`) and bounded supporting `evidence`.
  The server refuses a `measured` send without non-empty evidence; existing
  messages and callers remain `unknown` by default (#1428).

- Added `memory_patch_page` for atomic, heading-anchored edits to an existing
  Markdown page without re-emitting its body or metadata. It requires the
  page's current `page_id`; section replacement additionally requires the
  bounded SHA-256 token returned by `memory_read_page`. Missing, duplicate or
  fenced-code headings, stale tokens, conflicting edits, failed admission and
  concurrent writes leave the page unchanged. The typed TypeScript client
  exposes the same contract as `pages.patch` (#1697).

- Added an offline `ai-memory-eval abstention` benchmark with versioned
  clean-room answerable, hard-negative, conflicting, partial and
  missing-channel fixtures; raw lexical/score/density/belief signals;
  FSR/FAR/coverage threshold sweeps with undefined denominators preserved;
  optional reuse of pinned LongMemEval `*_abs` cases; and optional structured
  LLM answerability through the existing QA provider path. Conflicting and
  partial cases remain separate from binary ground truth, no threshold is
  presented as calibrated, and production runtime behavior is unchanged.
  The retrieval evaluator now also parses production's structured
  `{text,citations}` answer shape instead of a stale string (#1717).

- Added `[briefing] inject_on_subagent_start` to `.ai-memory.toml`. A project
  that sets it hands each `claude-code` subagent the project brief when the
  subagent starts: the same pinned, `_rules/` and `_slots/` brief that
  `inject_on_session_start` adds at session start, kept within `max_chars` and
  within the 10,000-character limit of that agent's hook context. The hook
  reads it from a new read-only `GET /brief`, which resolves and authorizes the
  project as `/handoff` does but never reads or claims a handoff, so a subagent
  cannot take its parent session's baton. A server without the route answers
  404 and the subagent starts as before. The native hook and the
  `hooks/claude-code` shell and PowerShell scripts deliver it, and every marker
  parser treats the key as a settings boundary. It is off by default (#1715).

- Added an optional `[query_answer] max_context_tokens` admission budget for
  answer synthesis. It preserves retrieval hits, reports omitted or clipped
  evidence, prioritizes known newer corrections, prefers whole snippets and
  clips only complete trailing sentences or statements into leftover space. It
  skips the model when no meaningful evidence fits. The default
  remains disabled (#1684).

- `GET /admin/status` and `ai-memory status --json` now carry a
  `consolidation` block: the queue counts plus up to 20 `failed_sessions`
  (`session_id`, `attempts`, `failed_at_us`, scrubbed `last_error`), newest
  first. A consolidation run that gives up after its retries now has a
  machine-readable record with the session ids needed to re-run it, instead of
  only a journal line and the `/health` panel's count — which also resets with
  the process. The web health panel's own view stays counts-only, and an older
  server's status shape is unchanged (#1692).

- Added the bundled `coding-v1` retrieval-evaluation corpus for offline checks
  of decision rationale, resolved-error causes, correction evidence, and late
  session details. It reuses the existing scorer and A/B reports; it measures
  evidence retrieval, not semantic answer correctness or handoff behavior (#1686).

- `[auto_improve.eval] builtin = "structure"` runs the eval gate's structural
  checks in process, so the gate needs no interpreter, no scorer script inside
  the container and no timeout to hit. It checks an H1, an upper-case
  `TODO`/`TBD` placeholder (a hard failure), a `##` section and a list of steps
  for `procedures/`, and text beyond the heading within 2,400 characters for
  `_rules/` — never a section name or a wording, so the wiki's language does
  not matter. `eval_results` records `checker_name = "builtin:structure"`.
  `builtin` and `command` are mutually exclusive; `command` is unchanged for
  project-specific scorers (#1638).

- `ai-memory llm-test --server` probes the server's own configured provider
  instead of building one from the caller's environment, through a new
  root-only `POST /admin/llm-test`. The retry hint that `status` and the health
  panel print now names it: with the server in a container or on another host
  its keys exist only there, so the old client-side hint failed with "no API
  key" or a provider `400` while the server was healthy. The probe runs one
  fixed short prompt, reports provider, model, latency and the error class, and
  scrubs the provider's message before printing it (#1639).

- Added `[handoff] deliver_on_session_start = false` to `.ai-memory.toml`. A
  project that sets it no longer gets its handoff, or the continuity notice, at
  session start; the handoff is still created when the session ends and stays
  open for `memory_handoff_accept`. The native hook, the shell and PowerShell
  scripts and the generated TypeScript integrations send `handoff=0` only when
  the value is explicitly falsy (#1621).

- Added first-party Qoder CLI/IDE lifecycle and MCP support. Both installers
  merge into `~/.qoder/settings.json`: `install-mcp --client qoder` writes a
  sessionless stdio bridge, while `install-hooks --agent qoder` registers the
  eight documented memory-bearing events as native commands. Session start
  delivers a pending handoff through `hookSpecificOutput.additionalContext`,
  prompt/tool capture uses Qoder's Claude-shaped payloads, and a missing hook
  `cwd` falls back only to `QODER_PROJECT_DIR`. Setup detects `~/.qoder`,
  uninstall removes both entries, and agent backups include its settings and
  shared AGENTS-compatible skills/instructions (#1611).

- Profile writes now return `digest_warning` when the selected summary,
  abstract, or prose statement exceeds the 240-byte session-start digest
  limit. The write still succeeds, but its author can shorten the summary
  instead of discovering the truncation only in a later session (#1609).

- Added project routing for the named profiles of a multiplexing Hermes
  gateway. The gateway exports only the default profile's `terminal.cwd`, so
  every other profile's events used to fall back to the gateway's own working
  directory and all its bots landed in one project. A gateway event of a named
  profile now routes by an `.ai-memory.toml` placed in that profile's Hermes
  home (`~/.hermes/profiles/<name>`, or under the gateway's `HERMES_HOME`), and
  that marker's capture exclusions and server profile apply. A profile without
  such a marker keeps the old behavior. The profile name is checked against
  Hermes' profile-id pattern before it is used as a path (#1597).

- Added Hermes Agent hook consent to `ai-memory doctor`. Hermes runs a shell
  hook only when that exact command is approved in the home's
  `shell-hooks-allowlist.json`, and each profile under
  `~/.hermes/profiles/<name>` keeps its own allowlist. A profile with the
  ai-memory hooks in `config.yaml` but no approvals captured nothing, and its
  gateway only logged `not allowlisted — skipped` for each event. `doctor` now
  reads the root home and every profile, reports which ai-memory hook events
  each one wires, and warns about every profile whose events are not
  allowlisted (unless `hooks_auto_accept` is on), with the `hermes -p <name>
  --accept-hooks` command that approves them. The JSON output gains a `hermes`
  array. It never writes either file and never prints the hook commands,
  which can embed a bearer token (#1598).

- Added `AI_MEMORY_RERANK_TIMEOUT_SECS` (`rerank_timeout_secs` in
  `config.toml`) to set the `memory_query` reranker budget, which was fixed at
  20 seconds. The default stays 20 and zero is rejected at startup. A hosted
  provider behind an LLM fallback chain can spend the whole fixed budget on one
  slow 503 before the next candidate answers, and every such query then keeps
  its pre-rerank order. ([#1614](https://github.com/akitaonrails/ai-memory/issues/1614))

- `ai-memory status` now shows the client's version next to the server's: a
  `ai-memory X (client)` line under `ai-memory X (server)`, and `version` in
  the JSON `client` object. In a split install (native client and hooks, the
  server in a container or on another host) the two drift apart when only one
  side is upgraded, and nothing showed it. When they differ in major version,
  the text report adds a warning to upgrade the older side (#1644).

- Added editing for queued messages in the web inbox. Edit takes a message that
  is still waiting out of the queue, so no agent can collect it, and opens an
  inline editor. Saving puts the new text back at the message's original place
  in the queue, after the usual sanitizer, size caps and `message_send`
  admission. Discarding returns it unchanged. A held message shows as Editing,
  with a count above the list and a way back into the editor. Editing needs
  write access to the mailbox project and to the project the message came from,
  and a message an agent already collected cannot be edited. Held messages use a
  new `editing` state (migration V95) that no claim path selects (#1832).

- Added server-side response investigation to the project dashboard. Responses
  can be filtered by UTC period, exact model, harness, session, turn and tool,
  with complete pagination, filtered token totals and bounded native turn and
  tool provenance. The context lives in extra `token_calls` columns and a
  `token_call_tools` table (migration V94). A response from an older hook keeps
  its provenance unknown instead of guessed. Filters apply after project
  authorization and the session-owner check and before counting, ranking and
  pagination (#1830).

- Added version-specific knowledge evidence to the page and project views:
  readable source links, declared contradictions, expiration context and an
  on-page recall recomputation that reuses the original search filters (#1830).

- Added caller-aware installation states and next steps to feature guides and
  the authenticated `/api/v1/docs/features/status` reference. They
  separate effective settings from client facts the server cannot know, and
  administrative settings are masked for callers without admin rights (#1830).

- Replaced the static Inbox label with live, access-filtered pending counts and
  project links throughout the web navigation. Counts are grouped by workspace
  and project, hide restricted projects, never consume mail, and link to the
  pending-only mailbox of the project that has mail. Received inboxes open on
  pending messages, with older queued requests behind collected history, and the
  composer shows the current project explicitly (#1830).

- Added bundled product documentation to the web UI and the API. The guides and
  release notes are compiled into the binary and served at `/web/docs`,
  `/web/docs/changelog` and `/web/docs/{id}` (a `.md` suffix returns the
  Markdown). They include searchable feature activation guides, per-release
  changelogs, and authenticated `/api/v1/docs` JSON and Markdown references for
  agents. No network access or LLM is needed, and the pages describe shipped
  capabilities without exposing the running server's configuration or
  credentials. The source-built Docker image now copies `CHANGELOG.md` for this
  (#1829).

- Organized project pages into accessible tabs showing one section at a time: Overview,
  Agents, Token usage, Sessions and Knowledge. The selected tab is kept in the
  URL (`?tab=agents`), which keeps reload, shared links and browser history in
  sync, and the tabs support Left, Right, Home and End. Existing
  `#agents`-style section bookmarks still open the matching tab (#1829).

- Updated the browser inbox composer to default To project to the source
  project and list only harnesses with captured sessions in the destination
  workspace and project. It resets a stale recipient when a project changes,
  and it disables sending, with an explanation, for a project that has no
  registered agent (#1829).

- Redesigned the responsive web dashboard. It has persistent workspace
  navigation, searchable project cards, per-agent consumption and coverage, and
  session drilldowns that show the token totals recorded for each agent (#1828).

- Added a browser inbox at `/web/inbox`. It lists received and sent mail
  without consuming it. A caller with write access to both the source and the
  destination project can also queue a message for a registered harness of that
  project. The text goes through the configured sanitizer and admission before
  the single writer, and the browser never claims mail or starts an agent.
  Messages can now be addressed to one harness (`to_agent`, migration V93).
  `memory_message_pop` and `ai-memory message pop` take an optional `agent`,
  and only that harness can claim the message, even by id. Existing shared mail
  (`to_agent = NULL`) behaves as before. A claim from a session-aware client
  whose routing metadata arrives before its session start was persisted is no
  longer refused. A browser session opened with the legacy read-only login can
  send with an explicit write token supplied for that one request and cleared
  right after, with no browser persistence (#1828).

- Added live, deduplicated response token accounting for Claude Code and Codex.
  Updated native hooks to send measured response usage on `post-tool-use`,
  `stop`, `pre-compact` and `session-end`, keeping open-session totals current
  rather than waiting for session end. Each response is stored once by its
  native identity in a new `token_calls` ledger (V92). A partial or replayed
  delivery raises the stored counters column by column and never adds the same
  response twice. Live extraction reads a bounded 4 MiB transcript tail and
  reports at most 256 responses per event. Fresh input excludes cache reads, so
  Codex, whose reports included cached input, is normalized once during the
  upgrade. The project page has a Token usage panel listing the fifty largest
  captured responses. It flags contexts of at least 100,000 tokens, outputs of
  at least 20,000 tokens and responses above three times the project average.
  Only counters, response identities and model names are kept, and project
  authorization and session-owner filters apply before ranking or aggregation
  (#1811).

- Added Light, Dark and System theme controls to the web UI. The choice is
  stored in the browser and System follows the operating system. Tailwind's
  dark variant now uses the `dark` class instead of the media query, so a
  manual choice overrides the system setting (#1811).

- Added a memory dashboard to the read-only web home, using sanitized data:
  projects, sessions, sanitized capture events, compiled pages, open-session
  continuity signals and last activity. Project cards separate captured work
  from compiled pages, so an active hook no longer looks empty just because
  its sessions have not ended (#1803).

- Added dashboard hook-health counters for accepted, policy-dropped,
  capacity-shed and rate-limited events. They count events since the process
  started, and prompt, command, path and payload data is neither persisted nor
  rendered (#1803).

- Added agent CLI attribution to the project overview to show which harnesses
  produced a project's memory. The stats card has an "Agents" badge row with session counts per
  harness, and each Recent Activity entry shows the harness whose session last
  contributed evidence to that page version. Both are best effort: a page or
  project with no session evidence shows no badge (#1803).

- Added per-session token accounting. At `session-end` the native hook (Claude Code
  and Codex) reads the harness's own transcript and reports cumulative input,
  output, cache-write and cache-read totals. It scans Claude Code's JSONL in
  full and reads only the tail of Codex's rollout file, since both can reach
  hundreds of MB. The server stores the totals with a `MAX(existing, reported)`
  upsert, so a replayed or out-of-order delivery never lowers a total. The
  project overview page shows a "Tokens" total in the stats card and a Sessions
  table (agent, start, duration, events, tokens) with the 20 most recent
  sessions. A session whose harness never reported usage, an unsupported
  harness or an older client shows no number, never a zero (#1803).

- Added recovery of usage and closure for sessions missing a graceful
  `session-end`, which previously stayed open without tokens. This usually
  happens when a paid quota runs out mid-task and the operator switches agents. When a new session
  starts in the same scope and finds one quiet for at least 15 minutes, the
  client looks for the old session's own transcript (Claude Code and Codex),
  reports its usage and closes it with a normal synthetic `session-end`. That
  is safe if the old session turns out to be live, because a later real
  `session-end` with more work runs the full end path again. A second, more
  conservative scheduled sweep closes sessions nobody continues anywhere,
  after 48 hours without activity in any scope, so `open_session_count` stops
  growing. It attaches no usage and starts no consolidation, because the
  server cannot read a client's transcript outside a live hook request (#1803).

- Added optional `max_chars` to `memory_read_page` (default 12000,
  minimum 500, maximum 64000). It caps the returned body with the same visible
  truncation marker as `memory_read_session_observations`. The response always
  includes `truncated` and `total_chars`, which report whether the body was cut
  and its full size (#1827).

### Changed

- `memory_lint` now reports embedding-band pairs as
  `similarity_candidate` instead of `contradiction`, and no longer infers that
  a newer page supersedes an older one from timestamps alone. Explicit
  `contradicts` edges remain `contradiction` findings (#1685).

- `move-project` now refuses the **copy+purge merge** — the path taken when the
  destination workspace already holds a project of the same name — unless
  `--allow-lossy-merge` is passed as well as `--confirm`. That path keeps only
  the source's latest pages and deletes the project with its sessions,
  observations, handoffs and older page versions, which `--confirm` written for
  "move this project" did not say; the refusal answers `409` with the counts it
  would discard. A true-move (fresh destination, everything preserved) is
  unchanged, and `merge-workspace`, whose command already says merge, keeps
  working without the extra flag (#1713).

- `memory_install_self_routing(compact: true)` and `ai-memory
  install-instructions --compact` now return a block that is actually compact:
  2.1 KB instead of 5.9 KB, close to the 7.9 KB full block it was supposed to
  replace. It keeps the rules a session needs without reading anything else
  (scope selection for session-aware and static clients, managed-run resume,
  capture, the untrusted-memory trust rule, and where rules belong) and points
  at the installed `ai-memory-*` Agent Skills for the rest, which it already
  required to be installed (#1672).

- When the native session linked to a workstream is missing from its store,
  an interactive `ai-memory run <harness>` now also names the newest session
  this checkout still holds for that harness and the native selector that
  resumes it, such as `resume <id>` for Codex. The launch still starts fresh
  as before; running the same command again with that selector right after
  the harness name links that session instead (#1654).

- Managed-workstream startup packets now render completed tool calls and
  results as compact provenance stubs instead of spending the 30,000-character
  resume budget on raw command output or generated file content. Conversation,
  checkpoints, compactions, and annotations remain inline; the complete
  sanitized tool evidence stays searchable in the ledger (#1607).

- The LLM fallback chain now logs its own events. A transient candidate
  failure logs a warning with that candidate's provider, model, chain
  position, error class, HTTP status and cooldown, and an info line records
  each change of the answering candidate. Before, the chain logged nothing,
  and callers such as the reranker named only the first candidate, so a log
  could blame the primary for a timeout another candidate caused. Response
  bodies are never logged (#1628).

- Reduced the default output of `memory_read_session_observations`: `limit`
  from 50 to 20 and `body_max_chars` from 4000 to 1000. Callers can still raise
  them to the unchanged ceilings of 200 and 16384. The smaller response reduces
  token costs for routine "what did this session do" lookups. Updated the
  retrieval skill with an `order="desc"` recipe using a narrow `kinds` filter
  and small `limit` and `body_max_chars` values to avoid reading the whole
  session (#1827).

### Fixed

- Retried an OpenCode version probe briefly when the executable is temporarily
  busy, covering both an updater replacing the binary and a just-written
  launcher under parallel load instead of failing the managed launch.

- Fixed live response capture after transcript gaps. Bounded batches now resume
  from durable enqueue checkpoints, including across hook processes and
  transcript read windows (#1830).

- Fixed response search, signal filtering and recent ordering so they query all
  authorized calls before pagination. Response pages are independent, and
  session detail pages refresh usage live (#1830).

- Updated scoped session reads to treat substantive work after an old end as a
  resumed session, matching the activity feed while preserving persisted
  lifecycle watermarks. Completed-only listings exclude those resumed sessions (#1830).

- Moved token model label sanitization before truncation to prevent the size
  limit from exposing a prefix of private text (#1830).

- Updated release selection on the changelog page to load notes immediately
  and keep the URL, outline and Markdown reference in sync. The form still submits
  without JavaScript (#1830).

- Added a read-only recovery snapshot at agent startup when the previous
  substantive session in the project ended without a `SessionEnd` (quota
  exhaustion, a killed process, a machine restart). The snapshot is built on
  demand from at most 64 sanitized observations and capped at 6,000
  characters, so no rolling copy of the transcript exists. It is scoped to the
  project and owner, leaves out the receiving session, keeps the
  untrusted-memory boundary, and never closes or claims the source, which may
  be a legitimate parallel agent. It skips work a handoff already covers and
  never mixes another live session's transcript into a delivered handoff.
  Normal single-use handoffs and managed workstream packets are unchanged
  (#1803).

- Excluded Claude Code's synthetic cross-session messages and plugin-submitted
  prompts from cross-project profile harvesting, and removed a leading
  side-agent quote while retaining the user's words after it. Generated text
  can no longer become a standing preference attributed to the user (#1855).

- Serialized mutating wikisync runs per destination across their full
  read/plan/write lifecycle, so overlapping exports no longer race on the
  shared state temporary file or apply plans from the same baseline (#1847).

- Bounded the wiki append-versus-commit concurrency regression so filesystem
  scheduling cannot turn its background writers into an unbounded fsync
  workload on macOS, while every round still contends with a commit (#1850).

- Fixed false command timeouts in the macOS menu app when CLI output or
  diagnostics exceeded a pipe buffer (#1845).

- Fixed the macOS menu app's LaunchAgent installation and restart checks for
  app and home-directory paths containing XML characters such as `&` (#1844).

- OpenCode 1.x `task` subagents no longer claim the pending handoff or leave
  one behind. OpenCode runs each subagent in a child session, which the
  generated plugin took for a top-level one: the subagent's first model call
  claimed the newest open handoff in the checkout, and the end the plugin
  posts for it when OpenCode exits carried no subagent marker, so the user's
  next session got the subagent's handoff instead of the top-level session's.
  The plugin now records a child session's parent from OpenCode's session
  events, skips the claim for it and marks its end on every close path, a
  subagent the task tool resumes by `task_id` included (#1842).

- Preserved `title:` and `body:` filters inside parenthesized CJK searches,
  so grouped queries no longer include matches from other columns (#1846).

- Made `auth login --timeout-secs` stop device-authorization polling requests
  and waits at the deadline for OpenAI OAuth, GitHub Copilot and OIDC device
  login. Late polling results no longer reach credential storage; provider
  intervals and `slow_down` backoff are preserved. OpenAI's subsequent exchange
  of an authorized code keeps its own HTTP request timeout (#1848).

## [3.2.10] - 2026-10-29

### Changed

- Documented the settings that `config.default.toml` had left out, and added
  `docs/config-properties.md`, a reference table of every key `config.toml`
  accepts with its default, environment variable and purpose. The template now
  describes the `llm_*` keys (including `llm_compat_strict` and
  `[[llm_fallbacks]]`), the embedding endpoint and prefix keys,
  `tcp_keepalive_secs`, `base_path`, `release_base_url`, `cors_allow_origins`,
  `consolidate_on_session_end`, `backfill_on_start`, `run_autowire`,
  `claude_true_yolo`, the `contradiction_band_*` keys, and the `[run]`,
  `[hook_spool]`, `[handoff]`, `[maintenance]`, `[dream]`, `[retrieval]`,
  `[search.fts]`, `[profile]` and `[[admission_webhooks]]` sections. It also
  lists the remaining keys of `[decay]` (including `[decay.half_life_days]`),
  `[consolidation]`, `[auto_improve]`, `[auto_scope]` and `[auth]`, and the
  `[auto_improve.scheduler.experience_entropy_filter]` table. The new entries
  are all commented examples, and keys with a built-in default show that
  default. `AGENTS.md` now requires a new config key to land in both files.

### Fixed

- Made the per-session auto-improve reviewer, the cross-session experience
  pass, the dream merge and the lint contradiction pass write their prose in
  the dominant natural language of their input, as the consolidation prompts
  already did. Their English-only system prompts gave no language rule, so
  sessions held in Portuguese could still produce English `gotchas/`,
  `decisions/` and `concepts/` pages and English lint findings. Code,
  identifiers, paths, patch anchors, evidence quotes, JSON keys and enum
  values stay verbatim (#1839).
- Pinned `zip` 8.6 to upstream's official security-backport commit for
  GHSA-m665-v7pm-7grj. Candle still requires the yanked 8.x line, so the
  immutable backport keeps fresh builds resolvable while adding the header
  consistency hardening shipped in `zip` 9.0.5.
- Updated `libz-sys` to 1.1.31 so its build probe no longer records the build
  environment, including possible secrets, in Cargo target artifacts
  (RUSTSEC-2026-0418).
- Imported every Claude transcript left behind by `/clear`, oldest first,
  before finalizing the managed run on the newest session. Pre-clear turns now
  retain their native-session provenance instead of disappearing from the
  workstream ledger (#1797).
- Kept purge-session tombstones out of automatic hollow-project cleanup, so
  an old otherwise-empty project cannot lose replay protection immediately
  after its session is purged (GHSA-pphw-hm98-3wh4).
- Made Crush transcript cursors revision-aware within their one-second native
  timestamp bucket. Same-second inserts and edits are now imported exactly
  once, while captured recovery endpoints still exclude later revisions
  (#1813).
- Made TypeScript SDK capability initialization client-owned with independently
  cancellable waits, so one caller cannot abort another caller's shared probe
  and a later caller observes its own cancellation promptly (#1815).
- Initialized MCP sessions in the document and agentmemory importer routes,
  retaining validated session IDs and sending `notifications/initialized`, so
  conditional imports work with both stateless and `--http-stateful` servers
  (#1836).
- Explicit `ai-memory run --fresh claude` launches now expose the previously
  linked Claude session through `AI_MEMORY_PREVIOUS_SESSION_HISTORY`, including
  when the two sessions live under different Claude config homes (#1789).
- Replaced the misleading hook-refresh-only warning after a linked Codex run
  fails to acknowledge context with a `/new`-specific ownership warning. The
  wrapper does not adopt the unlinked thread because Codex supplies no trusted
  parent relationship (#1788).

- Sorted recent page lists by timestamp rather than RFC3339 text, so updates
  with different fractional-second precision appear in chronological order
  in the legacy recent API, project activity, folder previews and namespace
  listings. Kept title-sort and equal-timestamp path tie-breaks (#1247).

- Fixed `restore` replacing live data when a backup without `config.toml`
  contains API keys that the configured pepper cannot verify. It now refuses
  before the swap, preserving the live wiki, database and config, and names
  the original configuration as the remedy (#1495).

- Preserved explicit DOCX/PPTX line breaks and DOCX tabs in document imports
  when XML uses paired tags, including formatted PowerPoint breaks in slides
  and speaker notes. DOCX carriage returns now also keep adjacent text runs
  on separate lines instead of joining their words. Paragraph tab-stop
  definitions no longer insert spurious tabs into the extracted text (#1786).

- Kept a wiki-sync server page when its repository file was recreated after
  the sync planned a deletion. `sync --propagate-deletes --apply` now rechecks
  local absence before sending the delete, preserves the page's baseline and
  reports the changed file for the next sync, including with `--prefer repo`
  (#1787).
- Transcript imports (`backfill`, the `run` ledger, `recover`) now drop Codex
  memory-writing agent runs by the same signals the live hook uses: the
  checkout cwd, an event's recorded cwd, or a Codex user prompt that opens
  with `## Memory Writing Agent`. A custom `CODEX_HOME` whose last path
  component is not `.codex` previously reimported those sessions (#1790).
- Admission webhook HTTP requests now follow redirects only within the
  configured `scheme://host:port` origin (up to 10 hops) and fail on
  cross-origin redirects without forwarding page payloads or applying foreign
  response mutations, surfacing the redirect refusal cause in logs and
  `reject`-policy errors (#1794).
- `ai-memory project access` and `ai-memory project grants` now refuse empty
  or whitespace-only `--workspace`, `--project`, and (for access) `--mode`
  values before sending any request to the server. These commands take
  required names and never call `resolve_scope`, so the #1774 choke point
  did not cover them (#1791).
- `ai-memory user grant` and `ai-memory user revoke` now refuse empty or
  whitespace-only `--user`, `--workspace`, `--project`, and (for grant)
  `--level` values before sending any request to the server. These commands
  take required names and never call `resolve_scope`, so the #1774 choke
  point did not cover them (#1792).
- Stopped the lifecycle relay from treating an oversized HTTP response as a
  transport failure and immediately resending its batch. A response beyond the
  64 KiB acknowledgement cap now retains its HTTP status, stops the flush, and
  keeps unacknowledged events pending, including on HTTP 429 backpressure (#1799).
- Decoded WebVTT cue character references before document-import sanitization,
  preserving literal escaped tags and applying the existing text-size limit
  to the decoded text. SRT extraction is unchanged (#1800).
- Preserved native OIDC hook-spool entries when the POSIX or PowerShell script
  bundle drained the shared queue. Scripts now leave those entries for native
  credential resolution instead of posting them anonymously and retiring them
  on a 2xx or 401 response; static, anonymous and legacy entries still drain
  normally (#1801).
- Refused backup restores with a missing or invalid gzip trailer before
  replacing live data, even when all tar entries were readable (#1805).
- Kept separate pre-migration archives when installations share a backup
  destination and start in the same second, instead of overwriting an
  existing installation's snapshot (#1806).
- Restored the previous wiki file when `restore-page` could not commit its
  SQLite update, including a rejected case-colliding historical path. Restores
  now share the page writer's lock through conformance, file replacement and
  indexing so a concurrent write to that same path cannot leave disk and
  SQLite disagreeing (#1807).
- Kept concurrent page edits current during opt-in cold-cluster dedup and dream
  merges. Each write batch now skips if a selected source version changed,
  preserving the newer body, tier and pin along with unchanged peers (#1808).
- Dropping the final store writer handle now finishes when its command queue
  is full. Accepted writes still drain before the writer thread exits, including
  when callers have cancelled their waits (#1809).

- Scheduled empty-project cleanup now preserves projects that have sent or
  received agent messages, including claimed/cancelled history, so it cannot
  silently delete their inbox/outbox through a project cascade (#1810).

- Fixed `server add` rejecting valid inline tables in `servers.toml`, including
  token rotations and URL changes. Updates now accept the same table forms as
  reads and removal, preserving registered roots and other profiles (#1812).
- Kept OpenCode V1 failed tool results in the managed workstream ledger,
  including their visible error text and error status, instead of importing
  only the call and silently dropping the failure reason (#1814).
- Initialized the TypeScript SDK's MCP session before the first `callTool`,
  fixing HTTP 422 responses with `--http-stateful`. Raw calls still skip
  schema checks, and later wrapped calls reuse the session (#1816).
- Preserved literal reasoning tags and their contents inside structured JSON
  strings when parsing OpenAI-compatible tolerant replies and Anthropic text
  replies. Reasoning blocks outside the JSON are still removed, including
  when a strict OpenAI-compatible request falls back to tolerant parsing (#1817).
- Disabled HTTP redirect following on the native hook client (`build_client`)
  so 3xx responses from SSO gateways, captive portals, or proxies fail closed
  instead of following to a 200 OK login page, which dropped spooled hook events
  and injected login HTML into agent handoff context (#1818).
- Supported singular units (`day`, `week`, `month`) and weeks in
  `parse_since_to_epoch` for git commit filtering during consolidation
  bootstrapping, trimmed surrounding whitespace, rejected negative intervals,
  and used checked arithmetic to prevent integer overflow panics (#1820).
- Kept OpenCode 2 failed tool results in the managed workstream ledger,
  including their visible error text and error status, instead of importing
  only the call and recording a status-error loss (#1823).
- Imported OpenCode 2 compaction session messages into the managed workstream
  ledger (`summary`, or `error.message` on a failed compaction) instead of
  dropping them with no event and no loss (#1824).
- `memory_query(global=false)` now overrides a project's
  `[recall] default_global` marker and uses the resolved project's normal
  search. Previously the explicit opt-out still returned cross-project
  `global_hits` and disabled project-only query options (#1835).
- Honored `base_path` from `config.toml` when mounting the HTTP server, so
  thin clients and the web UI use the same prefix. Explicit flag and environment
  overrides, including an empty value to serve at the root, kept their precedence (#1837).
- Refused empty and whitespace-only `--workspace` and `--project` flags at the
  shared CLI scope-resolution choke point (`resolve_scope`,
  `resolve_scope_noting_marker_project`, `resolve_scope_with_marker`, and
  `resolve_workspace`) so an unset shell variable (`--project "$UNSET"`) or
  blank value fails closed across all scoped CLI subcommands instead of
  silently falling back to the current directory's scope or forwarding a
  whitespace name (#1774).
- OMP subagent sessions (the task tool, eval `agent()`, `/tan` clones) no
  longer claim the pending handoff or leave one behind. OMP runs them inside
  the parent's process and loads the extension into each, so a subagent's
  first prompt claimed the newest open handoff in the checkout and its end left
  a sub-task handoff for the next session. The extension now reads OMP's
  `ctx.agent`, skips the claim for a subagent and posts its events with
  `agent_id`, which the router treats like an OpenCode child session.
  `drop_subagent_captures` now covers OMP subagents too, and an advisor's tool
  calls stay with the session they advise (#1779).
- The openai-compat strict path now treats an engine 501 structured-output
  rejection as a capability gap and falls back to the tolerant parser. It used
  to return the 501 as a transient failure, so the bootstrap chunk retry loop
  repeated a call that failed the same way each time.

## [3.2.9] - 2026-10-27

### Fixed

- Fixed Antigravity CLI hooks on Windows failing when the install or profile
  path contains spaces — bare hook commands are now caret-escaped (#1767).
- `ai-memory read-page` now refuses explicitly empty scope flags (`--workspace`
  and `--project`), empty `--path`, and an empty search query before resolving
  scope or sending a request to the server (#1769).

- `ai-memory finalize-session` now refuses explicitly empty scope flags
  (`--workspace` and `--project`) before resolving scope or querying the server
  (#1771).

- Admission webhook URLs are validated when the chain is built: only
  `http://` and `https://` URLs are accepted. A webhook configured with any
  other scheme (`file:`, `ftp:`, a custom scheme), a bare hostname, or an
  empty URL could never be called, yet previously loaded fine and then
  errored on every covered write at request time — under a `reject` failure
  policy that aborted each write. The server now refuses to start and names
  the offending webhook instead (#1766).

## [3.2.8] - 2026-10-27

### Fixed

- `ai-memory restore-page` now refuses empty or whitespace-only `--path` and
  `--from` values before resolving scope, authentication, or sending a request
  to the server (#1764).
- `ai-memory message` subcommands (`send`, `pop`, and `cancel`) now refuse
  explicitly empty scope flags and targets before reading stdin or sending
  any request to the server (#1761).
- `ai-memory rename-project` now refuses an explicitly empty `--to` target
  name before sending any request to the server (#1759).
- `ai-memory write-page` now refuses explicitly empty scope flags
  (`--workspace` and `--project`) as well as an empty `--path` before
  reading stdin or sending any request. Passing an empty `--project ""`
  previously bypassed the flag check while falling back to resolving the
  project from the current working directory (#1757).

## [3.2.7] - 2026-10-26

### Fixed

- Fixed `ai-memory doctor` native memory inspection omitting Antigravity CLI
  and using a Claude-only synthetic probe (#1745).
- Fixed native hook backlogs repeatedly missing their 250 ms mid-session
  delivery window on loaded macOS hosts. Threshold catch-up now launches the
  long-budget detached drainer, and POSIX fallback hooks use macOS's real
  Application Support data directory for spool and auth state (#1744).

## [3.2.6] - 2026-10-26

### Fixed

- Fixed `ai-memory-wikisync` scope-adoption warnings containing long runs of
  spaces where source formatting had leaked into the displayed sentence.
- Fixed opt-in cold-page compaction replacing a newer user edit with the stale
  episodic body and metadata it had selected earlier. The sweep now writes the
  whole compaction batch only when every selected page version is still latest;
  one changed page leaves the entire batch untouched for the next sweep (#1721).
- Automatic backfill no longer flashes a console window for each Git probe on
  Windows. Git children inherit `CREATE_NO_WINDOW`, and a run resolves each
  harness's session directory only once (#1727).
- The OpenCode version probe now waits up to 10 seconds instead of 2, allowing
  cold-start executable scanning and loaded hosts without treating a working
  installation as hung (#1718).
- Fixed agentmemory imports resuming over destination body edits that retained
  the original attribution footer. A resume now requires the complete body to
  match a planned version before using its page id for the next conditional
  write; identical imports still resume without duplicate versions (#1726).
- Refused restore archives whose `db/memory.sqlite` snapshot is absent,
  empty, or has no applied ai-memory migration history before replacing any
  live data. Opening those sources previously initialized a new empty store
  and let `restore --force` discard the previous database and wiki (#923).
- Fixed `restore-agents` skipping files whose contents matched the archive
  but whose Unix permissions differed, leaving executable plugins without
  their execute bits. Permission-only changes now appear in the preview and
  require `--apply --force`, with the usual backup copy (#962).
- Fixed a saturated hook server blocking delivery to other servers sharing
  the native spool. HTTP 429 now pauses only that endpoint for the current
  drain pass, preserving unacknowledged events and their retry budget across
  batched, fallback and retry delivery paths (#1283).
- Fixed `ai-memory-wikisync sync` overwriting a concurrent server edit when
  importing a repository edit. Edited files now use one full page read for
  classification and the write precondition; a second read after a 304 could
  previously accept the newer version without checking for a conflict (#986).
- Restored affirmative profile preferences about tools, browsers and MCP.
  The capability-ban filter also discarded statements such as "Always use
  tools to verify facts"; it now requires negative or disabling wording.
  Capability bans still need explicit cross-project scope (#1678).
- Fixed `restore-page` accepting an explicitly empty `--workspace` or
  `--project` and restoring a page in the current checkout's scope instead.
  Empty and whitespace-only values are now refused before any request; omit
  the flags to intentionally use the current scope, extending the guard
  already used by purge commands (#1675).
- `ai-memory-wikisync` records the server, workspace and project its export
  state came from and refuses to reuse a destination for another scope before
  writing anything. Reusing one made every page of the previous project read
  as "deleted on the server", so `sync --propagate-deletes --apply` removed
  those files and emptied the state. `--adopt-scope` is the explicit way to
  take a destination over, and a baseline written before the field existed is
  adopted on its first run with a warning (#1708).

## [3.2.5] - 2026-10-25

### Fixed

- `ai-memory backup` and `ai-memory backup-agents` now carry a size that
  would round to `1024.00` of the current unit into the next one. A backup
  of 1 MiB - 5 bytes was printed as `1024.00 KiB` (#1705).
- The OKF migration dialog now carries an archive size that would round to
  `1024.0` of the current unit into the next one. An archive of 1 MB - 51
  bytes was printed as `1024.0 KB` (#1706).

## [3.2.4] - 2026-10-25

### Security

- A restricted project now stays closed when its access-mode or grant query
  fails. The common authorization resolver previously degraded these store
  failures to an open project, so an unrelated authenticated user could gain
  access during a database fault. Unknown projects and forward-unknown access
  modes retain their compatibility behavior, and normal root, creator and
  grant access is unchanged.
- Human-auth endpoints now log unexpected store and task failures server-side
  and return a stable `authentication unavailable` response. Login and
  recovery previously returned raw SQLite or task error text before the caller
  authenticated; password-change conflicts also no longer expose store errors.

### Fixed

- `ai-memory move-session` now refuses explicitly empty scope flags
  (`--from-project`, `--from-workspace`, `--to-workspace`, and `--to`) before
  sending any request. Passing an empty string like `--from-project ""` previously
  bypassed the flag check while falling back to deriving the project from the
  current working directory, which triggered an unintended batch move of all
  sessions from the current repository (#1698).

- `ai-memory delete-page` now refuses explicitly empty scope flags (`--workspace`
  and `--project`) as well as an empty `--path` before sending any request.
  Passing an empty `--project ""` previously bypassed the flag check while falling
  back to deriving the project from the current working directory, which risked
  deleting the page from the local repository (#1700).

- `ai-memory rename-workstream` now refuses explicitly empty scope flags
  (`--workspace` and `--project`) as well as empty workstream names (`--from`
  and `--to`) before inspecting the repository or sending any request.
  Passing an empty `--project ""` previously bypassed the flag check while
  falling back to resolving the project from the current working directory (#1702).

- Fixed `ai-memory setup` treating another service's HTTP 200 response as a
  healthy ai-memory server. Setup now checks the bounded `/healthz` JSON,
  reports an occupied address with port/configuration remedies, and skips
  MCP/admin probes when the health response is unrecognized (#1647).

- Preserved repository edits saved while `ai-memory-wikisync export --apply`
  fetched server pages. Export now rechecks each file before replacing it,
  as two-way sync already did, and refuses a changed file even with `--force`
  without advancing its export state (#986).

- Codex's own memory-writing agent no longer appears as captured sessions.
  Codex runs a background "memory writing agent" from `$CODEX_HOME/memories`
  (the default `~/.codex/memories`) to consolidate its memory; the hooks
  captured each run as a normal session, so one project of 78 sessions and
  ~2,600 observations existed for work nobody did, and it returned with the
  next run after a purge. Those events are now accepted and stored nowhere,
  recognized by the directory (any `…/.codex/memories`) or by the agent's
  `## Memory Writing Agent` prompt header, and transcript imports
  (`backfill`, the `run` ledger, `recover`) skip them for the same reason
  (#1674).

- `memory_query(answer=true)` now validates model-generated citation paths
  against the snippets supplied in that request. Unknown paths and paths
  shared by distinct page versions withhold the answer, while retrieval hits
  remain available with an `answer_unavailable` note. Repeated citations are
  deduplicated in model order; the default query path makes no added calls
  (#1682).
- `ai-memory write-page` and `restore-page` now print the last eight
  characters of the page id instead of the first eight. Page ids are UUIDv7,
  so the first eight are a timestamp prefix that every page written within
  about a minute shares, and the printed `page_id` was the same for all of
  them. The web UI already shortens ids this way (#1693).

## [3.2.3] - 2026-10-24

### Changed

- `ai-memory hook` and `ai-memory hook-drain` now run on a single-threaded
  tokio runtime. Both are spawned once per agent event and answer one request,
  and the multi-threaded default started a worker thread per CPU before the
  command did any work: on a 32-core machine every hook call paid to spawn and
  join 32 threads for one short HTTP request. Every other subcommand keeps the
  multi-threaded runtime (#1669).

- `purge-project --confirm` without `--project`/`--project-id` now prints the
  project it resolved from the current directory and the equivalent command
  that names it, before the irreversible request. Naming the target will be
  required in 4.0: from the wrong directory the derived name is the one project
  nobody meant to delete, and a flag that spells it out is the only way to tell
  the two cases apart (#1642).

### Fixed

- A capture whose `.ai-memory.toml` names a promoted project's old name (its
  v2 basename, now the project's `legacy_name`) no longer creates a second,
  empty project under that name. The read path already resolved the old name
  to the promoted project, so the twin made every later lookup of that name
  fail as ambiguous until it was purged — and the next capture from the same
  checkout created it again. The capture path now reuses the project the read
  path resolves, and refuses an already-ambiguous name instead of adding a
  third row (#1657).
- The cross-project profile no longer promotes an agent task brief into a
  default for every project. Two more filters now apply: a prompt that wraps a
  transcript or asks for a verbatim reply is the signature of a program
  driving the agent through its SDK, and none of it is harvested; and a
  sentence that switches off one of the agent's own capabilities ("Do not use
  any tools.", "Never call browser_close") only becomes a preference when it
  states cross-project scope. Two scripted one-shot prompts had reached
  `min_projects` and shipped "do not use any tools" as a `tools` default with
  confidence 0.95, which made agents ask for facts instead of looking them up
  in the wiki they were already in (#1648).
- `purge-project --project ""`, and an empty `--project-id`/`--workspace`/
  `--from`/`--to-workspace` on `move-project`, `purge-session` and
  `rename-project`, are now refused before anything is sent. An empty flag
  passed the "was the target named" check and then resolved as if it had been
  omitted, so a script that passed an unset variable reached the project its
  current directory resolved to — a real purge from an explicit-looking
  invocation (#1642).
- `ai-memory workstream-search` now labels a historical tool call or tool
  result as completed evidence, as the managed-workstream docs describe. Its
  text output used the event's role, which every transcript adapter sets to
  `assistant` on a tool call, so a past command read like an assistant action
  still to run (#1629).
- `install-hooks --agent grok` run from the home directory no longer stops
  with "Grok project configuration ~/.grok/config.toml may override
  GROK_HOME". With the default `GROK_HOME` that file is Grok's own user
  config, which Grok reads only as the user layer (`grok inspect` reports no
  project layer there), so it is no longer mistaken for a project overlay. A
  real overlay is still refused, including `~/.grok/config.toml` when
  `GROK_HOME` points elsewhere (#1636).
- The OpenCode, OpenCode 2, OpenClaw, Pi and OMP integrations installed with
  `--capture-mode allowlist` no longer send the handoff (or OpenCode's recap)
  request from a repository without a `.ai-memory.toml` marker. They already
  dropped its capture, but the server answers that request by resolving the
  directory into a project, so opening one of these agents in an unmarked
  directory left an empty project named after it. The native hook already
  returned before the request; these integrations call the server directly
  (#1641).
- An auto-improve full-page proposal is now rejected when its body has two
  identical headings or more than one H1, the two checks the patch path
  already applies to every body it materializes. A full-page proposal is
  auto-approved whenever approval is not required, and the page it wrote was
  then permanently un-patchable: the patch path refuses on the target's own
  duplicate headings before it materializes any edit, so no later proposal
  could repair it (#1635).
- `memory_feedback` now answers a path with no current page in the resolved
  project with `invalid_params` (-32602) rather than `internal_error`
  (-32603). An absent caller-supplied target is the request being wrong, not
  the server failing, and -32603 tells a client to retry a path that can
  never succeed. This was the last tool reporting an absent target as a
  server fault; genuine store failures are still `internal_error`, and the
  message is unchanged (#1634).
- A Qwen Code `UserPromptSubmit` body is now capped and scrubbed before the
  native hook spools it. The per-event cap covered `prompt`, `message` and
  `text`, but Qwen's prompt arrives in `submitted_prompt` — the field the
  harness-specific extraction reads — so a Qwen prompt reached the local spool
  and the server at full length with no client-side redaction pass. The field
  is now capped alongside its siblings; nothing falls back to the generic
  `prompt`, so submission provenance is unchanged (#1633).
- The forget sweep now keeps an already-compacted page terminal when
  `compact_cold_episodic` is off — the default everywhere. The guard that made
  a page carrying the V65 `compacted_at` marker "neither re-compacted nor
  evicted" sat inside the compaction arm only, so with compaction off such a
  page was tombstoned and then hard-deleted. The marker is not A2's alone: the
  A3 cold-cluster dedup merge-note stub and the B2 dream pass both stamp
  `compacted: true` under their own flags, so the durable residue a merge chose
  to keep could be destroyed without anyone enabling compaction. A
  non-compacted cold page is still evicted exactly as before (#1632).
- A project whose every page has passed its TTL is read as new again when
  the profile digest sizes itself. The newness probe was the one `pages` read
  in the profile module without the expiry guard its three siblings apply, so
  such a project still counted as having memory: it got the smaller
  established-project digest and lost the note offering
  `ai-memory profile apply`, with no live page left to show for it (#1631).
- The first start after the 3.2.0 upgrade no longer slows down as the
  one-time CJK index backfill (V87) progresses. Each batch filtered the source
  table from its first row again, so the pass probed every row it had already
  indexed once more per batch: quadratic in the row count, and hours of
  startup on a store with 1.6 million observations while `serve` answered
  nothing. Each batch now resumes after the last rowid it indexed, so every
  row is examined once (#1652).
- Restoring a page from a checkpoint (`/admin/restore-page`) no longer
  rewrites the restored file's `generated.at` to the current time when the
  checkpoint is already OKF-conformant. The restore now keeps the
  checkpoint's own timestamp when conformance leaves the rest of its
  frontmatter unchanged and the file on disk holds other content, so a
  checkpoint emitted by ai-memory restores byte for byte whatever the clock.
  When the file on disk already holds the same content, its timestamp is kept
  so file and row stay aligned; checkpoints that need conformance still go
  through the existing inheritance and stamping rules (#1671).
- The native Windows Docker wrapper now recognizes `upgrade` after valid
  `--config` or `--data-dir` global options and pulls the image instead of
  forwarding `upgrade` into the helper container. Unknown and malformed
  options still reach the CLI parser (#1667).
- `ai-memory run codex` no longer starts a new session when Codex has
  compressed the linked rollout. Codex keeps a rollout it has not written for
  a week as `rollout-*.jsonl.zst`, which the launcher did not recognize, so it
  reported the session as missing and repointed the workstream to a fresh one.
  The compressed copy is now read for the resume, the pre-launch baseline and
  the transcript import, and the plain file wins when both exist. `backfill`
  and the session listings see compressed Codex sessions too (#1653).
- Updated tokio to 1.54.1 in the server, relay, importer and wikisync
  lockfiles. 1.52.3 and the 1.53.x releases before 1.53.3 fall inside
  RUSTSEC-2026-0386, an unsound advisory for `AsyncFd` constructors that
  nothing here calls, and the `cargo-deny` gate failed on every pull request
  from the moment the advisory was assigned (#1658).
- A write that names an ambiguous project now reports the workspace it was
  resolved in. The refusal read "project 'api' is ambiguous in workspace 'the
  resolved workspace'" on the write path (`ai-memory run`, explicit MCP
  writes, profile writes), while the read path already named the workspace
  (#1657).
- `ai-memory user list` and `ai-memory api-key list` now size their columns
  by terminal display width rather than bytes, as `list-projects` and the
  grants tables already do. An accented username, display name or key label
  no longer widens its column with phantom padding, and a double-width CJK
  name no longer shifts the cells after it (#1625).

- `install.sh` now runs its smoke test of the downloaded binary from beside
  the install directory (`~/.local/lib`), not from the temporary work
  directory. On a host whose `/tmp` or `TMPDIR` is mounted `noexec`, nothing
  can be executed there, so the install stopped with "the downloaded binary
  does not run on this machine" for a release that runs from where it is
  installed. A binary that really does not run is still refused, naming the
  directory it was tried from, and its staged copy is removed (#1623).
- The document importer no longer drops a VTT or SRT cue that follows two
  blank lines (or any even number of them). The subtitle reader split cues on
  a single blank line, so the extra one was left at the top of the next block,
  and a block that started with an empty line was skipped whole. A WebVTT file
  whose header was followed by two blank lines lost its first cue, or was
  rejected as containing no extractable text when that was its only cue.
  WebVTT allows any number of blank lines between blocks; the reader now skips
  them before reading a block.
- The example auto-improve eval scorers in `docs/examples/auto-improve-eval/`
  no longer reject pages written in a language other than English. They
  required `## Purpose`/`## Overview` and `## Steps`/`## Checklist` headings in
  procedures and English imperatives in rules, and read the Portuguese and
  Spanish word "todo" as a TODO placeholder, so a correct procedure in
  Portuguese or Japanese failed. They now check structure only: an H1, an
  upper-case TODO/TBD, a `##` section and a list of steps, text beyond the
  heading and its length counted in characters. `score_proposal.sh` no longer
  embeds Python: it is the same scorer in POSIX `sh` and `awk`, so it runs in
  the official Docker image, which has no `python3`. The docs no longer call
  the Python scorer dependency-free and say how to point the eval command at a
  script inside the container (#1640).
- Kimi Code gets the compiled project brief on the first prompt of a session
  only, also in a checkout outside `$HOME` whose only `[briefing]` opt-in is
  the operator-home marker. The handoff request takes the home marker as its
  fallback for any cwd, but the once-per-session gate looked for a marker with
  a walk that stops at the checkout root outside `$HOME`. The gate stayed
  shut, no brief marker was written, and every prompt asked for the brief
  again. The gate now reads the same marker as the request (#1665).
- The startup warning about unusable proxy variables now also fires when the
  user name or password in `HTTPS_PROXY`, `HTTP_PROXY` or `ALL_PROXY` (or a
  lower-case spelling) holds an unencoded `#`, `/` or `?`. The URI grammar
  ends the host at that character, so the client sent provider and login
  requests to a host made of the start of the credentials, such as
  `alice:18080` for `http://alice:18080#k@proxy.example:3128`, and the failure
  looked like a connection error to a host nobody configured. The warning
  names the variable and the character, not the value, and the network
  behaviour is unchanged (#1637).

## [3.2.2] - 2026-10-22

### Fixed

- Fallback-provider circuits now honor bounded retry hints instead of retrying
  a quota-exhausted candidate every 30 seconds. HTTP `Retry-After` takes
  precedence; Gemini's `google.rpc.RetryInfo.retryDelay` is read from the full
  scrubbed error body before its diagnostic text is cut to 1 KiB. Hints are
  clamped to 30 seconds through one hour, remain scoped to one chain entry, and
  never enlarge the logged provider body (#1619).
- Fixed the PowerShell Docker wrapper leaving native managed commands without
  the root bearer stored in a named volume. Native `install-hooks` now refreshes
  a host copy under the user's local app-data directory once, and `run`,
  `show`, `continue`, `resume`,
  `workstreams`, and `rename-workstream` reuse it without starting a container;
  explicit token and bind-mounted data-dir settings still take precedence
  (#1603).
- Fixed Antigravity print-mode managed runs recording no user prompt because
  `agy --prompt` does not append to shared `history.jsonl`. When that history
  yields no prompt, export now reads only explicit `USER_INPUT` records from
  the conversation's bounded, containment-checked brain transcript (#1605).
- Kept Claude Code's combined SessionStart context below its fixed 10,000
  character inline limit. Profile and handoff sections are independently
  bounded, must-see handoff/continuity/inbox notices take priority, and an
  omitted managed ledger leaves a `workstream-search` pointer instead of
  pushing the entire response into an unread file (#1606).

- Corrected the startup error for a gap in SQLite migration history. A
  skipped earlier migration is now reported as missing from the history,
  instead of claiming the store is newer than the binary. The store still
  refuses to open until its migration history can be reviewed (#1612).

- Fixed the POSIX Docker wrapper routing host commands into the helper
  container when `--config` or `--data-dir` preceded the command. Managed
  launches, MCP bridges and lifecycle hooks now retain their native host
  route and complete argument list with either global-option syntax (#1602).

- Fixed native transcript imports dropping a whole session when one JSONL
  record exceeded the 1 MiB per-record cap, as a Claude Code screenshot in a
  tool result does: `ai-memory backfill` imported nothing from the session,
  and an `ai-memory run` recorded its transcript import as failed. The record
  is now skipped as an extraction loss, its tail read in bounded chunks that
  still count toward the 64 MiB source cap, and the rest of the session is
  imported; a record still being written is left for the next read, not
  skipped half-way. The transcript baseline steps over it the same way, so a
  degraded run's recovery cursor and its exact replay agree. An annotation
  marks the gap, and shows in a managed run's ledger as an import note, so the
  capture policy fails closed: a tool result without a call id (Kimi, Grok)
  later in the same turn is treated as an orphan, since the skipped record may
  have been its call under an ignored path. `backfill` and `recover` print how
  many records they skipped on stderr, `backfill` also under `--quiet`
  (#1613).

- The `user grants` and `project grants` tables now measure terminal display
  cells, so accented and double-width CJK workspace, project and user names
  keep the `USERNAME` and `LEVEL` columns aligned.
  ([#1564](https://github.com/akitaonrails/ai-memory/pull/1564))
- The LLM dream pass no longer writes a cross-scope wikilink that does not
  resolve. Every other consolidation write runs its proposed bodies through the
  cross-scope link guard, but the dream pass never did, so a `[[project:path]]`
  carried in from a cluster member's body — or invented by the model — reached
  the merged survivor and minted a dangling cross-project edge that the lint
  pass reported on every later run and nothing could clear.
  ([#1566](https://github.com/akitaonrails/ai-memory/pull/1566))
- The desktop installer no longer hangs on "Starting the setup page" when
  `ai-memory setup --web` logs heavily. It read the child's stdout for the
  whole run and its stderr only afterwards, so once that unread pipe filled
  (64 KiB) the child blocked on its next log line and never printed the page's
  address, leaving the window on the status line with no error. Both streams
  are now read at the same time.
  ([#1567](https://github.com/akitaonrails/ai-memory/pull/1567))
- The importer no longer carries a UTF-8 BOM into an imported text or Markdown
  document. `document --file notes.md` on a file saved with a BOM — what
  Notepad, VS Code's `utf8bom` encoding and Excel's "CSV UTF-8" export all
  write — stored a body beginning `\u{feff}# Overview`, so the first heading
  stopped being a heading. Nothing downstream removed it, since U+FEFF is
  neither whitespace nor a control character, and the transcript extractor in
  the same file already stripped it.
  ([#1568](https://github.com/akitaonrails/ai-memory/pull/1568))
- A workbook that contains a chartsheet now imports its worksheets instead of
  failing outright. OOXML lists chartsheets and dialogsheets as `<sheet>`
  elements alongside worksheets and only the relationship type tells them
  apart, so a chart tab created by Excel's "Move Chart -> New sheet" had no
  worksheet relationship and aborted the whole extraction; a workbook of only
  chartsheets still reports that it contains no worksheets.
  ([#1569](https://github.com/akitaonrails/ai-memory/pull/1569))
- Skip an oversized session-header record instead of failing the read when the size cap's boundary cuts a multi-byte character, so one unreadable transcript no longer aborts discovery or lookup for the other sessions beside it ([#1570](https://github.com/akitaonrails/ai-memory/pull/1570))
- `/api/v1/search` no longer tells the browser it may reuse a search response
  for 30 seconds. The handler takes an owner filter that reaches the hybrid
  ranker, so both the ranking and each hit's observation snippet depend on who
  asked, and a credential switch at the same URL inside that window could be
  answered from the browser cache without the filtering path running. The
  endpoint now sends `private, no-store`, as every other owner-scoped route in
  the same module already does.
  ([#1571](https://github.com/akitaonrails/ai-memory/pull/1571))
- `ai-memory recover` now recovers a GitHub Copilot CLI session. The recovery
  journal records `copilot` for a degraded or finish-failed `ai-memory run
  copilot`, but the reader had no entry for that name, so recovery reported
  `journal names unsupported harness 'copilot'` and retained the entry on every
  rerun, leaving the transcript unrecovered. The same gap keyed the quarantined
  hook events with `copilot` instead of `copilot-cli` and sent an exportable
  Copilot transcript down the spool-only path.
  ([#1572](https://github.com/akitaonrails/ai-memory/pull/1572))
- `ai-memory status` now reports an unreadable recovery journal while the
  server is down. The offline recovery hint read the journal error, used it to
  decide whether to print anything, then printed neither branch's copy of it:
  a truncated `recovery-journal.json` left the human line saying `repair
  unrecorded runs (0 journaled)` and the `--json` line saying
  `{"journal_pending": 0, "spool_pending": 0}`, a machine-readable all-clear
  for a journal that cannot be parsed.
  ([#1573](https://github.com/akitaonrails/ai-memory/pull/1573))
- `purge-project` now keeps `--force` and `--compact` in the re-run command its
  refusal prints. The dry-run preview above it is computed with the operator's
  own flags, but the printed command dropped them, so pasting it after a
  `--force` run came back as a 409 `managed run lease is active` — the exact
  condition `--force` overrides — and a `--compact` run quietly reverted to a
  logical delete ([#1574](https://github.com/akitaonrails/ai-memory/pull/1574))
- `move-project` now keeps `--force` and `--on-conflict` in the re-run command
  its refusal prints. Both flags are honoured in the request the command
  builds, but the printed command dropped them, so pasting it after an
  `--on-conflict overwrite` run fell back to the `block` default and came back
  as a 409 listing the conflicting paths instead of merging them, while a
  dropped `--force` put the active-project guard back in place
  ([#1575](https://github.com/akitaonrails/ai-memory/pull/1575))
- The compaction recap now reaches Claude Code, Codex and Qwen Code while an
  external producer owns capture. A `reload context` prompt is answered
  through the `UserPromptSubmit` context envelope, but that delivery path was
  missing from the set exempt from `AI_MEMORY_CAPTURE_OWNER` suppression, so
  the hook returned an empty response before it could fetch the recap. Every
  enqueue and drain on the path stays gated on the external owner, so the
  exemption delivers context without capturing anything.
  ([#1576](https://github.com/akitaonrails/ai-memory/pull/1576))
- Fixed the SessionEnd consolidation queue retrying a provider failure that a
  later attempt cannot clear. The queue's terminality check hand-enumerated
  only the deterministic structured-response shapes, so a rejected or expired
  credential, an unconfigured provider, a schema failure and a
  malformed-request `4xx` were all retried: one expired API key burned the
  full five-attempt cap — five complete consolidation prompts over 7.5
  minutes — for every ended session, logging "queued for retry" four times.
  The check now defers to `LlmError::is_transient`, the predicate the
  in-process consolidation and bootstrap retry loops already use, so only a
  `429`, a `5xx` or a transport timeout/connect failure keeps the bounded
  backoff and every deterministic failure settles the job terminal with the
  heuristic session page left in place
  ([#1581](https://github.com/akitaonrails/ai-memory/pull/1581))
- Fixed the document importer's subtitle reader dropping WebVTT cues whose
  identifiers begin with `NOTE` and rejecting CR-only WebVTT files as empty.
  It now distinguishes comment tokens from cue identifiers and normalizes
  CR, LF, and CRLF before splitting VTT/SRT cues. (#1601)

- Fixed the profile digest cutting an entry's line at 240 bytes with no
  sign of the cut: a `summary`, `abstract` or first prose line over the limit
  was clipped mid-word, so the agent read part of a rule as the whole rule.
  The line now ends in `…` inside the same 240 bytes, and
  `docs/cross-project-profile.md` states the limit next to `summary`.
  (#1608)
- `install-mcp --client freebuff --apply --auth-token` now stores the key in
  `<data_dir>/auth-token`, where the hooks keep theirs, and writes the FreeBuff
  entry with `--data-dir` and no token, as it already did for the Claude Code
  (`--session-aware`) and Muse bridge entries. FreeBuff's entry is always the
  session-less `mcp-bridge` and already carried that `--data-dir`, but the key
  was written into `~/.agents/mcp.json` as `env.AI_MEMORY_AUTH_TOKEN`, outside
  the data dir's file permissions and outliving a rotation. The key still stays
  in the entry whenever the bridge would not reach it: a configured bearer that
  would win, a different key already stored for the hooks, hooks that sign in
  with OIDC, a failed store, a different endpoint, the Docker helper, or a
  snippet that is only printed.
  ([#1593](https://github.com/akitaonrails/ai-memory/pull/1593))
- `install-mcp --client codex` now prints the same table name `--apply` writes.
  The printed snippet hand-interpolated `[mcp_servers.<name>]`, so `--name
  ai.memory` printed a header a TOML reader resolves as three nested tables
  rather than one server, and `--name "my memory"` printed an invalid header
  that breaks the whole `config.toml` it is pasted into — while `--apply`
  quoted the key correctly all along. The printed table is now built with the
  apply path's own `toml_edit` writer; output for an ordinary name is
  unchanged.
  ([#1594](https://github.com/akitaonrails/ai-memory/pull/1594))
- Every command except `hook` and `mcp-bridge` now warns, naming the variable
  but not its value, when `HTTPS_PROXY`, `HTTP_PROXY` or `ALL_PROXY` (or a
  lower-case spelling) is set to something the HTTP client cannot use: bytes
  that are not UTF-8, such as a Latin-1 password, a URI with an unencoded
  space or non-ASCII character, or an unsupported scheme. The client skips
  such a value and connects directly, so provider and login requests used to
  time out on a network that blocks direct egress, or bypass a required proxy
  where it does not, with nothing pointing at the proxy setting.
  ([#1565](https://github.com/akitaonrails/ai-memory/pull/1565))
- `init`, `auth` and `auth enable` print paths on Windows without the `\?# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

This file holds the current major line. Earlier lines are archived per major
version in [`docs/changelog/`](docs/changelog/):
[2.x](docs/changelog/2.x.md), [1.x](docs/changelog/1.x.md) and [0.x](docs/changelog/0.x.md).

  verbatim prefix of the canonical data dir, in their output and their errors,
  as the hook and service commands already did, and so does the
  `ai-memory starting` line every command logs. A path copied from those
  messages now works in `cmd` and in scripts that look for it.
  ([#1565](https://github.com/akitaonrails/ai-memory/pull/1565))

- Corrected the Docker rollback instructions to retag the prior digest and recreate the service without pulling the newer tag. ([#1585](https://github.com/akitaonrails/ai-memory/pull/1585))

- Corrected Docker backup instructions to use an online server snapshot or stop the container before copying its data. ([#1586](https://github.com/akitaonrails/ai-memory/pull/1586))

- Corrected the multi-user migration command to pass the pepper length with `--bytes`. ([#1587](https://github.com/akitaonrails/ai-memory/pull/1587))

- Corrected the API smoke test to use the running server’s configured bearer instead of a newly generated token. ([#1588](https://github.com/akitaonrails/ai-memory/pull/1588))
- Fixed restoring a page from a checkpoint leaving a pre-OKF file on disk.
  `restore-page` wrote the checkpointed bytes verbatim while the three
  sibling write paths conform theirs, and the row it indexes is conformed at
  the store's write choke point either way — so only the file was left
  behind. Restoring the pre-OKF checkpoint every migrated install carries
  then broke `export-okf` for the whole project with "run the server once to
  migrate", which the one-shot wiki migration never does again, and made
  every `serve` boot re-archive the data dir once the migration archive was
  deleted. The restore now conforms the file like the siblings do;
  conformance is idempotent modulo `generated.at`, which it inherits from the
  file on disk, so an already-conformant page still restores byte-exact.
  ([#1595](https://github.com/akitaonrails/ai-memory/pull/1595))
- Fixed `memory_query` skipping its raw-observation fallback (`raw_hits`)
  whenever an embedder was configured, which the default local embeddings
  are. The vector stream has no similarity floor, so an unrelated page
  kept `hits` non-empty, and an observation from a session with no page
  yet (still open, or ended without SessionEnd) could not be found, while
  the same store with `embedding_provider = "none"` returned it. The
  fallback now runs unless a page matched the query's own terms through
  page FTS, the entity stream or the observation stream; page ranking is
  unchanged (#1600).

- An empty structured answer from a Responses-API model is now reported as
  empty content rather than a JSON parse error. The `opencode`, `openai-oauth`
  and `codex` paths defaulted a missing `output_text` to an empty string and
  handed it to the parser, so the operator saw `serde: EOF while parsing a
  value at line 1 column 0`, naming neither the model nor the cause.
  ([#1529](https://github.com/akitaonrails/ai-memory/pull/1529))
- Detected `core.hooksPath` when a git config set it on the same physical
  line as the `[core]` section header, so `ai-memory-wikisync install-hook`
  no longer writes a `.git/hooks/post-merge` that git would never run.
  ([#1530](https://github.com/akitaonrails/ai-memory/pull/1530))
- Counted a file's final line in `ai-memory-wikisync` diff summaries when it
  does not end in a newline, so the `bytes/lines` figures no longer contradict
  the `first difference at line N` printed beside them.
  ([#1531](https://github.com/akitaonrails/ai-memory/pull/1531))
- `ai-memory run` now refuses an `--env`/`--env-file` entry whose key carries
  whitespace. `--env-file` lines are trimmed as a whole, not per half, so the
  common dotenv spelling `KEY = value` produced a key with a trailing space —
  an entry no shell or harness can read, so the override silently did nothing.
  ([#1532](https://github.com/akitaonrails/ai-memory/pull/1532))
- Discovered a repository page whose `--include` family names a bare top-level
  page (`README.md`), which `ai-memory-wikisync sync` previously left out of the
  repository side entirely, so a newly created one was never imported.
  ([#1533](https://github.com/akitaonrails/ai-memory/pull/1533))
- A Copilot `/responses` answer stopped by `max_output_tokens` is now reported
  as a truncated response naming the model and its output-token count, instead
  of an unexpected response shape reading "no error detail" — `error` is null
  on an incomplete answer, so nothing explained the stop and the class implied
  an unparseable reply rather than a `max_tokens` to raise.
  ([#1534](https://github.com/akitaonrails/ai-memory/pull/1534))
- Omitted TTL-expired pages from the page count in `memory_briefing` and
  `memory_explore`. Every page list in a briefing already hid them, and the
  project cards have since 3.1.x, so a project with an expired page reported
  more latest pages than any list in the same response could show. The
  all-versions total is unchanged, as is `ai-memory status`.
  ([#1535](https://github.com/akitaonrails/ai-memory/pull/1535))

- `memory_read_page` now reads a blank `path` or `query` as omitted, as its
  sibling arguments already do. An empty or whitespace-only `query` searched
  for an empty FTS expression and reported the inevitable miss as an internal
  error (-32603), which a looping client reads as "retry"; an empty `path`
  failed as "invalid path: empty path" and never tried the `query` passed
  beside it. Both now reach the instructive "exactly one of `path` or `query`"
  refusal, and a blank `path` beside a real `query` runs the search.
  ([#1536](https://github.com/akitaonrails/ai-memory/pull/1536))
- Reported a wiki page renamed away with an external tool, which arrives as a
  rename event rather than a removal, so the next auto-commit drops it from the
  tree instead of leaving a checkpoint naming a page that no longer exists.
  ([#1537](https://github.com/akitaonrails/ai-memory/pull/1537))
- Omitted TTL-expired pages from the 7- and 30-day activity counters in
  `memory_briefing`. A page written with a short TTL made a project look
  active while the brief's own recent-page list was empty, which is the
  signal an agent uses to decide how much context to pull. Session and
  observation counters have no TTL and are unchanged.
  ([#1539](https://github.com/akitaonrails/ai-memory/pull/1539))

- Kept literal `<mark>` tags from a page's own text out of search-result
  markup, including balanced pairs that previously looked identical to SQLite
  highlights. FTS now uses internal delimiters that sanitized page content
  cannot forge, and only presentation boundaries turn them into HTML.
  ([#1540](https://github.com/akitaonrails/ai-memory/pull/1540))
- `install.ps1` now refuses a plain-http GitHub release base. The scheme check
  sat inside a condition whose arms already implied https, so it could never
  fire, and an `http://github.com/...` base matched neither arm and skipped it
  entirely — while the archive and its checksum are fetched from that same
  origin, so the checksum proved nothing against an on-path rewrite.
  ([#1541](https://github.com/akitaonrails/ai-memory/pull/1541))
- An unspaced-CJK search carrying `AND`, `NOT` or `NEAR` no longer returns the
  pages the operator excluded. The auxiliary CJK stream discarded every boolean
  operator and `OR`-joined the CJK runs it found, and because that stream is
  unioned into the primary one, `東京 NOT 大阪` fused the 大阪-only pages back in
  and ranked them as if they had been asked for. The stream now declines such a
  query, so unspaced-CJK recall narrows to the pre-feature answer for it instead
  of inverting; `OR` queries are unaffected.
  ([#1542](https://github.com/akitaonrails/ai-memory/pull/1542))
- An `openai-oauth` answer the Codex backend stopped at its output-token
  budget is now a terminal truncation instead of a retryable `502`. The
  streaming parser read `response.incomplete` as a provider fault, so the
  fallback chain advanced to the next candidate and opened a circuit, and the
  SessionEnd consolidation job kept re-sending the same prompt on its backoff
  schedule, each attempt truncating again at the same budget.
  ([#1543](https://github.com/akitaonrails/ai-memory/pull/1543))

- `memory_read_page` now answers a missing read target with `invalid params`
  (-32602) instead of an internal error (-32603). A page absent under the
  resolved scope, and a query that matched nothing, are caller-fixable: the
  store answered, there is simply nothing there, and a client reads -32603 as
  "retry". `memory_status` and `memory_read_session_observations` already
  answer -32602 for an unknown project or session. The messages and the
  auto-resolved-scope hint are unchanged, and a genuine disk or store failure
  on the same path still answers -32603.
  ([#1544](https://github.com/akitaonrails/ai-memory/pull/1544))
- An OpenCode subagent session's end no longer hands the next session a
  sub-task baton. The generated plugin forwarded the session's `parentID` as
  the `agent_id` subagent marker on `session.created` but not on
  `session.deleted`, so the `session-end` request carried no marker, the
  router did not recognize a child session, and it published a summary page
  and an open handoff that the next root session in that checkout consumed.
  ([#1546](https://github.com/akitaonrails/ai-memory/pull/1546))
- `install-mcp --client opencode --flavor <dialect>` now writes the flavored
  URL into the OpenCode entry. Both OpenCode entry builders read the server
  URL directly instead of the flavored one, so the flag was silently dropped
  for a client its own documentation names: a Vertex-routed install got a bare
  URL and 400'd every `tools/list`. OpenCode installs that do not pass
  `--flavor` are unchanged.
  ([#1547](https://github.com/akitaonrails/ai-memory/pull/1547))

- A `title:` or `body:` filter on an unspaced-CJK search now narrows the
  auxiliary CJK stream too. The stream read `:` as a plain run separator and
  dropped the qualifier, so its MATCH searched every indexed column including
  the path, and because that stream is unioned into the primary one,
  `title:支付服务` returned pages holding the term only in their body or path.
  ([#1548](https://github.com/akitaonrails/ai-memory/pull/1548))
- An Anthropic structured answer the model stopped at `max_tokens` is now a
  terminal truncation instead of a successful result. The provider did not read
  `stop_reason`, so a `tool_use` block holding only the JSON produced before the
  budget ran out was returned as a complete answer: a consolidation or bootstrap
  chunk could write a fraction of its pages and report success. On models that
  reject a forced `tool_choice` the same stop reported "no tool_use block"
  instead. Non-structured completions are unchanged — prose cut at the budget is
  still returned.
  ([#1549](https://github.com/akitaonrails/ai-memory/pull/1549))
- Counted a page needing only an L0 abstract embedding in `ai-memory embed
  --dry-run` and `/admin/embed`. A page whose body vector was already current
  appeared in neither the would-embed nor the up-to-date total, so the dry run
  reported no work for a scope where a live run writes abstract embeddings.
  ([#1550](https://github.com/akitaonrails/ai-memory/pull/1550))

- The PowerShell `session-start` hooks for Claude Code, Command Code and Devin
  now wrap the handoff in the same `hookSpecificOutput` envelope their POSIX
  peers emit. On Windows the baton was written as bare text, so the harness
  logged "Hook output does not start with {, treating as plain text" on every
  start and the handoff arrived unstructured instead of as `additionalContext`.
  ([#1551](https://github.com/akitaonrails/ai-memory/pull/1551))
- The Japanese iteration mark 々 (and the other letters of CJK Symbols and
  Punctuation) now stay inside a CJK run. They terminated one, so a word like
  日々 was indexed as two single characters and searching it emitted a bare
  `"日"` that matched every page containing 日 while 日々 itself was unfindable
  as a unit. Indexing changes with this, so existing rows keep their old terms
  until they are rewritten or the index is rebuilt by a compaction.
  ([#1552](https://github.com/akitaonrails/ai-memory/pull/1552))
- `memory_write_page` now refuses a relation target whose `project:` or
  `workspace/project:` scope holds a character the link reader cannot parse,
  such as a space in `my project:notes/x`. The write boundary used a looser
  character class than the reader, so the colon stayed inside the page path on
  read and the write created an edge to a path no page write can ever produce —
  a `links` row that could never be resolved.
  ([#1553](https://github.com/akitaonrails/ai-memory/pull/1553))

- `memory_handoff_cancel` now answers a well-formed `handoff_id` that resolves
  to nothing in scope with `invalid params` (-32602) instead of an internal
  error (-32603). The lookup succeeded and found no row, so the id is the
  caller's to fix, and a client reads -32603 as "retry". The same function
  already answered a malformed id with -32602, and `memory_handoff_accept`
  reports an unknown id as `{"handoff": null}`. The message is unchanged and
  still identical for an absent id and for another operator's, so cancelling
  reveals no more than before.
  ([#1554](https://github.com/akitaonrails/ai-memory/pull/1554))
- `ai-memory run` no longer loses a native session selector that sits behind a
  non-UTF-8 argument. The scan over the forwarded arguments ended at the first
  argument that was not valid UTF-8 instead of skipping it, so a run pinned with
  an explicit `--session`/`--resume` kept no expected session id and fell back
  to post-exit transcript discovery.
  ([#1555](https://github.com/akitaonrails/ai-memory/pull/1555))
- Dropped the merged-away page's `summary:` and `abstract:` from the merge
  stub left behind by cold-cluster dedup and the dream pass. Retrieval
  prefers a frontmatter summary over the body, so the brief and the
  non-keyword hit lists described the stub as if it still held content that
  had moved to the survivor. The pre-merge frontmatter stays reachable
  through the supersession chain; compaction is unchanged.
  ([#1556](https://github.com/akitaonrails/ai-memory/pull/1556))

- The OKF migration now conforms a page nested under a project bundle whose
  file is named `index.md`. Only the bundle root's own `index.md` is reserved,
  but the skip matched on the basename at any depth, so the database row was
  conformed while its file kept pre-OKF frontmatter — and markdown being the
  source of truth, the next reindex de-conformed the page again.
  ([#1557](https://github.com/akitaonrails/ai-memory/pull/1557))
- A root Grok session no longer loses its handoff because a tool used one of
  the subagent marker key names. The shell `post-tool-use` hook matched
  `agent_id` and its siblings as substrings of the whole raw payload, so a key
  of that name nested in `tool_input` or in a structured tool result, or a
  blank top-level value, suppressed the fetch. The gate now reads top-level
  keys with a non-empty string value, like the native router and the
  PowerShell bundle already did.
  ([#1558](https://github.com/akitaonrails/ai-memory/pull/1558))
- `ai-memory list-projects` now sizes its columns by terminal display width
  rather than bytes. Accented names no longer add phantom padding, and
  double-width CJK project or workspace names no longer shift later columns.
  ([#1559](https://github.com/akitaonrails/ai-memory/pull/1559))

- A byte size just under a unit boundary now carries to the next unit instead
  of printing a magnitude that unit cannot hold. The scaling loop stopped at
  `value < 1024.0` while the result is rendered with one decimal, so a
  database of 1 MiB minus six bytes made `ai-memory status` report
  `1024.0 KiB on disk` rather than `1.0 MiB`; `reclaim-ledger-versions` could
  likewise print `1024.0 KiB -> 1.0 MiB` for a six-byte difference.
  ([#1562](https://github.com/akitaonrails/ai-memory/pull/1562))

- A popped message can no longer pass its own text off as `ai-memory`'s. The
  human rendering of `message pop` printed the sender's subject above the
  security notice and the body between two literal fence lines, neither
  escaped, so a newline in the subject forged lines in the provenance block
  and an `---- end of body ----` line in the body closed the fence early. The
  subject now renders on one line, body fence markers are neutralized, and
  terminal control sequences are stripped; both JSON paths remain structural.
  ([#1563](https://github.com/akitaonrails/ai-memory/pull/1563))

- Hardened the privacy strip. It now matches credentials across invisible
  formatting characters while storing text that matches nothing byte for
  byte, so emoji sequences and scripts that need joiners are untouched. It
  also redacts PGP private keys and private-key blocks that have no end line,
  as long as a key-body line follows the header, so prose that only names a
  header is kept. Tag characters outside an emoji flag, a byte-order mark
  after the start of the text and the invisible math operators are stripped.
  The allowlist no longer exempts private keys, and blank `[sanitize]
  allowlist` entries are ignored with a warning at startup. The client scrub
  shared by the relay and importer companions follows the same rules.
  Reported by @FabioLeitao.
  ([GHSA-jpcg-w5c5-wcv6](https://github.com/akitaonrails/ai-memory/security/advisories/GHSA-jpcg-w5c5-wcv6))

- Tightened how client commands decide that a server URL is on this machine
  before using the install's root token. The check now reads the host the
  HTTP client would connect to and refuses any URL that carries user
  information. `ai-memory upgrade` uses the same check for its remote-server
  note. Reported by @FabioLeitao.
  ([GHSA-jpcg-w5c5-wcv6](https://github.com/akitaonrails/ai-memory/security/advisories/GHSA-jpcg-w5c5-wcv6))

- `ai-memory restore` no longer replaces an existing live `config.toml` with
  the backup's by default, because a config can run commands and redirect
  data. The backup's copy is saved beside it as `config.toml.from-backup`
  (owner-only) with a notice; pass the new `--with-config` flag to restore it
  in place as before. A data dir with no config still gets the backup's. When
  the live config is kept and the restored database holds API keys, its pepper
  must match them, or the restore stops before changing anything and names
  `--with-config`. Reported by @FabioLeitao.
  ([GHSA-jpcg-w5c5-wcv6](https://github.com/akitaonrails/ai-memory/security/advisories/GHSA-jpcg-w5c5-wcv6))

- `install-mcp --client codex` now makes Codex ask before running ai-memory's
  destructive tools. The read tools still run without a prompt; every tool
  annotated destructive (deleting or restoring a page, the forget sweep,
  accepting or cancelling a handoff, popping or cancelling messages, recall
  feedback) gets a per-tool `approval_mode = "prompt"`, and a re-install keeps
  per-tool settings you already chose. Re-run `install-mcp --client codex
  --apply` to update an existing entry. Reported by @FabioLeitao.
  ([GHSA-jpcg-w5c5-wcv6](https://github.com/akitaonrails/ai-memory/security/advisories/GHSA-jpcg-w5c5-wcv6))

- A server profile's `roots` now admit a checkout whose Windows cwd differs
  from the registered root only in case. `Path::starts_with` compares
  components byte-exactly, so a root added as `C:\Work\team-b` refused
  `C:\work\team-b\repo` and every event captured there was dropped as
  `rejected-outside-roots`, even though the home marker's own path routes
  already fold case under a drive-letter or UNC root. POSIX roots stay
  case-sensitive.
  ([#1538](https://github.com/akitaonrails/ai-memory/pull/1538))

### Security

- A malformed OAuth entry in `auth.json` no longer prints the stored value in
  its error. When a field held a value of the wrong type, such as a token
  written into `expires` by a hand edit or a provisioning script, the parse
  error quoted the token, and from there it reached terminals, CI logs and
  captured hook output. The error now names the type it found with the value
  replaced by `<redacted>`.
  ([#1565](https://github.com/akitaonrails/ai-memory/pull/1565))

## [3.2.1] - 2026-10-22

### Fixed

- Fixed a 3.2.0 regression that serialized local document embedding and
  dropped the vector stream from queries that raced a document embed: the
  local embedder's capacity was hardcoded to two slots (one document, one
  query), and a query finding both busy degraded instantly to lexical
  search. The scheduler is now sized from the CPU (half the available
  parallelism, clamped to between 2 and 6 slots, documents capped at
  capacity minus one so a slot stays query-reserved), and a query waits up
  to 250 ms for a slot before degrading. On the affected LongMemEval-S
  question set this restores query latency (p50/p95 66/87 ms vs 3.2.0's
  118/295) and ingest throughput; the reported hit@5 delta (0.947 → 0.938)
  traces to rank-boundary questions that flip between identical runs, not
  a deterministic ranking change (#1580).

- Fixed `ai-memory run` (and `show`, `continue`, `resume`, `workstreams`,
  `rename-workstream`) failing on Windows Docker installs with "managed
  claude executable `claude` was not found in the host PATH": the PowerShell
  wrapper sent every subcommand into the Linux helper container, which has no
  host CLIs. It now intercepts the same six commands `bin/ai-memory` does and
  runs them through the native Windows client, resolved from
  `AI_MEMORY_NATIVE_BIN` or the documented
  `%LOCALAPPDATA%\Programs\ai-memory` install location, inheriting
  `AI_MEMORY_SERVER_URL` (an explicit value wins) and exiting with the
  child's code. A missing client fails with the documented installer command
  instead of entering the container; the POSIX wrapper's Git Bash refusal
  points at the same installer; and the missing-harness error's Docker
  advice is now OS-aware: on Windows it names the native-client installer
  rather than `ai-memory upgrade`, which in the `.ps1` wrapper only pulls
  the image. Managed launches keep the hook/MCP wiring rendered through the
  container (the wrapper sets `AI_MEMORY_RUN_AUTOWIRE=false` for the
  intercepted commands unless the operator set it). (#1545)

- Corrected four statements in `SECURITY.md` that no longer matched the
  code, plus two clarifications in the same sections (#1561). The
  `AI_MEMORY_ALLOWED_HOSTS` default is `localhost`, `127.0.0.1`, and `::1`;
  request bodies are capped at 10 MB on `/mcp`, `/hook` and `/workstream/*`,
  32 MB on `/admin/*`, and 2 MB elsewhere; the auth ladder is anonymous,
  root bearer, proxy-asserted user, and `aim_` database-user tokens (the
  server itself never validates OIDC tokens, and the doc no longer says it
  does); and the "no per-user or per-project ACLs" claim became the current
  truth about per-project access modes and grants, including that
  proxy-asserted identities are not subject to them. The bearer-check note
  now lists the routes that are public by design (`/healthz`, `/auth/login`,
  `/auth/recovery`, `/favicon.ico`, custom `/web` static assets).
- Completed the egress inventory in `DATA_HANDLING.md` and documented the
  default first-start behavior it omitted (#1560). A default install now
  says plainly that the local embedder downloads its three model files
  (~87 MB, sha256-pinned) from `huggingface.co` in the background on first
  start, that nothing stored is sent, and how to avoid it
  (`embedding_provider = "none"` or offline pre-seeding). The export table
  gained the rows it was missing: the `llm_provider` pipeline (including
  the `codex`, `copilot` and `cursor` CLI-backed providers), the `copilot`
  embedding provider, and `[[admission_webhooks]]`, plus notes on
  `ai-memory upgrade`'s GitHub release query, the OAuth/OIDC login flows,
  and remote `server_url` topologies. `docs/airgapped-install.md`'s
  runtime summary was corrected to match.

## [3.2.0] - 2026-10-21

### Added

- Added a pointer to a session that was cut off without ending, so the next
  harness continues from where the work stopped. A harness stopped by a quota
  limit, a crash or a closed terminal never runs its session end and leaves no
  handoff, so an older handoff or session page used to win. When the newest
  captured work in a project belongs to another of the operator's sessions that
  never ended and left no handoff after that work, session start now adds a
  one-line notice naming that session's agent and id (no captured text) and
  pointing at `memory_read_session_observations`. `memory_briefing` gains
  `recent_sessions` (newest work first, with `ended`, `handed_off` and
  `current`), and `memory_handoff_list` gains `newer_unended_session` when such
  a session is newer than every open handoff. `memory_read_session_observations`
  also accepts a harness's native session id, such as an OpenCode `ses_…` id.
  The routing block and the handoff and retrieval skills tell agents to read
  that session first, and the TypeScript SDK decodes both new fields.

- Added first-party Qwen Code lifecycle and MCP support. `install-mcp --client
  qwen-code` (aliases `qwen`, `qwencode`) writes a Streamable HTTP `httpUrl`
  entry into `~/.qwen/settings.json`; a token-file install gets a stdio
  `mcp-bridge` instead of embedding the bearer. `install-hooks --agent
  qwen-code` merges ten Claude-shaped events (including `PostToolUseFailure`
  and subagent start/stop) as native `ai-memory hook` commands with no script
  bundle. `SessionStart` injects the prior handoff as
  `hookSpecificOutput.additionalContext`; Qwen's own `SessionEnd` closes the
  session. User-prompt capture uses Qwen's provenance-bearing
  `submitted_prompt`, never an automated continuation in `prompt`. Capture
  exclusions cover Qwen's Gemini-like tools. Qwen-only agent backups include
  its settings, both documented global skill roots, `QWEN.md`, and `AGENTS.md`.
  `setup` detects `~/.qwen`. There is no `ai-memory run qwen` adapter. ([#1487](https://github.com/akitaonrails/ai-memory/pull/1487))
- Added zero-LLM substring retrieval for unspaced Han, Kana, and Hangul text.
  Pages and observations gain transactionally maintained derived unigram/bigram
  FTS indexes; existing unicode61 search and ranking remain unchanged for
  Latin-only queries, while CJK matches participate in project, scoped,
  global, temporal, raw-observation, and observation-evidence retrieval.
  Upgraded stores backfill missing derived rows before accepting traffic, and
  compaction rebuilds the auxiliary indexes with the existing FTS set.
  ([#1433](https://github.com/akitaonrails/ai-memory/issues/1433))
- Added a first-party Hermes Agent lifecycle and an `install-mcp --client
  hermes` client. The `hooks:` block `install-hooks --agent hermes` prints now
  also wires `on_session_start`, `pre_llm_call` (the prompt, never the
  conversation history), `on_session_end` as a turn boundary and
  `on_session_finalize` as the session end, and each primary-session
  `pre_llm_call` checks for a pending handoff and brings it into that turn as
  `{"context": ...}`. This includes a resumed native Hermes session. Hermes
  events route by `TERMINAL_CWD` when it names an existing directory (in a
  gateway, for the default profile only), and a delegated child's lifecycle
  events are dropped. When the
  community memory-provider plugin is installed, only the tool events are
  printed. `install-mcp --client hermes` prints an `mcp_servers:` entry for the
  session-less stdio bridge with no token in it, and refuses `--apply` ([#1454](https://github.com/akitaonrails/ai-memory/pull/1454)).
- Added bounded local document extraction to the optional
  `ai-memory-importer` companion. Its dry-run-first `document` command accepts
  UTF-8 text/Markdown, text-layer PDF, DOCX, PPTX, XLSX, VTT, and
  SRT; emits attributed, client-sanitized markdown; caps source, archive,
  extracted text, PDF streams, sheets, and actual XLSX cells; and uses deterministic digest paths plus
  conditional MCP writes for idempotent reruns without overwriting an existing
  foreign page. OCR, images, and raw audio/video remain explicitly out of
  scope.
- Added passive per-workload LLM telemetry to `ai-memory status --json` and
  `GET /admin/status`. The closed workload set covers consolidation, reranking,
  auto-improvement, bootstrap, query answers, lint, profile maintenance, dream
  merges, and experience abstraction, with process-lifetime calls, outcomes,
  latency buckets, coarse token estimates, and provider-reported usage where available.
  It records no prompt or output text and does not change provider/model
  routing; public per-workload profiles remain deferred until measurements
  justify their configuration and authentication surface.
- Added structured pre-rank filters to `memory_query`: case-insensitive
  `kind` and exact-array `tag`, typed `tier`, and literal `path_prefix`.
  Filters apply consistently to project/scopes hybrid streams, cross-project
  global FTS, `as_of` history, supplemental profile hits, and `pin_first`.
  Filtered misses omit raw observations because those rows carry no page
  metadata.
- Added a read-only managed-workstream operations page at
  `/web/workstreams`. On multi-user servers it is root-only; single-operator
  installs retain their existing admin access. The bounded overview orders
  live and interrupted work first and derives `active`, `interrupted`,
  `completed`, and `empty` only from trusted run lifecycle fields and imported
  event counts. It exposes no checkout paths, repository fingerprints, lease
  owners, native session ids, source metadata, or transcript content, and it
  does not broaden checkout-local `resume` discovery.
- Added `ai-memory recent` and `GET /admin/recent` to list the most recently
  updated wiki pages, newest first. The listing is scoped to the current
  project by default (cwd / marker, or `--workspace`/`--project`); `--global`
  covers every project and labels each hit with its workspace and project.
  Each hit carries an RFC 3339 `updated_at` instead of an FTS rank. The admin
  route is root-only on a multi-user server, the same as `/admin/search`.
  ([#1379](https://github.com/akitaonrails/ai-memory/pull/1379))
- Added an explicit `serve --allow-insecure-human-auth` (or
  `AI_MEMORY_ALLOW_INSECURE_HUMAN_AUTH=true`) escape hatch for operators who
  knowingly run password login over plain HTTP on a private LAN.
  It is off by default, does not bypass the separate unauthenticated-exposure
  guard, and prints an unconditional warning that passwords, API credentials,
  session cookies, and memory data cross the network unencrypted. (#1383)
- Added first-party Cline CLI lifecycle and MCP support. `install-hooks
  --agent cline --apply` writes Cline's nine supported event files globally or
  per project, refuses user-owned and symlinked targets, and keeps embedded
  bearer credentials owner-only. `install-mcp --client cline --apply` writes
  Cline's top-level transport fields and explicitly selects Streamable HTTP.
  Detached start hooks recover handoffs through MCP; yolo mode disables hooks.
  Also added `install-mcp --client freebuff --apply`, which registers the
  sessionless stdio bridge in `~/.agents/mcp.json` without starting another
  ai-memory server. (#1335)
- Added `[ingest] drop_events` (env `AI_MEMORY_INGEST__DROP_EVENTS`), an
  opt-in list of hook event kinds the server drops before storing them, next
  to the existing `drop_tool_use_id_prefixes` and `drop_tool_names`. A dropped
  event gets the same 202, `dropped_by_policy` metric and log as the other
  policy drops, before the rate limiter, the ingest semaphore and the
  sanitizer. Subagent classification precedes every id, name, and kind matcher,
  so a matching marked event still seeds tracking for its unmarked tail. The
  default drops nothing. `Config::load` checks each value
  against the typed event kind and rejects unknown kinds and `session-start`
  and `session-end`, which open and close sessions. The motivating case is
  `pre-tool-use`: in one production store about 87% of those observations had
  an empty body, and each pairs with a `post-tool-use` that carries the
  content. With `pre-tool-use` dropped, a session whose only work was a tool
  call that never finished now ends as lifecycle-only. (#1339)
- Added opt-in `[sessions] idle_end_after_hours` (env
  `AI_MEMORY_SESSIONS__IDLE_END_AFTER_HOURS`; default `0`, off; range
  `1..=8760`). When it is set, the `serve` maintenance tick ends open sessions
  with no activity for that many hours, such as a harness that crashed or a
  terminal closed before `SessionEnd` fired. A substantive session gets its
  `sessions/<id>.md` summary page, attributed to the session's owner, and its
  consolidation job. A boundary-only session ends lifecycle-only and releases
  the handoff it had accepted. The tick creates no handoff of its own and skips
  sessions bound to an active managed run. A real `SessionEnd` or checkpoint
  that lands while the tick runs wins, and the tick leaves that session alone.
  `ended_at` is the time of the tick; the session's last activity goes in the
  audit row. (#1338)
- Added first-party DeepSeek Harness (DSH) lifecycle capture. `install-hooks
  --agent dsh` (aliases `deepseek-harness`, `deepseek_harness`) stages the
  new `hooks/dsh` bundle, writes a generated Claude Code `hooks.json`, and
  prints a Cordis patch fragment that mounts DSH's official
  `@deepseek-ai/dsh-hooks-claude-code` bridge on
  `$DSH_HOME/profiles/<profile>/cordis.patch.yml` (the patch file itself is
  never auto-written). DSH sessions are stored as `agent=dsh`, and its bridge
  forwards Claude Code-shaped tool payloads, so `[capture] ignore_paths`
  exclusions apply to its file, search and shell calls (Windows `pwsh`, POSIX
  `bash`) like any other Claude-shaped adapter; the reserved Code Mode
  `run_code` transport that its `headless` profile calls is treated as inline
  program source. The bridge
  delivers exactly seven events (no `PreCompact` or `SessionEnd`) and runs
  `SessionStart` detached, so finished sessions close with
  `finalize-session --agent dsh` and pending handoffs are recovered via
  `memory_handoff_list` / `memory_handoff_accept`. (#1337)
- Added an Antigravity skill root family. `install-skills --agent antigravity`
  (aliases `antigravity-cli`, `agy`; also `install-instructions
  --skills-agent antigravity`) writes the managed Agent Skills to both global
  roots Antigravity reads, `~/.gemini/config/skills` (App and IDE) and
  `~/.gemini/antigravity-cli/skills` (CLI), or to the workspace
  `.agents/skills` with `--scope project`. `ai-memory setup` now uses it for
  Antigravity, which until now got its skills only in `~/.agents/skills`, a
  directory Antigravity does not read globally, and `uninstall --only skills`
  sweeps both roots. MCP-driven routing refresh and `backup-agents` expose and
  preserve the same roots, including the shared project `.agents/skills`.
  (#1357)

- Added a read-only wiki link graph in `/web`. The header **Graph** link opens
  `/web/graph`, a server-rendered map of resolved wikilinks whose endpoints sit
  in different projects, plus a table of those edges. Each project page links
  to `/web/w/<workspace>/<project>/graph`, which shows that project's latest
  unexpired pages and the resolved same-project links between them as an SVG
  and a table. Both pages are HTML with no JavaScript.
  `GET /api/v1/graph` now accepts `workspace` and `project` together to return
  only edges that touch that project (passing only one of them is `400`), and
  `GET /api/v1/workspaces/{workspace}/projects/{project}/graph` returns the
  in-project node and edge lists. A project graph keeps at most 120 pages,
  preferring those with the most same-project neighbours and then the most
  recently updated; the store applies that cap before loading nodes or their
  selected edges into the process. Expired pages stay hidden. An edge still
  requires both ends to be readable. ([#1378](https://github.com/akitaonrails/ai-memory/pull/1378))
- Added Antigravity hook parity with Claude Code and Codex for the `agy` CLI,
  the desktop App and the IDE. `install-hooks --agent antigravity-cli` now
  wires the native `SessionStart`, which injects the handoff as an
  `injectSteps` system message that stays in the conversation, where the
  ephemeral message it replaces was dropped after the first turn.
  `PreInvocation` gets its own `pre-invocation` handler: on builds without
  `SessionStart`, the first turn's call stands in for it, once per
  conversation. The native hook now reads each turn's prompts from the
  conversation's own transcript (`transcriptPath`, only under
  `~/.gemini/<app>/brain/<conversationId>/`, no symlinks out of it, 8 MiB per
  hook and a 100 ms filesystem deadline) and records them as user prompts,
  so summaries and handoffs carry them. A timed-out read leaves its cursor
  untouched and retries on the next hook. A `Stop` that finished the turn
  (`NO_TOOL_CALL` or `model_stop`) with
  `fullyIdle: true` writes a turn checkpoint, the path OpenCode 2 uses: the
  summary page and one handoff are refreshed and the conversation stays open,
  so `finalize-session` is no longer needed for normal use. It remains the
  explicit close, hands the baton over without the 10-minute quiet period,
  queues opt-in consolidation, and skips its `history.jsonl` prompt replay
  for a conversation whose prompts were already captured. Rerun
  `install-hooks --agent antigravity-cli --apply` to add `SessionStart`;
  existing native-hook installs get the prompts and checkpoints without it.
  A turn checkpoint never ends a conversation, even a turn with no captured
  prompt or tool call, so a handoff it claimed at start is never offered
  again; the conversation start is claimed atomically, so a concurrent
  `SessionStart` and first `PreInvocation` cannot both claim a handoff;
  prompt ingest keys include the prompt's time and text, so a rewind that
  reuses step numbers keeps its new prompts; and the transcript is opened
  with `O_NOFOLLOW | O_NONBLOCK` and, on Linux, checked against the opened
  descriptor's real path. (#1416)

### Security
- Updated `rustls-webpki` 0.103.15 → 0.103.16 for
  [RUSTSEC-2026-0382](https://rustsec.org/advisories/RUSTSEC-2026-0382), in
  which certificate verification may accept distrusted intermediates, and
  `webpki-root-certs` 1.0.9 → 1.0.10 for
  [RUSTSEC-2026-0383](https://rustsec.org/advisories/RUSTSEC-2026-0383), whose
  bundled roots still trusted distrusted emSign intermediates. The importer,
  relay and wiki sync companions take the same `rustls-webpki` bump.

### Fixed
- Memory health no longer counts a link from a superseded or TTL-expired page
  version as a reference. A page whose only inbound link was removed by a later
  edit was reported as linked, because the old version's `links` row survives
  the edit and is repointed at the new latest id, so the orphan count and
  detail list disagreed with the page's own backlinks.
  ([#1526](https://github.com/akitaonrails/ai-memory/pull/1526))
- A `title:` or `body:` search filter whose term carries a second colon, such
  as `title:10:30`, now keeps its column. FTS5 read the inner `10` as a column
  name and rejected the whole expression, so the query degraded to the quoted
  bag of words: the filter was dropped and every column was searched for the
  literal word `title`.
  ([#1521](https://github.com/akitaonrails/ai-memory/pull/1521))
- `memory_query` no longer discards `as_of` on a global query. An explicit
  `global` with `as_of` is now refused, as the error message always claimed,
  and a repo on `[recall] default_global` no longer broadens an `as_of` query
  to global and answers it with present-day rows.
  ([#1519](https://github.com/akitaonrails/ai-memory/pull/1519))
- A `429` or `5xx` from a token endpoint is now a transient provider error
  rather than an auth failure. The Copilot token exchange, which GitHub
  rate-limits per account, and the shared `openai-oauth`/OIDC refresh both
  reported throttling as a credential problem, so the fallback chain stopped
  instead of trying the next provider and the operator was told to log in
  again for a grant that was never rejected.
  ([#1520](https://github.com/akitaonrails/ai-memory/pull/1520))
- The Gemini embedder now cuts a long input to the same 8 KB head as the
  OpenAI, Copilot and local embedders. It sent the whole page, while the
  default `gemini-embedding-001` takes at most 2,048 input tokens, and a
  refused page embedding is only logged, so a long page could end up with no
  vector. Voyage is unchanged: its API shortens an over-long input itself.
  ([#1513](https://github.com/akitaonrails/ai-memory/pull/1513))
- `memory_auto_improve` now reports an exhausted implicit queue (every
  completed session already has a review run) as invalid params instead of an
  internal error, matching `memory_consolidate` for the same dead end. A client
  no longer reads the routine end of the queue as a server fault and retries it.
  ([#1518](https://github.com/akitaonrails/ai-memory/pull/1518))
- The wiki's git objects and refs are now synced to disk. libgit2 renamed them
  into place without fsync, so a power loss could keep a ref naming a commit
  whose object was never written, and every later checkpoint failed. Every
  repository handle ai-memory opens now turns on `core.fsyncObjectFiles` from
  `.git/ai-memory.gitconfig`, a file only libgit2 reads, so `.git/config` is
  untouched and the git CLI shows no deprecation warning; an explicit `false`
  in `.git/config` is kept. Startup logs an `ERROR` when `HEAD` does not
  resolve to a readable commit, and a failed bootstrap auto-commit is logged
  instead of dropped. `docs/lifecycle-ops.md` describes the repair for a
  history already damaged.
  ([#1511](https://github.com/akitaonrails/ai-memory/issues/1511))
- `memory_read_session_observations` without a `session_id` now reads the
  session with the newest captured work instead of the newest one that ended.
  A harness cut off without ending (a quota limit, an aborted turn, a crash)
  never ends its session, so the agent taking its work over used to get some
  older session, often from another checkout. The default now skips the
  caller's own session, prefers a session in the caller's cwd, and falls back
  to the latest completed session only when nothing worked in the last 30
  days. It stays owner-filtered and bound to the resolved project. (#1512)
- A server whose hook router could not read an event's working directory no
  longer stops answering. Resolving a new cwd runs git discovery on it, and on
  macOS a LaunchAgent binary without Full Disk Access can block indefinitely
  on a protected folder or an external volume; that `open()` ran on an async
  worker and took the HTTP accept loop down with it, so `/mcp`, `/hook` and
  `/web` timed out while the hook spool grew. Those probes now run on the
  blocking pool behind four permits and a 2-second deadline, and an event whose
  cwd cannot be read in time is dropped with a warning instead of stalling the
  server or being routed on a guess. `docs/macos.md` now explains the Full Disk
  Access grant. (#1514)
- Cold-cluster dedup (A3) and the opt-in dream pass (B2) now merge a cold set
  of exactly `min_pts` near-duplicates. The adaptive eps returned nothing when
  the cold set had no more points than the density floor, so with the default
  `min_pts = 2` a lone pair of near-identical cold pages was never merged,
  while the same pair next to any unrelated third cold page was. The
  conservative `max_eps` ceiling still decides whether a pair is close enough
  to merge.
  ([#1528](https://github.com/akitaonrails/ai-memory/pull/1528))
- Reserved local-embedding capacity for interactive search queries instead of
  allowing concurrent page writes and backfill work to occupy every inference
  slot. Excess queries fall back immediately to non-vector retrieval, and a
  cancelled request retains its slot until its non-cancellable CPU inference
  actually ends. An opt-in real-model contention probe records idle,
  unbounded-load, and bounded-load p50/p95 query latency; hosted embedding
  providers are unchanged.
- A resumed session that had already ended (`claude --resume` after a clean
  exit, for example) now still gets its session-start context. An ended
  session may not take a handoff, and that refused claim used to fail the whole
  session-start fetch whenever any handoff was open, so the session started
  with no profile digest, brief or inbox notice at all. The refusal now costs
  only the claim, and the handoff stays open.
- The pre-migration backup archive is now flushed and fsynced, along with its
  directory entry, before it is verified. The buffered tail of the archive was
  written when the writer was dropped, which discarded any error such as a full
  disk, and the file was never put on stable storage, while the verify pass
  re-read it from the page cache and cleared a destructive in-place migration
  to run. ([#1525](https://github.com/akitaonrails/ai-memory/pull/1525))
- Wiki commits no longer stage a write that is still in flight. A batch write
  keeps one `.ai-memory-tmp.` staging file per page on disk across an await,
  and auto-commit does not hold the mutation lock, so a checkpoint landing in
  that window committed page bodies that were never installed, and a batch
  that then failed left the rolled-back content in wiki history. A page parked
  under `.ai-memory-delete.` during a delete, or under
  `.ai-memory-tmp.move-session.` during a session move, is left out the same
  way. ([#1524](https://github.com/akitaonrails/ai-memory/pull/1524))
- The `/hook` ingress now skips a leading UTF-8 byte-order mark, as the native
  hook client already did. Some PowerShell pipelines add that marker when
  writing to a native process, and the POSIX shell hook bundle forwards the
  payload byte for byte, so such an event was answered with 400 and lost
  without notice. `ai-memory-relay enqueue --file` skips the marker too.
  ([#1523](https://github.com/akitaonrails/ai-memory/pull/1523))
- The POSIX shell hook bundle, the PowerShell bundle under PowerShell 7 and the
  generated TypeScript integrations under Bun (OpenCode) no longer send
  their requests through an environment proxy (`http_proxy`, `HTTPS_PROXY`,
  `ALL_PROXY`), as the native hook client and the bridge already did. In a
  sandbox that exports a proxy with an empty `NO_PROXY`, the filtering proxy
  answered the loopback event POST with a 4xx, which the hooks treat as a
  permanent rejection, so the observation was dropped instead of spooled and a
  resuming session got no handoff. A server reachable only through a forward
  proxy needs a direct route; see `docs/https-via-proxy.md`.
  ([#1517](https://github.com/akitaonrails/ai-memory/pull/1517))

- `mcp-bridge` and the `setup` self-check no longer send requests to the
  server through `HTTP_PROXY`, `HTTPS_PROXY` or `ALL_PROXY`, as the hooks
  already did. A sandbox such as ai-jail exports a proxy with an empty
  `NO_PROXY`, and its filtering proxy answered the bridge's loopback
  `initialize` with 405, so Claude Code reported `CONNECTION_CLOSED`.
  ([#1504](https://github.com/akitaonrails/ai-memory/pull/1504))

- Bound native API-key hashes to their configured pepper with a persisted
  Argon2id verifier. A restored database with existing keys now refuses to
  serve under a different `[auth].token_pepper` instead of silently making
  every key return 401; pepper rotation remains allowed while no keys exist.
  A pre-verifier backup containing keys fails restore until the operator
  supplies the original pepper and explicitly confirms it with
  `--accept-legacy-api-key-pepper`.
  ([#1495](https://github.com/akitaonrails/ai-memory/issues/1495))

- Defined `ai-memory backup` as an online data snapshot rather than a complete
  host image, documented the separately protected config, root credentials and
  raw ledgers a host move may require, and corrected the missing-token startup
  remedy so it no longer implies the snapshot contains `auth-tokens/`.
  ([#1494](https://github.com/akitaonrails/ai-memory/issues/1494))

- Accepted the exact CRLF migration checksums written by Windows-native
  releases before v3.1.2 and replaced them transactionally with the current
  LF checksums before strict migration validation. Unknown migration changes
  still fail closed. Bare hook reinstallation also preserves the Windows
  exec-form `--capture-assistant` opt-in instead of looking only inside the
  command string. ([#1499](https://github.com/akitaonrails/ai-memory/issues/1499))

- Held `ai-memory run` heartbeat notices while the harness owns the terminal
  and printed them after it exits, so a server outage no longer writes into the
  harness TUI. Only the first and latest notices are replayed, with a count of
  the rest. A transport outage that changes into a server-side lease rejection
  is reported as a new state instead of being hidden as the same failure.
  ([#1500](https://github.com/akitaonrails/ai-memory/issues/1500))

- Dropped stopwords from the quoted bag of words a search falls back to when
  its text looks like FTS5 syntax but does not parse, such as a question
  ending in `(prod)`. The fallback OR-joined `what`, `is`, `the` and `of`
  with the content words; it now filters through the configured
  `[search.fts] stopwords` list like a bare query, keeps the words the user
  quoted, and keeps every word when all of them are stopwords.

- Corrected `docs/lifecycle-ops.md` on two points. The `move-project` merge
  drops everything the source project owns, the same set `purge-project`
  deletes. This includes managed workstreams, cross-project messages,
  auto-improvement and profile-harvest state, consolidation jobs, ingest keys,
  entities, page feedback, per-session purge tombstones and project grants as well as
  sessions, observations and handoffs. The `400` for sending both a project
  name and an id, or neither, applies to the admin endpoints; the CLI resolves
  a missing project from the marker, the git remote, then the directory name.
  ([#1493](https://github.com/akitaonrails/ai-memory/issues/1493))

- Named the install's data dir in the `mcp-bridge` entries `install-mcp`
  writes for Claude Code and the session-less stdio bridge when the install
  was given one (`--data-dir`, `AI_MEMORY_DATA_DIR` or `data_dir` in the
  config), as Muse's entry and the install's hooks already did. The bridge
  read the default data dir's config and token instead, so on a host whose
  server uses the default data dir it authenticated with the server's root
  bearer.
  ([#1489](https://github.com/akitaonrails/ai-memory/issues/1489))

- Let `ai-memory mcp-bridge` send the key `install-hooks --apply --auth-token`
  stored in `<data_dir>/auth-token` when no bearer is configured, ahead of the
  loopback root token discovery, so a remote
  client whose hooks hold a native API key wires the MCP bridge without
  copying the key. `install-mcp --apply --auth-token` now stores the key there
  for the Claude Code (`--session-aware`) and Muse bridge entries and leaves
  it out of the harness config, where it outlived a key rotation; existing
  entries keep working. `uninstall --only hooks` keeps the stored key while
  such a bridge entry remains. ([#1490](https://github.com/akitaonrails/ai-memory/issues/1490))

- Kept the bridge from sending a discovered root key to a non-loopback
  server, rejected a malformed stored user key instead of falling through to
  root, made failed installs roll back newly stored credentials, and included
  the absolute data dir in Claude Code's printed session-aware command as well
  as its JSON entry.
  ([#1490](https://github.com/akitaonrails/ai-memory/issues/1490))

- Moved the commented `[run.profiles.work.env]` and `[ingest]` examples in the
  default config, the `config.toml` that `ai-memory init` writes and the
  native packages install, below its top-level keys. A TOML table header owns
  every key after it, so uncommenting one of those tables and a top-level
  example below it, such as `reranker`, `capture_assistant` or
  `gemini_safe_schemas`, put that key inside the table: `[ingest]` ignored it
  without a warning, and the run profile's env map either failed the config
  load or exported the key to the harness. A config written by an earlier
  release keeps its layout; move an uncommented table there below the last
  top-level key.
  ([#1506](https://github.com/akitaonrails/ai-memory/issues/1506))

- Kept the user PATH's `%VAR%` entries and its `REG_EXPAND_SZ` type when
  `install.ps1` and the desktop installer add their directory to it, and in
  the manual PATH steps in `docs/windows.md`. They read the value with
  `[Environment]::GetEnvironmentVariable`, which expands every variable, and
  wrote it back with `SetEnvironmentVariable`, which stores it as `REG_SZ`.
  An entry such as `%JAVA_HOME%\bin` stopped following its variable, and one
  whose variable the installer did not have stayed as literal text that
  Windows never expands. A PATH an earlier install already rewrote keeps its
  expanded entries.
  ([#1509](https://github.com/akitaonrails/ai-memory/issues/1509))

- A tool hook for an MCP tool now keeps the tool's identity. Claude Code, Codex
  and Hermes Agent name such a call `mcp__<server>__<tool>`; it stays in the
  `unknown` family, so its arguments and result are still never stored, but the
  observation now adds `mcp_server` and `mcp_tool` lines when the name is at
  most 64 ASCII letters, digits, `_` or `-`. Before, a session's MCP calls
  were recorded only as `tool unknown`.
  ([#1522](https://github.com/akitaonrails/ai-memory/issues/1522))

## [3.1.2] - 2026-10-20

### Fixed
- Reported appended and removed lines in wiki sync conflict summaries, with
  an explicit end-of-file marker instead of line 0 and matching previews.
  ([#1477](https://github.com/akitaonrails/ai-memory/issues/1477))
- Accepted ordinary absolute and relative Windows destination paths in the
  wiki sync companion while still refusing directory junctions and symlinks.
  ([#1476](https://github.com/akitaonrails/ai-memory/issues/1476))
- Preserved dotted Windows harness names when resolving PATHEXT wrappers, so a
  command such as `agent.tool` launches `agent.tool.cmd` rather than `agent.cmd`.
  ([#1475](https://github.com/akitaonrails/ai-memory/pull/1475))
- Corrected Pi MCP/setup-agent guidance to name `ai-memory-pi.ts`, described
  its location when `PI_CODING_AGENT_DIR` is set, and corrected OMP's
  setup-agent hint to name `ai-memory-omp.ts`.
  ([#1446](https://github.com/akitaonrails/ai-memory/pull/1446))
- Launched the wrapper executable named by a `claude*` harness spelling
  (`ai-memory run claude-glm`) when it is on the launch `PATH` and no
  `--executable` is given, instead of silently starting plain `claude` on the
  wrong account. When no such executable exists, `run` now prints a warning
  and still starts `claude`. An explicit `--executable` still wins.
  ([#1486](https://github.com/akitaonrails/ai-memory/pull/1486))
- Routed OpenCode's `claude-haiku-5-5` through the Anthropic Messages API for
  both Go and Zen instead of the unsupported Chat Completions protocol.
  ([#1470](https://github.com/akitaonrails/ai-memory/issues/1470))
- Ignored a UTF-8 byte-order mark at the start of `run --env-file` input so
  the first environment variable keeps its intended name.
  ([#1465](https://github.com/akitaonrails/ai-memory/pull/1465))
- Invoked GitHub Copilot CLI's native Windows lifecycle commands with
  PowerShell's call operator. Quoted `ai-memory.exe` paths now execute instead
  of producing a parser error that made fail-closed pre-tool hooks deny every
  tool call. ([#1462](https://github.com/akitaonrails/ai-memory/issues/1462))
- Repaired the exact migration checksum recorded by v2.2.1 for the original
  V62 page-window migration before running strict migration validation. This
  removes the false divergence error on every upgraded-server start while
  keeping unknown migration edits fatal. CI now rejects changes to migration
  files already present on a PR's base branch.
  ([#1443](https://github.com/akitaonrails/ai-memory/issues/1443))
- Kept the A2 compaction footer out of SessionStart excerpts for pages with
  CRLF line endings, matching the existing LF behavior.
  ([#1474](https://github.com/akitaonrails/ai-memory/pull/1474))
- Sent `response_format=json_schema` for structured OpenCode Chat Completions
  calls, with the existing tolerant fallback for models that reject it, so
  `memory_query answer=true` no longer receives unstructured prose by default.
  ([#1441](https://github.com/akitaonrails/ai-memory/issues/1441))
- Preserved third-party commands that share an event matcher group with an
  ai-memory hook when `install-hooks` reapplies or removes its own entry.
  ([#1453](https://github.com/akitaonrails/ai-memory/pull/1453))
- Accepted a UTF-8 byte-order mark before imported page frontmatter in the
  wiki sync companion.
  ([#1452](https://github.com/akitaonrails/ai-memory/pull/1452))
- Skipped H1-like comment lines inside fenced code when the importer derives
  page titles, including AgentMemory vault imports.
  ([#1451](https://github.com/akitaonrails/ai-memory/pull/1451))
- Recognized importer frontmatter whose closing fence is the final line of a
  file without a trailing newline.
  ([#1450](https://github.com/akitaonrails/ai-memory/pull/1450))
- Encoded spaces as `%20` instead of `+` in importer URL path segments,
  preserving workspace, project, and page names that contain spaces.
  ([#1449](https://github.com/akitaonrails/ai-memory/pull/1449))
- Recognized exclamation-mark punctuation in short automatic-handoff
  acknowledgments such as `thanks!` and `perfect!`.
  ([#1448](https://github.com/akitaonrails/ai-memory/pull/1448))
- Treated a blank `session_id` as omitted when starting an automatic
  improvement review, matching the other session-scoped MCP tools.
  ([#1447](https://github.com/akitaonrails/ai-memory/pull/1447))
- Returned MCP `invalid_params` for caller-supplied invalid page paths, tiers,
  and scopes instead of misreporting them as internal server errors.
  ([#1445](https://github.com/akitaonrails/ai-memory/pull/1445))
- Parsed an empty Markdown frontmatter block (`---` followed immediately by
  its closing fence) as frontmatter instead of folding it and later thematic
  breaks into the page body. ([#1444](https://github.com/akitaonrails/ai-memory/pull/1444))
- Kept hook events whose JSON payload contains half of a UTF-16 surrogate pair
  as a `\u` escape, such as tool output cut in the middle of an emoji. Native,
  shell, PowerShell and direct hook clients now share the bounded server-side
  repair, replacing the unpaired escape with U+FFFD instead of dropping the
  whole event.
  ([#1432](https://github.com/akitaonrails/ai-memory/pull/1432))
- Model-produced page paths are now cleaned of the remaining shapes `ensure_portable`
  refuses, a component ending in a dot or space and a DOS device name like `aux.md`,
  so one such path no longer aborts every page in a consolidation or bootstrap batch.
  ([#1430](https://github.com/akitaonrails/ai-memory/pull/1430))
- Preserved FTS5 column filters inside parenthesized groups, so queries such
  as `(title:handoff)` no longer broaden into a search for the literal word
  `title` across every indexed field. ([#1442](https://github.com/akitaonrails/ai-memory/pull/1442))
- Revalidated session-purge tombstones inside the reindex write transaction,
  preventing a queued watcher update from recreating a deleted summary after
  file cleanup failed. Kept surviving manual replacements at the same path
  editable through the watcher (#1439).
- Added exact hover timestamps to `/web` page metadata, made the OKF upgrade
  dialog keyboard accessible, and added a labelled main-navigation landmark
  with current-page state and narrow-screen wrapping.
  ([#1391](https://github.com/akitaonrails/ai-memory/pull/1391),
  [#1392](https://github.com/akitaonrails/ai-memory/pull/1392),
  [#1393](https://github.com/akitaonrails/ai-memory/pull/1393))
- Made guarded admin routes treat an omitted `confirm` as `false`, returning
  the documented 400 response without performing the operation.
  ([#1394](https://github.com/akitaonrails/ai-memory/pull/1394))
- Retried OpenAI-style embedding requests on HTTP 503 as well as 429, honored
  bounded whole-second `Retry-After` values, and made `ai-memory embed` fail
  after printing a report that contains failed pages.
  ([#1397](https://github.com/akitaonrails/ai-memory/pull/1397))
- Made pending-write diffs return an internal error when the existing page
  cannot be read instead of presenting an update as a new page.
  ([#1399](https://github.com/akitaonrails/ai-memory/pull/1399))
- Refused page-path components over 255 UTF-8 bytes and additional Windows DOS
  device spellings, while keeping existing non-portable pages readable.
  ([#1400](https://github.com/akitaonrails/ai-memory/pull/1400),
  [#1407](https://github.com/akitaonrails/ai-memory/pull/1407))
- Recognized frontmatter whose closing fence is at end of file, ignored H1-like
  comments inside fenced code when deriving titles, and gave empty exported
  wikilink labels their target as visible text.
  ([#1402](https://github.com/akitaonrails/ai-memory/pull/1402),
  [#1403](https://github.com/akitaonrails/ai-memory/pull/1403),
  [#1419](https://github.com/akitaonrails/ai-memory/pull/1419))
- Preserved FTS5 prefix operators and `title:` / `body:` filters on punctuated
  terms, and stripped managed-workstream column prefixes only at word starts.
  ([#1404](https://github.com/akitaonrails/ai-memory/pull/1404),
  [#1405](https://github.com/akitaonrails/ai-memory/pull/1405),
  [#1406](https://github.com/akitaonrails/ai-memory/pull/1406))
- Stripped reasoning that ends in a bare closing tag from OpenAI-compatible
  structured responses and found Cursor Agent JSON inside bracketed prose.
  ([#1408](https://github.com/akitaonrails/ai-memory/pull/1408),
  [#1409](https://github.com/akitaonrails/ai-memory/pull/1409))
- Matched automatic-handoff acknowledgments as whole phrases, treated Windows
  drive roots as broad session anchors, preserved Antigravity output around
  invalid UTF-8 bytes, and stopped repeating the last prompt in automatic
  handoffs.
  ([#1410](https://github.com/akitaonrails/ai-memory/pull/1410),
  [#1411](https://github.com/akitaonrails/ai-memory/pull/1411),
  [#1412](https://github.com/akitaonrails/ai-memory/pull/1412),
  [#1418](https://github.com/akitaonrails/ai-memory/pull/1418))
- Returned `invalid params` for malformed handoff and message ids and accepted
  surrounding whitespace consistently on handoff cancellation.
  ([#1413](https://github.com/akitaonrails/ai-memory/pull/1413))
- Fixed A2 compaction summaries when a heading directly precedes the first
  paragraph or the page uses CRLF line endings.
  ([#1414](https://github.com/akitaonrails/ai-memory/pull/1414))
- Excluded TTL-expired pages from cross-project degree metrics, memory-health
  counts and details, and project/workspace page-count and timestamp summaries.
  ([#1422](https://github.com/akitaonrails/ai-memory/pull/1422),
  [#1423](https://github.com/akitaonrails/ai-memory/pull/1423),
  [#1426](https://github.com/akitaonrails/ai-memory/pull/1426))
- Stopped LLM provider and embedder requests from following a redirect to
  another origin. reqwest drops `Authorization` on a cross-host hop but keeps
  every other header, so `x-api-key`, `x-goog-api-key` and operator headers
  from `llm_headers` reached whatever host a configured endpoint redirected
  to. Redirects inside the same origin still work; one that changes scheme,
  host or port now fails the request and names the origin it pointed at.
  ([#1396](https://github.com/akitaonrails/ai-memory/pull/1396))
- Required Write access before a session start records a project's `[profile]`
  `profile_contribute` / `profile_consume` flags. A Read-only viewer's explicit
  values now only narrow its own session instead of changing the flags for every
  operator in the project.
  ([#1421](https://github.com/akitaonrails/ai-memory/pull/1421))
- Stopped the server from opening files named in a hook payload. Antigravity
  step-log enrichment now runs only in the client-side `ai-memory hook`; the
  `/hook` and `/hook/batch` ingest paths store the `tool_response` the client
  sent and never read `artifactDirectoryPath` or `transcriptPath` locations
  on the server's own disk.
  ([#1420](https://github.com/akitaonrails/ai-memory/pull/1420))
- Fixed `memory_lint` reporting findings for TTL-expired pages. The lint pass
  now omits expired pages before evaluating episodic staleness, rule suggestions,
  duplicate titles, and vector or LLM contradiction candidates, preventing
  findings for expired pages that are pending forget-sweep deletion.
  ([#1390](https://github.com/akitaonrails/ai-memory/pull/1390))
- Fixed curator reporting TTL-expired pages as cold episodic candidates,
  stale slots, or title collisions. The curator now filters out expired pages
  before evaluating episodic retention scores, and uses `list_unexpired_pages`
  when evaluating slot staleness and title uniqueness.
  ([#1389](https://github.com/akitaonrails/ai-memory/pull/1389))
- Resolved percent-encoded ordinary Markdown link paths before indexing and web-route encoding, preserving spaces and Unicode filenames without double decoding or changing project scope. ([#1382](https://github.com/akitaonrails/ai-memory/pull/1382))
- Encoded a literal `%` in an exported wikilink target as `%25`, so an OKF
  bundle's Markdown link still resolves to the same literal page path.
  ([#1456](https://github.com/akitaonrails/ai-memory/pull/1456))
- Preserved indexed Markdown links with escaped or nested brackets in their labels, including links exported in OKF bundles and reindexed after import. ([#1380](https://github.com/akitaonrails/ai-memory/pull/1380))
- Fixed `open_feedback_findings` reporting feedback attached to TTL-expired
  pages. The store query now filters out expired page targets against the
  current timestamp, preventing `memory_lint` from emitting `feedback_flagged`
  findings for expired pages that are pending forget-sweep deletion.
  ([#1388](https://github.com/akitaonrails/ai-memory/pull/1388))
- Fixed `existing_latest_paths` ignoring page TTL expiration. The query now
  filters out expired pages against the current timestamp, preventing the
  consolidation link guard from keeping wikilinks or typed relations targeting
  expired pages that are pending forget-sweep deletion.
  ([#1387](https://github.com/akitaonrails/ai-memory/pull/1387))
- Fixed `page_belief_inputs` counting contradictions against TTL-expired target
  pages as active live contradictions. When a page declares a `contradicts`
  relation to a target whose `expires_at` TTL has passed, the declaration is
  stale and must not weaken belief strength or reduce confidence in retrieval
  ranking. `contradiction_sql` now applies the `not_expired` guard on the target
  page, aligning belief input aggregation with the lint's resolved/stale split.
  ([#1386](https://github.com/akitaonrails/ai-memory/pull/1386))
- Stopped the LLM contradiction pass from auditing its previous
  `_lint/report.md` as a knowledge page, which could recycle stale findings or
  displace a real page from the bounded prompt. ([#1455](https://github.com/akitaonrails/ai-memory/pull/1455))
- Kept the previous backup or OKF export intact when an archive download is interrupted, and publish completed downloads atomically. Successful downloads replace destination symlinks with private files rather than changing their targets or inheriting their permissions. ([#1381](https://github.com/akitaonrails/ai-memory/pull/1381))
- Read a first-line `.ai-memory.toml` scope key, flag, or `aliases`
  declaration when the marker starts with a UTF-8 BOM, in both native and
  legacy POSIX hook routing. ([#1457](https://github.com/akitaonrails/ai-memory/pull/1457))
- Counted a latest page whose only stored vector belongs to another provider,
  model, or dimension as missing an embedding in `ai-memory status` and the
  web `/health` page, so the count no longer reads zero after an
  embedding-model switch.
  ([#1431](https://github.com/akitaonrails/ai-memory/pull/1431))
- Bounded free-text search before the FTS5 parser sees it. A query of 100k
  distinct terms spent ~17.5 s in query preparation alone (and a ~500 KB
  document ~59 s end to end), and a NUL byte reached the phrase parser as
  end-of-input, surfacing `sqlite: unterminated string` as an internal error
  instead of a result. Query text is now folded (NUL becomes a space) and cut
  to the first 4 KiB on a UTF-8 boundary; ordinary queries are unchanged.
  ([#1434](https://github.com/akitaonrails/ai-memory/issues/1434))
- Fixed generated OKF directory indexes so names containing spaces, Markdown
  punctuation, literal URL delimiters, or Unicode remain readable, clickable
  links in exports and newly generated migration indexes.
  ([#1438](https://github.com/akitaonrails/ai-memory/pull/1438))
- Made the SQLite writer wait out the 5-second busy timeout when another
  process holds the database, such as an external `wal_checkpoint`, instead
  of failing hook ingestion at once with "database is locked".
  ([#1460](https://github.com/akitaonrails/ai-memory/pull/1460))
- Retried the Google, Copilot and Voyage embedders on HTTP 503 as well as 429,
  with the bounded whole-second `Retry-After` policy the OpenAI-style
  embedders already use. A brief provider overload no longer fails backfill
  pages, inline page embeds or query vectors. Voyage previously did not retry
  at all.
  ([#1466](https://github.com/akitaonrails/ai-memory/issues/1466))
- Returned MCP `invalid_params` from `memory_query` for conflicting
  `global`, `scopes`, `workspace`/`project` and `as_of` arguments and for an
  `as_of` that is not an ISO-8601 instant, instead of misreporting them as
  internal server errors.
  ([#1468](https://github.com/akitaonrails/ai-memory/issues/1468))
- Built the web UI's templates without askama's `config` feature, which only
  reads an `askama.toml` that ai-memory does not have. The unmaintained
  `basic-toml` (RUSTSEC-2026-0374) is no longer a dependency, and templates
  render as before. ([#1482](https://github.com/akitaonrails/ai-memory/issues/1482))
- Copied the release into place in `install.ps1` instead of moving it, so an
  antivirus scan that still holds the just-verified `ai-memory.exe` no longer
  stops a Windows install with "the process cannot access the file because it
  is being used by another process".
  ([#1480](https://github.com/akitaonrails/ai-memory/issues/1480))
- Invoked Gemini CLI's native Windows lifecycle commands with PowerShell's
  call operator. Gemini CLI 0.23.0 and later run hook commands through
  PowerShell, where the quoted `ai-memory.exe` path failed to parse, so every
  hook exited with an error and the session captured nothing.
  ([#1484](https://github.com/akitaonrails/ai-memory/issues/1484))

## [3.1.1] - 2026-10-17

### Fixed
- Prevented the Windows executable from overflowing its small default main
  thread stack as the async CLI command tree grows. The top-level dispatch now
  runs on an explicitly sized stack on Windows; the native Windows suite had
  reached the point where every integration test that launched `ai-memory`
  aborted before handling its command.
- Fixed native hook events getting stranded forever when a harness killed the
  process after the spool file was synced but before its atomic rename. A later
  drain adopts complete, valid `*.json.tmp` entries after a five-minute grace
  period and removes malformed old remnants. Claude Code settings installs now
  give `SessionEnd` a 15-second timeout instead of its 1.5-second default budget;
  plugin hooks rely on recovery because Claude does not let plugin timeouts
  raise that budget. (#1377)
- Hardened OIDC credential transport after CodeQL alert #176: the shared
  refresh-grant sink now requires a typed TLS-or-loopback endpoint, and device
  authorization and token polling revalidate their endpoints immediately
  before each request. A caller-mutated discovery object can no longer bypass
  the login-time HTTPS check.
- On Windows, `setup` now starts the logon host with a plain executable path,
  and the host passes plain drive/UNC data-directory and config arguments to the
  supervisor and server it launches, instead of the `\\?\` verbatim prefix a
  canonical path carries. The `Run` value was already plain (3.1.0); this covers
  the command lines built from `current_exe()` and the canonical data dir. This
  path cleanup does not by itself resolve Bitdefender's behavioral detection.
  (#1280)
- Fixed an explicit `llm_base_url` (or `AI_MEMORY_LLM_BASE_URL`) being dropped
  for the `anthropic` and `openai` providers. Config resolved the override for
  every provider, as documented, but the provider factory never applied it to
  those two, so requests and the API key went straight to api.anthropic.com or
  api.openai.com with nothing in the log. A gateway or reverse proxy set in front
  of either vendor now receives the traffic. The docs now name the backends that
  take no override: the ChatGPT backend behind `openai-oauth` and `codex`, and
  `cursor`'s CLI. (#1347)
- Corrected the `ExtractionState::NotApplicable` documentation and aligned the
  generated OpenCode capture policy with the native evaluator: only an active
  policy dropping a search/list tool reports `not-applicable`; an invalid
  policy reports `extracted`. `ai-memory hook --check-capture` also uses
  `not-applicable` when no policy was evaluated. (#1352)
- `ai-memory auth login openai-oauth` and `auth login copilot` no longer print
  a failed endpoint's whole response body. Error bodies are bounded, scrubbed
  of secrets and terminal control sequences, then capped for display; provider
  errors, including OIDC login and refresh, use the same ordering so a secret
  at the display boundary is still redacted. (#1359)
- Fixed OIDC device login and refresh sending credential-bearing requests to
  non-TLS endpoints. Issuers and advertised or persisted device/token endpoints
  must now use `https` outside loopback, and authentication requests do not
  follow redirects. Plain `http` remains available for local test issuers on
  `localhost`, `127.0.0.1` and `::1`. (#1354)
- Fixed `install-instructions` printing a false "Claude Code reads <file> instead
  of AGENTS.md" note when `$HOME` is reached through a symlink. The home
  `.claude/CLAUDE.md` skip now compares with the same canonicalising
  `paths_match` helper the import check uses, so a symlinked home matches its
  resolved path. (#1345)
- Corrected stale claims in `docs/comparison.md` and `docs/competitive-parity.md`
  that said Windows has no service step and the Windows install is experimental.
  The 3.1 `install.ps1` + `setup` Windows service step is shipped production,
  not experimental; the "3.0 (unreleased)" observation-stream note is corrected
  to reflect the released 3.0 line. (#1271)
- Made the executable auto-improvement eval policy server-owned. Manual CLI and
  HTTP reviews now use `[auto_improve.eval]` from the server process, and legacy
  requests that try to supply an `eval` object fail closed with an upgrade
  message.
- Made managed cross-harness continuation survive quota exhaustion without a
  voluntary handoff. The startup packet now places its server-authored resume
  directive outside the untrusted transcript fence, and refreshed
  `CLAUDE.md` / `AGENTS.md` routing tells an incoming agent that "continue",
  "resume", and "pick up where we left off" must consult the private
  `AI_MEMORY_PREVIOUS_SESSION_HISTORY` dossier when the managed-run variables
  are present. The dossier remains untrusted evidence and stays local to the
  launcher machine.
- Fixed unbounded memory use in consolidation and auto-improve for long
  sessions: both paths now read a bounded projection of the session's
  observations instead of loading every row, keeping the LLM input
  byte-identical. Sessions with no observations in the consolidator's target
  scope (purged or foreign) are refused before any work, exactly as empty
  sessions were before. (#1363)
- Fixed the session page's uncapped `## Prompts` list. Above 500 prompts the
  page now lists the first 250 titles and the last 250, numbered by their real
  position, with one line between them saying how many were left out. The
  store reads at most 500 titles for it, and the page's prompt count and the
  digest stats still report the true total. (#1349)
- Fixed auto-improve approvals that failed because a `reject`-policy admission
  webhook was unreachable, timed out, rate-limited or answered 5xx being
  stored as rejections. The proposal is still marked `failed`, but the outage
  no longer enters the rejection buffer, so later reviews are not told to
  avoid a proposal nobody refused and rejection telemetry counts only real
  refusals. (#1358)
- Fixed consolidation persisting non-page `relations:` targets in YAML
  frontmatter on disk. Raw titles containing dots, directory paths, or stemless
  `.md` strings in `relations:` were previously written into frontmatter and
  retained by `prune_relations` when `extract_relation_links` returned an empty
  list, causing the wiki watcher to emit repeated warnings on every
  reconciliation pass. Targets are now validated through `extract_relation_links`
  before frontmatter assembly and during relation pruning. (#1353, #1366)
- Consolidation now also drops well-shaped same-project `relations:` targets
  that name no existing page. Relations among pages in the same atomic batch
  remain valid, and ordinary same-project body wikilinks may still point to a
  page that will be written later. (#1353)
- Verified target latest status and expiration in
  `ReaderPool::contradiction_edges`, so a contradiction pointing at a
  tombstoned or expired page is reported as unresolved instead of active, and
  expired source pages do not emit contradiction findings. (#1367)
- Verified target latest status and expiration in
  `ReaderPool::dangling_cross_project_links`, so cross-project links to
  tombstoned or expired pages are reported as dangling instead of resolved,
  and expired source pages do not emit dangling link findings. (#1368)
- Fixed the `/web` project tree and its recent-activity list, the namespace
  listing, and the `/api/v1` `pages` list and legacy `recent` array showing
  pages whose `expires_at` TTL had passed, which search, `memory_recent`, the
  incremental `recent` form and page history already hid. A project move and
  the curator still see every current page, and an exact-path read of an
  expired page still answers. (#1369)
- Fixed `memory_read_page` returning a page whose `expires_at` TTL had passed
  with nothing to say so. The TTL design keeps an exact-path read answering for
  such a page while `memory_query`, `memory_recent` and the briefing hide it,
  and promised the response would carry `expired: true`; the flag was never
  written. The response now carries `expired: true` for an expired page, on
  both the markdown and the DB-fallback branch, and is unchanged for a live
  one. (#1371)
- Fixed the `/web` page view showing a page whose `expires_at` TTL had passed
  exactly like a live one, although search and the project listings leave such
  a page out. The metadata row now carries an `expired` badge beside the tier
  and kind; a live page renders as before. (#1372)
- Fixed `GET /admin/read-page` and `ai-memory read-page` returning a page whose
  `expires_at` TTL had passed with nothing to say so, although `search` no
  longer lists such a page. The response now carries `expired: true` for an
  expired page, on both the markdown and the DB-fallback branch, and
  `read-page` prints an `expired: true` line (the field also appears in
  `--json`); a live page's output is unchanged. (#1373)
- Fixed web wikilink rendering generating dead links for directory and stemless
  targets. In markdown pages, wikilinks pointing to a directory (`[[notes/]]`) or
  a stemless target (`[[notes/.md]]`, `[[.md]]`) were rewritten into invalid
  page links that yielded 404 responses in the web browser. They now remain as
  literal text, aligning web wikilink preprocessing with the wiki engine's link
  extractor. (#1364)
- `install-mcp` now writes the absolute path of the `ai-memory` binary as the
  `command` of every stdio bridge entry (Claude Code's session-aware entry,
  Muse Code and the session-less bridge of the other clients), as hook
  commands already did. A client started without `~/.local/bin` on its `PATH`
  no longer fails to spawn the bridge. The Docker wrapper keeps the bare
  `ai-memory`, and existing entries with either form are still recognized.
  (#1362)

- Fixed the session-start brief hiding a page's body behind a longer
  frontmatter excerpt. When a pinned, `_rules/` or `_slots/` page had a
  `summary:` or `abstract:` longer than its body, the brief printed that
  excerpt instead of the body, however much of `[briefing] max_chars` was
  left, so a small wiki whose pages fit did not get every body whole. Such a
  body now shows whole: it is priced at its own size when the brief decides
  which pages fit, and the bytes it saves over the excerpt go to other pages.
  A page with an empty body keeps its excerpt. (#1360)
- Fixed `ai-memory setup` wiring OpenCode 2 with OpenCode 1 artifacts. A
  named `--agent opencode` (or `open-code`), or an OpenCode found by the
  session store both majors share, always got the V1 function plugin
  `plugins/ai-memory.ts` and a direct `mcp.ai-memory` entry, which OpenCode
  2 does not load. `setup` now runs `opencode --version` once, as
  `install-mcp` and `install-hooks` already did for the generic spelling, and
  wires the V2 `{ id, setup }` plugin and nested `mcp.servers` entry when it
  reports major 2. A failed probe writes no OpenCode wiring and is reported
  as a setup problem; `--agent opencode2` still forces V2 without probing.
  (#1356)
- Fixed `ai-memory mcp-bridge` exiting when a client sent `server/discover`
  before `initialize`. A client that supports both the 2026-07-28 MCP revision
  and the older `initialize` handshake, such as Antigravity CLI 1.4 and the
  Antigravity 2.24 app, sends that probe first and falls back to `initialize`
  when the server answers with an error. The bridge ended the session instead,
  so the fallback found a closed pipe (`calling initialize: client is closing:
  EOF`) and the client never connected. Until `initialize` arrives, the bridge
  now answers every other request with a JSON-RPC Method not found error,
  ignores notifications such as the client cancelling its probe, and still
  answers `ping`. A client that starts with `initialize` sees no change.
  (#1355)

## [3.1.0] - 2026-10-16

### Added
- Added first-party IBM Bob support. `install-mcp --client ibm-bob --apply`
  (alias `bob`) merges a native Streamable HTTP server into
  `~/.bob/settings/mcp.json`, using Bob's documented `legacy` protocol
  negotiation so it skips the non-standard `server/discover` probe.
  `install-hooks --agent ibm-bob --apply` merges the five events common to
  Bob IDE and Bob Shell into `~/.bob/settings/settings.json`, preserves other
  settings and hooks, enforces capture exclusions over Claude-shaped tool
  payloads, and delivers the pending handoff as plain `SessionStart` context.
  Bob has no true `SessionEnd`, so finished sessions use
  `finalize-session --agent ibm-bob`. Managed transcript resume is not claimed
  without a documented stable native transcript store. (#1331)
- Added host-local native history continuation for managed harness switches.
  When `ai-memory run` moves a workstream from one harness to another, it pins
  the previous native session, validates it against the exact checkout, and
  exposes a private temporary visible-history dossier to the receiving child
  through `AI_MEMORY_PREVIOUS_SESSION_HISTORY`. The startup packet tells the
  agent to consult it for details omitted by the bounded portable ledger. The
  remote server receives no local path or transcript body; same-harness runs
  create nothing, foreign/missing stores fail closed to the portable fallback,
  and the dossier is deleted when the child exits. (#1327)
- Added opt-in `[consolidation] verbatim_residue = true`, which recovers a
  bounded set of literal error lines, paths, URLs, status codes and versions
  from observations omitted or truncated by the consolidation projection.
  The residue is marked as untrusted, grouped and indented so captured text
  cannot forge prompt structure, and never changes stored page content except
  through the existing structured LLM response. The default remains off and
  preserves existing prompt bytes. The eval harness can compare both states
  with `ab --residue` and `projection --residue`. (#1215)
- Grouped interactive `status` and `doctor` reports into sections with semantic
  colors, consistent margins and terminal-width wrapping, retaining every
  diagnostic field and remedy. Redirected, `NO_COLOR`,
  `TERM=dumb` and JSON reports kept their existing stdout format. (#1313)
- Added an opt-in server-rendered management console for users, sessions, and
  personal MCP API keys, using existing authentication and SQLite storage.
  Required the `management-ui` build feature and runtime switch. (#1231)
- Added opt-in session inspection and owned-session purging for signed-in
  browser users, enabled at runtime with `serve --enable-session-management`
  (`AI_MEMORY_ENABLE_SESSION_MANAGEMENT`). A user can purge only a session they
  own in a project they may write; an Admin can purge any. Requested in #1231.
  (#1267)
- Added opt-in self-service `aim_` API keys for signed-in browser users,
  enabled at runtime with `serve --enable-self-service-keys`
  (`AI_MEMORY_ENABLE_SELF_SERVICE_KEYS`). Labels must be 1 to 128 characters
  and a user may hold at most 20 active keys. A self-issued key survives a
  password reset or account disable and must be revoked separately. Requested
  in #1231. (#1268)
- Added `ai-memory search --global`, which searches the wiki pages of every
  project from the terminal instead of only the current one. Each hit shows
  its `workspace/project` in the text output and carries `workspace` and
  `project` fields in `--json`. The flag cannot be combined with
  `--workspace` or `--project`, and a search without it still resolves its
  scope from the current directory. The server side is `GET /admin/search`
  with no `workspace`/`project`: its hits now include `workspace` and
  `project` next to the fields they already had, and the route stays
  root-only on a multi-user server. Global search covers wiki pages only;
  raw observations and a search limited to one workspace are not part of
  this change. (#1253)
- Added kind and tier filters to the web search. The search page in `/web`
  has "Kind" and "Tier" selects, and `/web/search` accepts `kind` and `tier`
  for both a project search and a cross-project one. Every retrieval stream
  applies the filter in its own query, so other kinds cannot crowd out a rare
  one, a filtered search asks the streams for no more candidates than an
  unfiltered one, and the reranker scores only pages the filter keeps. Kind
  and tier both ignore case, a nonstandard kind is flagged on an empty
  result, and "Search all projects" keeps the filters. An unknown `tier` is
  a 400. (#1248)
- Added `ai-memory setup --plan`, which prints the plan and changes nothing,
  and `--plan --json`, which prints it as a JSON document with a `format`
  version for a script or front end to render. The plan now says what each
  detected agent was found by (its session store, its MCP config, or Muse's
  directory), numbers the agents, and lists the other agents `--agent`
  accepts. At the prompt you can type the numbers of the agents to wire
  instead of `y`; a number outside the plan changes nothing. (#1270)
- Added a native Windows path to the two-command install. `install.ps1`,
  published with each release next to its own `.sha256`, checks the release
  zip against that checksum, refuses archive entries that are absolute or
  hold `..`, unpacks it under `%LOCALAPPDATA%\Programs\ai-memory` and adds
  that directory to the user `PATH` (`AI_MEMORY_NO_MODIFY_PATH=1` skips it),
  with no administrator rights. `ai-memory setup` now has a service step on
  Windows: it writes an `ai-memory` value under the per-user `Run` key that
  starts a hidden `ai-memory service-host` at logon, and starts it right away.
  The value is written and read through the registry API, not by running
  `reg.exe`: an unsigned binary running `reg.exe add` on the `Run` key is a
  common persistence signal, and Bitdefender's behavioral engine quarantined
  `ai-memory.exe` for it (#1280). Its paths are written plainly, without the
  `\\?\` prefix a canonical path carries. Stopping the host does not run
  `taskkill` either: `setup` and `uninstall` leave a stop request in the data
  dir, and the host kills the server it started and exits. `taskkill` is
  only the fallback for a host that does not answer within ten seconds (one
  started by an older binary, or a hung one) or a server whose host already
  died.
  The host runs the server with no console window and restarts it after a
  failure, giving up after five failures within ten seconds of starting.
  `ai-memory uninstall --apply` stops the host and removes the value when it
  still has the shape `setup` writes and names this install's data dir, so an
  uninstall against another data dir (a second install, a test's temporary
  one) leaves the entry alone. `ai-memory doctor` and `ai-memory status`
  warn (`logon_binary_missing`, also in their `--json` output and when
  `status` cannot reach the server) when that entry names an
  `ai-memory.exe` that no longer exists, which is what an antivirus
  quarantine leaves behind, and say how to restore the binary or remove the
  entry. (#1271, #1280, #1300, #1311)
- Added `ai-memory setup --web`, which shows the setup plan as a page in
  your browser and applies the agents you tick there by running `ai-memory
  setup --yes --agent ...` and streaming its output to the page. The page is
  served on `127.0.0.1` behind a one-time address printed in the terminal
  (opened in the browser unless `--no-open` or there is no desktop), refuses
  requests with another `Host`, without the address's secret, or, for a
  change, without the page's own `Origin` and CSRF token, and holds no token.
  It stops on Finish, after 15 minutes without a request, or on Ctrl-C. (#1272)
- Added a desktop installer preview, `companions/ai-memory-desktop`: a Tauri
  app that uses an existing `ai-memory` install or copies its bundled one into
  the same per-user location as `install.sh` and `install.ps1`, then shows the
  `ai-memory setup --web` page in its own window. CI builds unsigned Linux,
  macOS and Windows installers on manual dispatch; they are not attached to
  releases until the project signs them. `setup --web` gained a hidden
  `--stop-on-stdin-eof` flag so the page stops when the app does. (#1273)
- A prompt that starts with `reload context` (for example `reload context
  after last compaction, then …`) now puts the stretch of the session that
  its last compaction dropped back in front of the agent: the prompts typed
  between the latest two compactions, verbatim; the last captured reply (with
  the assistant-capture opt-in); and the project's open handoffs, listed
  without claiming them. The new read-only `GET /recap` route builds it from
  captured observations. Claude Code and Codex receive it with that prompt
  through `UserPromptSubmit` `additionalContext`, OpenCode V1/V2 through the
  plugin's system/context hook, and Grok Build on the next `PostToolUse`.
  Serving the recap writes nothing, so a reload never consumes a baton (#1230).
- Added first-party CodeBuddy Code (Tencent) support. `codebuddy` is an
  `AgentKind` (migration V81), so hook events are attributed to it and a
  Claude Code-shaped tool payload (`tool_name`, `tool_input`, `tool_response`)
  is stored as a closed tool observation instead of an empty `other` capture.
  `install-hooks --agent codebuddy` (alias `code-buddy`) merges native
  `ai-memory hook --event … --agent codebuddy` commands for seven events
  (`SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`,
  `PreCompact`, `Stop`, `SessionEnd`) into the `hooks` key of
  `~/.codebuddy/settings.json`, keeps sibling settings and your own hooks,
  and changes nothing on a re-run. There is no script bundle. The
  `SessionStart` hook delivers the pending handoff through
  `hookSpecificOutput.additionalContext`, and a capture-only `SessionStart` or
  `UserPromptSubmit` prints nothing, since CodeBuddy appends that output to
  the session. A `SessionStart` without `cwd` falls back to
  `CODEBUDDY_PROJECT_DIR`. `setup` wires CodeBuddy when
  `~/.codebuddy` exists, `setup-agent --agent codebuddy` prints the hook
  block, and `uninstall` removes the entries. Transcript backfill and
  headless `codebuddy -p` session close are not part of this change.
  Requested in #1299. (#1312)
- Added the `memory_restore_page` MCP tool: without `rev` it lists the git
  checkpoints that changed a page, and with `rev` it writes that version back
  through the wiki write path (sanitizer, admission, slots and supersession
  apply), keeping the checkpoint's frontmatter. Restoring can bring back text
  a root deleted or purged, so both listing and restoring need the same admin
  authority as `/admin/restore-page`. The MCP surface is now 24 tools. (#1307)
- Added an opt-in sweep of old hook event ledgers by age. With `[decay]
  ledger_retention_months = N` (env `AI_MEMORY_DECAY__LEDGER_RETENTION_MONTHS`;
  `0`, the default, disables it; a negative value is rejected at startup), the
  forget sweep deletes a project's `log-YYYY-MM.md` once `N` whole UTC calendar
  months have passed after its month ended. A file qualifies only when its name
  is a rotated ledger **and** its content opens like a ledger, so a page that
  merely shares the name is never deleted; that content gate is repeated after
  the blocking admission round-trip. `log.md`, the current month and the
  previous month are never deleted whatever `N` is. The deletion goes through
  the wiki (`Wiki::delete_event_ledger`, under the mutation guard) and never
  writes the database; a dry run reports `ledgers_prunable` and
  `ledger_bytes_prunable` and deletes nothing, and a real run reports
  `ledgers_pruned`, `ledger_bytes_pruned` and `ledger_page_rows_left` (legacy
  page rows it left alone) in the `forget-sweep` / `memory_forget_sweep`
  report. Each removal consults the blocking admission chain on the new
  `delete_event_ledger` op, the same check a page delete runs: a `reject`-policy
  webhook keeps the file, which the sweep counts in `ledgers_undeleted`, keeps
  going and retries on the next run, and non-blocking observers are told only
  after the unlink. The deletion is permanent on disk; the wiki's git history
  keeps the blobs a checkpoint already captured. (#1322)

### Changed
- `ai-memory setup` now ends by naming the agents to restart and the question
  to ask one of them, "What do you remember about this project?", instead of
  a generic restart line. `install.sh` names the link's full path in its
  `next:` line when `~/.local/bin` is not on `PATH`, so the suggested command
  no longer fails with "command not found". The AUR install and upgrade
  messages, and the README Fedora, Arch and macOS app steps, point to
  `ai-memory setup` instead of the manual `init`, `systemctl` and
  `install-*` commands. (#1270)
- Changed how the opt-in session-start brief (`[briefing]
  inject_on_session_start`) spends `max_chars`. Every pinned, `_rules/` and
  `_slots/` page now gets a short excerpt first, taken from its frontmatter
  `abstract:` or `summary:`, or else from the prose it opens with (read on
  into its first list items). The budget left deepens pages to a longer
  excerpt and then the whole body: pinned and `_slots/` pages first, then
  pages a few bytes short of whole, then the cheapest bodies. Before, the
  brief printed whole bodies in order, so one long `_rules/` page was cut with
  a `[truncated by ...]` note and every page after it became an "omitted"
  path. Bodies are no longer cut: a body that does not fit whole shows its
  excerpt. Four or more unpinned excerpts from one folder share a heading,
  and the omitted list and the closing note point at `memory_read_page`.
  Small wikis whose pages fit still get every body whole. A project with many
  standing pages now sees all of them, fewer in full: pin a page, or give it
  an `abstract:`, to control what it shows. The change adds no marker key
  and no migration. (#1264)

### Fixed
- Restored fresh source installs after crates.io yanked every `zip` release
  compatible with Candle 0.11. Cargo now patches `zip` 8.6.0 to its immutable
  upstream release commit, and `cargo-deny` permits only that repository with
  an exact revision. Locked builds keep the same reviewed code, while
  `cargo install --path crates/ai-memory-cli` can resolve from a clean graph
  again. The patch can be removed when Candle publishes support for `zip` 9.
- Fixed the forget sweep's raw-observation prune deleting the raw capture of
  a session that still has a queued consolidation. A session resumed after its
  first summary gets a new `session_consolidation_jobs` row (`pending` or
  `running`) while its old summary page is still live, so the prune could
  delete the session's raw observations before the queued job read them, and
  the job would consolidate a truncated session. The prune and its dry-run
  count now share one exclusion: a session with a `pending` or `running`
  consolidation job, or resumed capture newer than its last SessionEnd
  generation, keeps every observation, however old; `completed`, `failed` and
  `superseded` jobs do not block after the session is fully ended. (#1324)
- Fixed SessionEnd loading every observation of the session into memory, up to
  three times, on the hook path. One production claude-code session held
  948,136 observations, so ending it deserialized close to a million rows
  (bodies included) per read. The lifecycle-only decision is now one `EXISTS`
  query, and the summary page and the auto-handoff are rendered from bounded
  reads taken in one read transaction: SQL aggregates for the counts and the
  "last of a kind" markers, the 250 first and 250 last rows the raw list
  prints, and early-exit scans for the first or last prompt that passes the
  usual filters; subagent-preamble prompt bodies are read one at a time and
  only until one names the page. The page, the handoff and the lifecycle-only
  decision are unchanged byte for byte (an equivalence test holds them to the
  original row-list code on sessions of every shape). What still grows with
  the session is what the page itself lists: the title of every user prompt.
  The PreCompact and PostCompaction checkpoint page uses the same bounded
  reads. The writer's lifecycle-only re-check and the reader now build their
  query from one shared predicate. (#1323)
- Fixed `ai-memory run --yolo` failing with `401 Unauthorized` after it
  re-ran itself under ai-jail on an install whose bearer token lives in
  `[auth].bearer_token_file`. ai-jail's private home hides the data dir, so
  the jailed run had no token. The outer run now passes the token it resolved,
  and a `server_url` set in `config.toml` or derived from `bind`, into the
  jail through the environment, never the command line. (#1284)
- Fixed every structured LLM call (consolidation, lint, auto-improve and the
  rest) failing with a 400 on Claude Opus 5.5, Sonnet 5.5, Fable 5.1 and
  Mythos 5.1. Those models reject a forced `tool_choice`, which the Anthropic
  provider sent to make the model call its `result` tool. They now get the
  default `auto`, with a tool description that asks for the call, and a reply
  that carries the JSON as text is read as the result. Other Claude models keep
  the forced call. (#1328)
- Fixed `AI_MEMORY_LLM_REASONING_EFFORT=none` failing every Anthropic request
  to Claude Opus 5.5 and Sonnet 5.5 with a 400. Both reject
  `thinking: {"type": "disabled"}`. Opus 5.5 now gets no `thinking` field, as
  Fable and Mythos already did, and Sonnet 5.5 gets
  `thinking: {"type": "between_tools"}`, its way to turn thinking off. (#1326)
- Fixed the Anthropic provider rejecting any response that carried a
  `thinking` or `redacted_thinking` block as an unknown variant. Claude
  Opus 5, Sonnet 5, Haiku 5.5 and the later Opus, Sonnet, Fable and Mythos
  models think when a request leaves `thinking` out, and Claude models from
  4.6 on are sent adaptive thinking once `AI_MEMORY_LLM_REASONING_EFFORT` is
  set, so `memory_explore` and `ai-memory llm-test` failed as soon as the
  model thought. The provider now skips the blocks it does not read. (#1325)

## [3.0.1] - 2026-10-15

### Fixed
- Fixed the WinSW v2.12.0 service-account example using the v3
  `<username>` element. WinSW v2.12.0 expects separate `<domain>` and `<user>`
  elements; it ignored `<username>` and installed the service as
  `LocalSystem`, which cannot safely own a wiki under another user's profile.
  The Windows guide now uses the version-correct XML, grants service logon,
  explains reinstalling an existing service, and verifies its recorded
  `StartName`. (#1321)
- Fixed `/web/health` warning that hook events wait in the local spool during
  every active session. Hooks spool each event first and deliver it at the turn,
  compaction or session boundary, so a few fresh events in the spool are normal,
  and `ai-memory doctor` already reports them as information. The page now warns
  only when an event has failed a delivery attempt, when the oldest event has
  waited 15 minutes or more, or when its age is unknown; a full spool is still
  an error. (#1320)
- Fixed a hook claiming a handoff from the wrong project when an event named
  no directory. A `SessionStart` (or Kimi Code's `UserPromptSubmit`, or Grok's
  first `PostToolUse`) whose payload carried no `cwd` sent `GET /handoff` with
  no scope, so the server resolved its default project or the caller's last
  active one, and the hook consumed that project's handoff. The native hook,
  the shell and PowerShell hook bundles, and the generated OpenCode, OpenCode 2,
  OMP and Pi integrations now fetch no handoff without a directory, for every
  agent; the native hook says so on stderr at session start. The handoff stays
  open for `memory_handoff_accept`. Devin still falls back to
  `DEVIN_PROJECT_DIR`, then the hook's own directory. (#1312)
- Fixed `ai-memory setup` reporting "ai-memory is set up" after it installed
  Codex hooks that Codex had not trusted yet. Codex skips an untrusted hook,
  so Codex sessions went uncaptured. When Codex was wired, `setup` now asks
  Codex for the hooks' trust state, the way `doctor` does, and when any are
  untrusted or modified it names them and says to open Codex, run `/hooks`,
  and trust them. It still exits 0. (#1313)
- Fixed `install-instructions` treating a line that is exactly `@AGENTS.md` as
  proof that Claude Code loads the `AGENTS.md` being written. Claude Code
  resolves `@path` from the file that contains the import, including in the
  middle of a sentence, expands `~/` from the home directory, and ignores
  imports inside code spans and fenced blocks. `@AGENTS.md` in
  `.claude/CLAUDE.md` loads `.claude/AGENTS.md`, a fenced `@AGENTS.md` loads
  nothing, and `@AGENTS.md` in a parent directory loads that directory's file.
  The note now compares the resolved path with the file being written and
  names the import that reaches it. Recursive detection reads at most 256 KiB
  per file and 1 MiB per import graph, and skips oversized or non-UTF-8 files.
  (#1298)
- Fixed Antigravity CLI posting a session start and claiming a handoff on every
  user turn: `invocationNum` restarts at 0 each turn, so only the first turn of
  a conversation (keyed by `conversationId`) now maps to SessionStart, in the
  native, shell and PowerShell hooks. Payloads without a `conversationId` keep
  the previous per-turn behavior. A conversation resumed in a new process does
  not start a new session, so a handoff left meanwhile is read with
  `memory_handoff_list` rather than injected. (#1306)
- Fixed `llm-test --provider anthropic-oauth` reporting Anthropic's refusal of a
  `claude setup-token` token as a plain rate limit: that 429 now carries a hint
  that it is not a real rate limit and names the alternatives, which
  `docs/llm-providers.md` already describes. (#1170)
- Fixed the curator report treating a per-user focus slot
  (`_slots/<operator>/current-focus.md`) as an ordinary slot. It only
  recognised the shared `_slots/current-focus.md`, so with per-user slots the
  focus slot got the 30-day stale threshold instead of the 7-day one and was
  reported three weeks late. (#1314)
- Fixed multi-page consolidation losing every page of a batch when the
  model gave one page a path that cannot be repaired: a leading slash, a
  `..` or `.` segment, or an empty segment. Path validation failed the whole
  session's consolidation, so the valid pages in the same answer were never
  written. That page is now skipped with a warning and the others are
  written, as already happened for paths that fail the portability check.
  (#1315)
- Fixed auto-improve patches losing track of a page's sections after a code
  block that contains a different fence. A line of three backticks inside a
  `~~~` block (or a shorter run inside a longer fence) ended the block early,
  so the headings after it were missed, and a `replace_section` on the
  section before them replaced those sections too. Profile harvesting had the
  same flaw: the code after such a line was read as prose and could be
  harvested as a preference. A fence now closes only on a run of the same
  character that is at least as long, with no more than the three leading
  ASCII spaces CommonMark permits. Four-space, tab and Unicode-whitespace
  pseudo-fences stay code in profile harvesting, section patches and wiki
  link extraction or rewriting. (#1316)
- Fixed the thin-client commands ignoring a custom `bind`, which could write
  into the wrong store. `server_url` defaulted to `http://127.0.0.1:49374`
  and was never derived from `bind`, so `ai-memory --data-dir X status` (and
  `write-page`, `search`, `purge-project`, ...) called the default port
  whatever `bind` said. With nothing listening there the command failed; with
  another server listening there it operated on that server's data, and one
  reporter wrote test pages into their real store this way. When neither
  `config.toml` nor `AI_MEMORY_SERVER_URL` sets `server_url`, it is now
  derived from `bind`: an unspecified address maps to loopback (`0.0.0.0` to
  `127.0.0.1`, `[::]` to `[::1]`) and any other host is used as given,
  hostname included. An explicit value in the file or the environment still
  wins, even when it names the default URL; a `bind` that is not a
  `host:port`, or one on port 0, leaves the default URL alone; and the default
  `bind` resolves to the same URL as before, so an install that never changed
  either key is unaffected. `install-hooks` and `install-mcp` still prefer an
  existing MCP entry's URL, and otherwise write the derived URL instead of the
  default port. The loader now retains that explicit provenance when the file
  deliberately names the default URL. (#1269)
- Fixed a burst of sessions in a directory with no backfill marker launching
  one `backfill --auto` per session. The SessionStart hook now takes an
  atomic in-progress marker before it spawns the backfill, so concurrent
  sessions in that directory start one; the marker is dropped when the worker
  ends and a leftover one stops blocking after 30 minutes. The backfill's
  emptiness check against the server also gives up after 30 seconds instead
  of waiting forever on a saturated server. Keying the trigger on the project
  rather than the directory is not part of this change. Each marker now has an
  unpredictable ownership nonce under an advisory lock, so a delayed worker
  cannot refresh or remove a newer stale takeover. (#1287)
- Fixed `/web/pending` offering Approve and Reject buttons that always failed
  with `401: unknown error` on a server with a bearer token and no password
  sign-in. The browser reaches `/web` there with the root token over HTTP
  Basic, which only admits reads. The page now says so, points to password
  sign-in, and shows each proposal's `ai-memory pending-writes approve` /
  `reject` command instead, separately quoted for POSIX shells and PowerShell;
  stored workspace or project names cannot inject shell syntax. A decision
  that fails for another reason now shows the server's message even when it is
  plain text. (#1288)
- Fixed the managed routing snippet's claim that Claude Code never reads
  `AGENTS.md`. Claude Code 2.1.277 and later reads it when no `CLAUDE.md`,
  `.claude/CLAUDE.md`, or `CLAUDE.local.md` is present, and reads that file
  instead when one exists. The `@AGENTS.md` import is how a canonical
  `AGENTS.md` reaches sessions that read only `CLAUDE.md`. (#1276)
- Fixed bundled hook discovery for a binary installed in a standard Unix
  prefix such as Nix: `install-hooks` and `setup-agent` now probe
  `<prefix>/share/ai-memory/hooks/` from `<prefix>/bin/ai-memory`, and
  `setup-agent` checks it before the fixed `/usr/local/share` and
  `/usr/share` locations. (#1275)
- Fixed `ai-memory install-hooks` and `setup-agent` failing on a second run
  when the staged hook scripts were read-only copies from a read-only package
  store (the Nix store ships them `0444`). Staging now replaces a read-only
  copy instead of erroring. (#1275)
- Fixed generated hook commands baking an absolute `/nix/store/...` binary
  path that Nix garbage collection later deletes. A binary running from the
  Nix store now renders the bare `ai-memory` command, resolved through `PATH`
  when the hook runs. (#1275)
- Fixed the NixOS service lacking `git` on `PATH`: the module now adds
  `pkgs.git` to the unit's `path`. The package binary stays unwrapped so the
  live-process guard can still see a running server. (#1275)
- Fixed Windows capture dropping the body of file and shell tool events
  the client had kept. With `[capture] ignore_paths` active, server
  re-inspection compiled a POSIX never-match sentinel and treated the
  policy as invalid, so a kept Read or Bash became metadata-only. The
  sentinel now follows the event cwd's path flavor. (#1250)
- Fixed `GET /admin/sessions/by-agent`, and so `ai-memory doctor` and
  `ai-memory backfill`, taking minutes on a large project (178 s for 780
  sessions over 185k observations). Without planner statistics SQLite
  answered each session's capture probe from the project-wide index and
  rescanned the whole project once per session; the probe now always uses the
  session index (0.3 s on the same store). The session list and session
  detail reads behind `/web`, `/api/v1` and `memory_read_session_observations`
  ran the same per-session probes and are pinned the same way. (#1257)
- Fixed pages written by an auto-improvement approval, a batch write or a
  checkpoint restore getting no embedding. An approval (through
  `pending-writes approve`, the `memory_auto_improve` tool,
  `POST /admin/auto-improve` or the scheduler), the batch writes behind
  consolidation, the forget sweep, dream and bootstrap, and
  `ai-memory restore-page` wrote and indexed the page but never embedded
  it, so it stayed out of the vector stream of hybrid search until an
  embedding backfill ran, which by default is only at the next server
  start. These paths now embed each written page the same way
  `memory_write_page` does. A provider or embedding-store failure after the
  durable page commit is recorded for backfill but no longer reports the whole
  mutation as failed or prevents later post-commit webhook dispatch. (#1258)
- Fixed `ai-memory run` reporting `server unavailable` for a heartbeat the
  server answered and rejected. A heartbeat is rejected once its managed run
  stops being active, which on a workstation usually means a later `run`
  prepared a workstream and expired every active run whose lease had already
  passed. The launcher printed the outage notice for a failure of any kind and
  sent the real reason to a `debug` line nobody sees at the default level, so a
  server that answered `409 managed run lease is not active` in about a
  millisecond read as an outage. The notice now carries the server's own reason
  for a rejection, and keeps the outage wording only for a server the launcher
  could not reach. A peer that accepts the connection but closes before an HTTP
  response is also classified as unreachable, and the tests no longer race to
  reuse a released ephemeral port. (#1277)
- Fixed `ai-memory run --jail` losing the configured server route inside
  ai-jail's private home. The outer run forwards an explicit or bind-derived
  `server_url` and `base_path`. It does not promote a root bearer read from
  `config.toml` or `bearer_token_file` into the harness environment: that case
  now stops with an actionable error and requires a least-privilege
  `AI_MEMORY_AUTH_TOKEN`, while a token the operator already supplied in the
  environment continues across the existing child-process boundary. (#1284)
- Updated rustls to 0.23.46 for RUSTSEC-2026-0346, -0347, -0348 and -0349,
  in the server and in the importer, relay and wikisync companions, and the
  importer's anyhow to 1.0.104 for RUSTSEC-2026-0190. (#1286)
- Fixed the hook spool delivering nothing after `ai-memory auth enable`
  while it still held entries captured before the switch. Those entries
  carry no credential and sort first, so their batch was refused with `401`
  and the drain stopped there, never posting the entries behind them that
  carry the accepted token. A refused credential now only holds back the
  entries that carry it: they stay queued, the first of them charged once
  per pass, and the drain carries on with the rest. Entries captured with no
  credential are still dropped once their attempt budget runs out; they are
  never resent with the current token, which would attribute them to the
  operator. (#1283)
- Fixed `install-instructions` writing the block into `AGENTS.md` alone, with
  no warning, when Claude Code would never read it. Claude Code reads
  `AGENTS.md` only when no `CLAUDE.md`, `.claude/CLAUDE.md` or
  `CLAUDE.local.md` exists in the directory or above it, so a developer's
  `CLAUDE.local.md` hid the block. The command now names the file Claude Code
  reads instead and how to import `@AGENTS.md` from it. (#1297)
- Fixed `move-session` leaving prompt profile candidates and ingest keys in the
  source project. When a session is moved or re-homed into another project, its
  prompt `profile_candidates` (`source_ref = 'session:<id>'`) and `ingest_keys`
  were not re-stamped into the target project, which distorted cross-project
  profile convergence and caused a subsequent `purge-project` of the source
  project to destroy them via cascade. They now re-stamp in the same transaction. (#1265)
- Fixed the SQLite `-wal` file staying as large as the database after a
  `VACUUM`. `compact`, `reclaim-ledger-versions --compact` and the session,
  project and workspace purges with compaction now run `PRAGMA
  wal_checkpoint(TRUNCATE)` after the rewrite, and the store's writer
  connection sets `journal_size_limit` to 64 MiB. A checkpoint that a reader
  holds back is logged at `warn` and the operation still succeeds. A 29.5 GiB
  database that shrank to 6.3 GiB kept a 17.8 GB WAL, and the next restart
  spent 9 minutes replaying it. `serve` now also drops its read connections
  and checkpoints the WAL on SIGINT, SIGTERM, Ctrl-C or the end of stdio
  input, waiting at most 5 seconds; a read that arrives after that point is
  refused. A hard kill before the checkpoint still leaves the WAL to replay.
  (#1317)
- Fixed the hook event ledgers (`log.md`, `log-YYYY-MM.md`) being committed to
  the wiki's git repository. Every checkpoint after a hook append wrote a new
  whole-file blob of the ledger, and nothing packed them: one install's
  `wiki/.git` reached 3.49 GB, 229 MB per version of a single ledger.
  Checkpoints now leave ledgers out, recognized by name and by content, so an
  ordinary page named like a ledger stays versioned. A ledger an earlier
  version committed is dropped from the git index, never from disk, by the next
  checkpoint. Existing history keeps its old blobs; `docs/lifecycle-ops.md`
  explains how an operator can rewrite it. (#1318)
- Fixed source-built Docker images failing after the plugin renderer began
  embedding the project license. The builder now copies `LICENSE` before
  compiling, and the packaging test guards that compile-time input.

## [3.0.0] - 2026-10-13

### Added
- Added backlinks, outgoing links and an outline to the web page view. A page
  in `/web` now has a side column (below the text on narrow screens) with "On
  this page", the h2 and h3 headings linked to their anchors (shown from two
  headings up); "Cited by", the pages that link to it, with an empty state;
  and "Cites", the pages it links to. Each group lists ten pages and then "and
  N more", and a page in another project carries its `workspace/project`. A
  link from a project the viewer cannot read is left out, as in the
  `/api/v1` page response. (#1246)
- Added ai-memory as a marketplace plugin for Claude Code and Codex. The
  repository is now a plugin marketplace: `claude plugin marketplace add
  akitaonrails/ai-memory` then `claude plugin install ai-memory@ai-memory`,
  or `codex plugin marketplace add akitaonrails/ai-memory` then `codex plugin
  add ai-memory@ai-memory` (Codex runs the hooks only after you trust them in
  `/hooks`; trust survives plugin upgrades). Each plugin carries the
  lifecycle hooks, the MCP tools through `ai-memory mcp-bridge`, the routing
  skills and a `status` skill that runs `ai-memory doctor`. The hooks call
  one short POSIX launcher that hands every event to the installed binary's
  native `ai-memory hook`, so `ignore_paths`, allowlist mode, marker scope
  and server profiles apply exactly as for `install-hooks`; without the
  binary the plugin captures nothing, says so once at session start and
  never blocks the agent. The binary is still installed separately. Catalogs
  pin each plugin to the release tag, and `plugins/` is generated by the
  hidden `ai-memory render-plugins`, with a test that fails when the
  committed files drift from it. When Claude settings-file hooks and the
  plugin are both installed, the settings hooks win and the plugin skips the
  events they deliver, but only a settings hook with a catch-all matcher
  (empty, `*` or absent) counts; sessions with no project folder are not
  captured through the plugin. Droid running the Claude Code plugin gets the
  same treatment: the plugin skips folderless sessions and the events
  `install-hooks --agent droid` already delivers. The Claude Code hooks start
  the launcher with `sh`, a relative `ai_memory_bin` option is ignored, and
  the plugin trees and every `*.sh` are pinned to LF line endings. With no
  server URL set, the launcher omits `--server-url`; a binary older than 3.0
  rejects that, so the launcher retries it once against
  `http://127.0.0.1:49374` and prints an upgrade hint. The plugins need
  ai-memory 3.0 or later. On Windows, Codex calls `ai-memory hook` directly
  without the launcher, so folderless sessions are captured there and a
  missing binary goes unreported; `docs/install.md` records this. `setup` and `ai-memory run` no longer write settings
  hooks, MCP entries or skills for an agent whose plugin is enabled;
  `doctor` reports each plugin, version skew, double wiring and Codex plugin
  hooks Codex skips; `upgrade` also updates the Claude Code plugin;
  `uninstall` removes the plugins. `ai-memory hook --server-url` and
  `ai-memory mcp-bridge --server-url` now treat an empty value as unset and
  use the configured server, and `hook --server-url` may be omitted. Each
  release attaches `ai-memory-claude-plugin.zip` for the Claude desktop app's
  plugin upload. See `docs/install.md#marketplace-plugins`. (#1166)
- Added a recall trace to web search. A project page has its own search box,
  and a search with `workspace` and `project`
  (`/web/search?q=…&workspace=…&project=…`) now ranks with the same hybrid
  ranker as `memory_query` for that project: FTS5, entity, vector, graph and
  observation streams, the authority adjustment, and the reranker when one is
  configured. The server now hands its configured embedder and reranker to the
  web UI, and the reranker's four-call concurrency limit is shared with
  `memory_query`. With "Explain ranking" (`explain=1`) each hit shows its rank,
  raw score and RRF contribution in every stream, the fused score, the
  authority multiplier, and the intent boost and rerank score when present;
  the page also lists the streams that ran, the embedder and reranker in use,
  and each stream's time in milliseconds. A web search without a scope is
  still the cross-project FTS5 search, as `memory_query(global=true)` is.
  Project search follows the per-project read rule: a viewer without a
  grant on a restricted project gets a 403 and no trace, and an unknown project
  is a 404.
- Added a root-only server health page at `/web/health`, linked from the web UI
  header. It shows the hook counters since the server started (accepted, shed
  with 429 by saturation or rate limit, stored, failed, dropped), the writer
  queue depth, the hook spool and recovery journal in the server's data
  directory, the consolidation queue (last completion, pending, running,
  completed and failed jobs), the configured embedder, both providers' status
  and the stored vector identities. Failing checks are listed with the command
  that fixes them, the same lines `ai-memory status` prints: `ai-memory
  recover` for a backed-up spool or journal, `ai-memory compact --confirm` for
  reclaimable space, `ai-memory embed` for pages without vectors, `ai-memory
  embed --force` for vectors made under another model, and the LLM provider's
  retry command. The page is read-only and is gated like `/web/pending`: on a
  multi-user server anonymous requests get a 401 and database users a 403. It
  never shows provider error text, page content or project names.
- Added a two-command native install for Linux and macOS:
  `curl -fsSL https://github.com/akitaonrails/ai-memory/releases/latest/download/install.sh | sh`,
  then `ai-memory setup`. `install.sh` is a new release asset with its own
  `install.sh.sha256`. It downloads the release tarball for the machine, checks
  it against the published `.sha256` file and refuses to install when that file
  is missing, names another asset or does not match, or when the archive holds
  an absolute or `..` path or a symlink or hard-link member. Its `wget`
  fallback for the default GitHub URL runs only with `--https-only` and
  refuses a `wget` that lacks the flag. It unpacks under
  `~/.local/lib/ai-memory` without restoring file ownership, links
  `~/.local/bin/ai-memory`, needs no root, leaves an existing non-link
  `ai-memory` (such as the Docker wrapper) alone, and runs nothing when the
  download is cut short. The new `ai-memory setup [--yes] [--agent <name>]...
  [--no-service]` runs `init` (a fresh data dir gets the root bearer token),
  installs and starts a per-user service rendered from the packaged systemd
  user unit or launchd LaunchAgent with this install's binary, data dir and
  config, wires MCP, hooks and global Agent Skills for each named or detected
  agent, and ends with a read-only check of `/healthz`, `memory_status` and
  `doctor`. The unit names a stable path to the binary (the invoked path, an
  `ai-memory` on `PATH`, or Homebrew's `opt/bin` link) when the resolved
  binary sits in a versioned Homebrew Cellar or `/nix/store` path, so the
  service survives an upgrade or garbage collection. It shows the plan and
  asks first; without a terminal it changes nothing unless given `--yes`. A
  second run changes nothing. Windows has no service step. `ai-memory
  uninstall --apply` stops and removes that service, but only a unit `setup`
  wrote and nobody edited since; any other unit at that path is left alone
  and reported. With `--purge-data`, uninstall refuses a live ai-memory
  process before it stops the service. `INSTALL_FOR_AGENTS.md` tells an AI agent how to install
  ai-memory for its user, and `scripts/onboarding-acceptance.sh` measures the
  path from a clean home to the first returned memory.
- Added page history and session replay to the read-only web UI. Each page
  links to its history (`/web/w/<ws>/<proj>/h/<path>`): every stored version,
  newest first, superseded ones included, so a version a concurrent write
  replaced stays visible, with timestamps and the author of attributed writes,
  plus a server-side unified diff between any two versions (by default the
  latest against the version it replaced). Expired versions are hidden, and a
  page whose TTL has passed shows no history. The same data is available as JSON
  at `GET /api/v1/workspaces/{ws}/projects/{proj}/history/{*path}`. The version
  list is capped at 200 per request, and the diff refuses inputs over 512 KiB,
  10,000 lines or 1,000 changed lines and stops at 2,000 output lines. The
  project page lists recent sessions, and each opens a timeline
  (`/web/w/<ws>/<proj>/s/<session-id>`) that pages through its observations in
  capture order with first/prev/next/last links and a jump field; with
  JavaScript, a slider and `j`/`k` keys step one observation at a time. It uses
  the same readers, owner filter and caps as the `/api/v1` session routes, and
  observations render as escaped text. Both views follow the per-project read
  rule: a non-grantee of a restricted project gets a 403, and a version id or
  session id from another project or workspace reads as not found. The
  `ai-memory-web` crate now has a `clippy.toml` that forbids the `Wiki` write
  and destructive methods, so a write path in the web UI or `/api/v1` fails
  `cargo clippy -D warnings`.
- Added an observation stream to `memory_query` ranking, on by default
  because it raised hit@5 on the retrieval eval for both zero-LLM and
  local-embedding search while staying inside its latency and context-token
  budgets. To keep the previous ranking, set `[retrieval]
  observation_stream = false` (or
  `AI_MEMORY_RETRIEVAL__OBSERVATION_STREAM=false`); ranking is then identical
  to before. A zero-LLM
  session page keeps only a short excerpt of each turn, so the detail a
  question asks about often lives only in the session's observations. With the
  flag on, project and `scopes` searches also run full-text search over
  observations in the same project, and each matching session votes once,
  through its best-matching observation, for its own latest
  `sessions/<id>.md` page as one more RRF stream. A page the stream voted for
  takes its lexical evidence from the stream alone: page FTS over its
  title-only body still reports a rank but adds nothing, so a session that
  only shares topic words in its titles cannot outvote the session holding
  the answer. When page search did not
  match the page itself, the matching excerpt becomes the hit's snippet, and
  `explain=true` lists `observation` in `streams_active` whenever the stream
  runs and reports the matched observation ids. The excerpt and the ids are
  shown only when the caller may read that session's timeline; another
  operator's observations still vote for the shared session page. The stream
  finds the project's observation range with an index seek. Hits are still
  pages and nothing is written; sessions without a page
  keep the `raw_hits` fallback, and `global=true` search is unchanged.
- Added a root-only `GET /admin/metrics` endpoint in Prometheus text format.
  It exposes the hook admission and delivery counters, MCP tool calls split
  into reads and writes, and fixed-bucket latency histograms for `POST /hook`
  and `memory_query`. Every label comes from a fixed set: no query text, page
  body, prompt, client or project name, scope or token appears in the output.
  On a multi-user server only root can read it, like every `/admin/*` route.
- Added a `timings` object to `memory_query` responses with `explain=true`:
  each retrieval stream's wall time in milliseconds (`fts_ms`, `entity_ms`,
  `vector_ms`, `graph_ms`, `observation_ms`, `rerank_ms`), summed over `scopes`.
  Streams that did not run are left out, and a query without `explain` reads
  no clock for them.
- Added a Grok Build CLI first-turn handoff rule. `install-hooks --agent grok
  --apply` and `install-skills --scope global --agent grok` write
  `$GROK_HOME/rules/ai-memory-handoff.md` (into the Grok home the hook file
  was written to), which has the model call `memory_handoff_list` as its first
  tool, through `use_tool` with the qualified key
  `ai-memory__memory_handoff_list`, before reading any file. Grok discards
  SessionStart stdout, so that call is what lets the first PostToolUse claim
  the baton through GET `/handoff`. The rule corrects `workspace` and
  `project` from the marker or git remote when the first list is empty or
  answers with a scope error, and treats the handoff PostToolUse claimed as
  authoritative. A same-name file without the managed marker is kept unless
  `--force`; `uninstall --only hooks` or `--only skills` removes the managed
  rule. (#1228)
- Added the end reason to session-end observations. When a harness's
  `SessionEnd` payload carries a `reason` (Claude Code sends `clear`,
  `logout`, `prompt_input_exit` or `other`), its first line is stored,
  sanitized, as the observation body, so consolidation can tell a normal
  quit from anything else, and the zero-LLM session page closes its summary
  with `ended (<reason>)`. Payloads without a reason are stored as before.
  (#1225)
- Added pending cross-project mail to the web UI. Project cards carry an amber
  `N inbox` badge when messages are waiting for an agent in that project, the
  home page summarises the total and links the projects holding mail, and the
  project page gains a read-only Mailbox section listing up to five of them
  (subject, sender, a snippet, age; "5 of N" when more are waiting), with an
  empty state when there are none. A sender the viewer may not read is shown as
  "a project you cannot read", and the page never pops or cancels a message.
  (#1202)
- Added `install-mcp --client prime-agent`, which registers ai-memory with
  Prime Agent (PrimeIntellect-ai/prime-agent), merging an HTTP entry into the `mcpServers`
  map of `$PRIME_AGENT_CODING_AGENT_DIR/settings.json` (default
  `~/.prime/agent/settings.json`) and preserving other settings and servers.
  With a token the entry names `"bearerTokenEnvVar": "AI_MEMORY_AUTH_TOKEN"`
  and the token is not written; Prime Agent 0.10 counts a user HTTP server as
  connected, and lists it to the model, only with that variable set (or an
  OAuth grant), so export it where you launch `prime-agent`. Without a token
  the server is still callable through `mcp.call_tool("ai-memory", ...)` but is
  not listed. MCP-only: Prime Agent 0.10 has no lifecycle hooks. (#1163)
- Added `expected_page_id` (write only if that is still the page's latest
  version) and `create_only` (write only if the page does not exist) to
  `memory_write_page`, and `expected_page_id` to `memory_delete_page`. A precondition
  that no longer holds fails with `reason: "precondition_failed"` and the
  current version, and changes nothing. `memory_read_page` returns the version
  as `page_id` and `/api/v1` pages as `id`. This is the compare-and-write the
  team-wiki sync needed (#986).
- The `ai-memory-wikisync` companion gained CI and post-merge integration
  (#986, slice 5): `sync --check` writes nothing and exits 0 in sync, 3 on
  drift and 4 on conflicts or refusals (a clone without sync state compares
  the repository with the server directly), and `install-hook` /
  `uninstall-hook` manage a marked git `post-merge` block that runs `sync`
  after each merge: a dry-run report unless `--on-merge apply`, never with
  `--prefer`, and never with a stored token. The cookbook has a GitHub
  Actions recipe. (#986)
- Added delete propagation to the `ai-memory-wikisync` companion with
  `sync --propagate-deletes` (#986, slice 4): a file deleted in the repository
  deletes its server page through `memory_delete_page`, and a page deleted on
  the server deletes its file, when the other side is unchanged since the last
  sync. A delete against an edit is a conflict that `--prefer` resolves, more
  than `--max-deletes` (default 10) refuses the run, and a pinned page is
  deleted only with `--prefer repo`. Every sync write and delete now carries
  the page version it was planned against, so a page changed during the run is
  reported and left alone while the rest apply, and `sync --apply` refuses a
  server older than 3.0. Without the flag deletes are still only reported.
  (#986)
- Changed `ai-memory doctor` to report three silent capture failures: an
  unreachable server (reported instead of aborting the check), a hook spool that is full and
  evicting undelivered events, and installed ai-memory Codex hooks that Codex
  skips because they are untrusted or changed since they were trusted (asked
  of Codex itself through `codex app-server`). `ai-memory status` no longer says
  queued events will be delivered once the spool has started evicting them.
- Added two-way sync to the `ai-memory-wikisync` companion (#986, slice 3):
  `sync` imports repository edits and new pages through the public
  `memory_write_page` MCP tool and exports server edits, comparing each
  page with the last sync. A page changed on both sides is a conflict until
  `--prefer repo` or `--prefer server`; deletes are reported, not synced.
  It is a dry-run without `--apply`, refuses files without frontmatter,
  refuses to overwrite server pages whose metadata a write would clear, and
  re-reads each page right before writing it. An export, or an import's
  rewrite of the repository file, is skipped and reported when that file
  changed during the run, and the run exits non-zero. (#1164)
- Changed `finalize-session --agent antigravity-cli` to replay the session's
  typed prompts from `agy`'s `history.jsonl` before its synthetic session-end, so
  live Antigravity sessions, whose hooks carry no prompt event, get a summary
  page titled by the first prompt, a handoff with "Started/Last", and prompts
  that `memory_query` and `memory_recent` can find. Only lines whose
  conversation and workspace match the session are read; each prompt carries
  the key `backfill` mints for it, so a repeated `--reopen` stores it once and a
  prompt typed after the first finalize arrives with the next one. A checkout
  the capture allowlist does not admit and `--all-owners` runs replay nothing,
  and a missing `history.jsonl` only warns. (#1160)
- Added `copilot` (alias `copilot-cli`) to `ai-memory run`, which manages
  GitHub Copilot CLI as a workstream harness: a fresh session gets a generated
  `--session-id`, a returning one is resumed with `--resume=<id>`, user
  selectors (`--resume`, `--continue`, `--session-id`, `--connect`) win,
  utility subcommands pass through, and `--yolo` maps to Copilot's native
  `--yolo`. Visible messages, tool calls and results, and compaction summaries
  are imported read-only from `$COPILOT_HOME/session-state/<uuid>/events.jsonl`
  (system prompts, hidden reasoning, hook output, and telemetry are excluded),
  and the existing SessionStart hook delivers the workstream context. The
  first managed launch auto-wires Copilot's hooks and MCP, and `doctor` now
  counts its local sessions. Verified against Copilot CLI 1.0.92. (#1040)
- Made GitHub Copilot CLI a first-class routing target:
  `install-skills --agent copilot-cli` (and `install-instructions
  --skills-agent copilot-cli`) writes the managed Agent Skills to the
  repository's `.github/skills` or, globally, to `$COPILOT_HOME/skills`
  (default `~/.copilot/skills`); `memory_install_self_routing` returns
  `copilot_cli` filename and skill-root hints; the routing snippet, the
  routing-install skill, and the MCP instructions name Copilot CLI with
  `AGENTS.md`; and `uninstall` sweeps both Copilot roots, removing only
  marker-bearing ai-memory skills. (#1040)
- Added `ai-memory recover` and a bounded local recovery journal for server
  outages. Offline managed launches now record only uniquely correlated native
  session locators; recovery quarantines overlapping spool entries before exact
  `/hook/batch` transcript replay, journals harnesses without transcript export
  as spool-only without calling their exporter, retains bounded recovery dedup
  keys with their sessions,
  fails closed on terminal drops, and lets only the original owner finish an
  expired unsuperseded managed run through exact idempotent replay. Backfill
  and recovery replay now use reserved `recovery_*` idempotency keys, so
  re-running `backfill --force` over sessions imported by ai-memory ≤2.6.0 (or
  plain-backfilling a recovery-replayed session) can duplicate those older
  observations. Every spool drain path deletes an event the server answered
  `dropped_*` or `resumed` instead of re-sending it each pass, and a hook
  that finds the spool's capacity lock busy waits up to 50 ms and then spools
  the event anyway. (#1125)
- Added `AI_MEMORY_LLM_COMPAT_DISABLE_THINKING` for the `openai-compat`
  provider (opt-in, off by default, ignored by every other provider): when
  set, every chat request carries
  `chat_template_kwargs: {"enable_thinking": false}`, so thinking-capable
  local engines (vLLM / SGLang serving Qwen3-class models) spend the output
  budget on the structured payload instead of a reasoning pass. (#1130)
- Added the `cursor` LLM provider. Consolidation calls the logged-in Cursor
  Agent CLI (`agent --print --mode ask`) so a Cursor subscription can drive
  summaries without an API key. The default model is `cursor-grok-4.6-high`
  ("Grok 4.6"). `AI_MEMORY_CURSOR_AGENT` overrides the binary. The provider never passes
  `--yolo` or `--force`, because the prompt is captured session text, and
  runs `agent` with an allowlisted environment, so it never inherits
  `AI_MEMORY_AUTH_TOKEN` or provider keys. (#1127)
- Added `applies_to` and `enforced_by` arguments to `memory_write_page`, and
  `--applies-to` / `--enforced-by` to `ai-memory write-page`, for profile
  entries. Both fields were documented as profile-entry frontmatter, but no
  tool could set them, so scoping an entry to a stack or keeping it out of the
  digest meant editing the wiki file by hand, and the next tool rewrite of the
  entry dropped the value. A tag the profile cannot detect in any project
  (`typscript`) is refused instead of silently hiding the entry, and both
  arguments are refused on any page the digest does not read: one outside
  `profile/`, or a `profile/` path in an ordinary project. (#1178)
- Added the server version to the footer of every signed-in `/web` page (the
  public login and change-password pages leave it out), on the right
  side opposite "ai-memory · read-only", so you can tell which build a browser
  tab is talking to without running `ai-memory status`. (#1171)
- Added `install-mcp --client dsh` for the DeepSeek Harness (aliases
  `deepseek-harness` and `deepseek_harness`). DSH keeps its MCP client in
  a Cordis profile patch rather than a fixed config file, so the command prints a
  ready-to-merge `- insert:` fragment for `@deepseek-ai/dsh-mcp-client` and
  refuses `--apply`. (#1193)
- Added agentmemory import to the `ai-memory-importer` companion.
  `agentmemory-vault` reads its Obsidian vault export; `agentmemory-export` reads its `ExportData`
  JSON file, from disk only, and needs a mapping file from agentmemory's
  project names (or exact session working directories) to ai-memory
  workspaces and projects. `--apply` refuses to run while any record is
  unmapped. Pages land under `imported/agentmemory/` (`--dest-prefix`), never
  under `sessions/` or another reserved family, and links to pages that were
  not imported become plain text. Every write goes through `memory_write_page`
  with `create_only` or `expected_page_id`, and the importer refuses servers
  older than 3.0. A memory's versions are written oldest first so ai-memory
  keeps the supersession chain, and `forgetAfter` becomes `expires_at`. Each
  page carries a stable UUIDv5 ingest key, so an interrupted import resumes
  without duplicates and a rerun writes nothing. Text is redacted before
  anything is planned or sent. Observations are dropped unless
  `--replay-observations` replays them as `external-import` sessions through
  `/hook/batch`, and records with no ai-memory counterpart are counted in the
  manifest. The cookbook has a "Coming from agentmemory" section.
- Added an agentmemory residue check to `ai-memory doctor`. It reports
  agentmemory configuration left behind after a migration: an MCP server entry in any harness config `install-mcp` knows (or
  in a project's `.mcp.json`, `.cursor/mcp.json`, `.gemini/settings.json` or
  `.vscode/mcp.json`), and agentmemory guideline blocks in rules files such as
  `AGENTS.md`, `GEMINI.md`, `.cursor/rules/` and `.kiro/steering/`. It names
  each file and the key or lines, prints the fix, and warns when a block names
  `memory_consolidate`, which ai-memory also has with different arguments. The
  check only reads; it never edits or deletes another tool's files. `doctor
  --json` carries the findings as `agentmemory_residue`.
- Added `ai-memory auth enable`, which turns on bearer auth for an install
  created without it. It reuses or creates an owner-only token at
  `<data_dir>/auth-tokens/default`, records it in `config.toml`, gives the
  installed hooks the token when they hold no other credential, and
  re-registers every MCP client whose `ai-memory` entry points at this server
  the way `install-mcp` now wires a token-file install. It lists each client
  it changed, each entry for another server it left alone, and anything that
  needs a manual change. Running it again changes nothing, and the token is
  never printed or written into a harness config. Restart the server
  afterwards.
- Added `ai-memory mcp-bridge --no-session-id`, a plain stdio-to-HTTP MCP
  bridge for any client that runs local commands. It authenticates with the
  token from config (`AI_MEMORY_AUTH_TOKEN`, `[auth] bearer_token`, or the
  file `[auth] bearer_token_file` names), forwards no session identity (the
  client passes `workspace` and `project` like any static MCP client, and an
  inherited `CLAUDE_CODE_SESSION_ID` is ignored), and exits at once with
  `401 Unauthorized` when the server refuses it. Without the flag the bridge
  keeps Claude Code's session forwarding and still refuses to start without
  `CLAUDE_CODE_SESSION_ID`.
- Added the `[auth] bearer_token_file` config key (env
  `AI_MEMORY_AUTH__BEARER_TOKEN_FILE`): a file holding the root bearer token,
  read once at startup. A relative path resolves against the data dir.
  `AI_MEMORY_AUTH_TOKEN` still takes precedence; setting the key together with
  `[auth] bearer_token` is a startup error, and a server whose configured file
  is missing or empty refuses to start. A token read from the file is never
  written into a harness config: `install-mcp` registers Claude Code through
  the session-aware `ai-memory mcp-bridge` (Muse Code always gets the bridge,
  as `mcp-bridge --agent muse`), gives Codex and Prime Agent a
  reference to `$AI_MEMORY_AUTH_TOKEN`, and writes every other client a stdio
  entry for `ai-memory mcp-bridge --no-session-id` in that client's own config
  format (OpenCode, OpenCode 2, Cursor, Claude Desktop, Gemini CLI, OpenClaw,
  OMP, Antigravity CLI, Zero, Devin, Grok, Kimi Code, Kiro CLI, Command Code,
  Copilot CLI, Swival, VS Code, Zed; DeepSeek Harness's printed
  patch too). Only ZCode, whose config keeps HTTP entries only, is refused with
  the manual option. `uninstall` recognises these entries.
  `install-hooks --apply` stores the token in the hook credential files only.
- Changed `ai-memory status` and `ai-memory doctor` to show the server's auth
  posture, and for an unauthenticated loopback server print
  `auth: none (loopback)` with the fix, `ai-memory auth enable`.
- Added a server-side drop list for tool events: `[ingest]
  drop_tool_use_id_prefixes` and `drop_tool_names` (both empty by default, at
  most 64 entries of 128 characters). A PreToolUse or PostToolUse event whose
  tool-call id starts with a listed prefix, or whose tool name matches
  exactly, is answered with 202 and counted as a policy drop in `/hook` and
  `/hook/batch`, before it is sanitized, rate-limited or stored. The id and
  name are read from each harness's own fields (Claude Code's `tool_use_id`,
  OpenCode's `callID`, Hermes's `extra.tool_call_id`, and so on). The lists
  apply to every project on the server. (#1241, #1244)
- Added `--project-id` to `purge-project`, `rename-project` and
  `move-project` (and `project_id` to their admin endpoints), as an
  alternative to the project name. A legacy twin row whose name collides with
  another project's `legacy_name` (the empty leftover an older migration
  leaves behind) was refused as ambiguous by name, so it could not be purged
  or renamed. The id is looked up inside the named workspace: sending both or
  neither is a 400, a malformed UUID is a 400, and an id that names no project
  in that workspace is a 404. `list-projects` now prints each project's id so
  an operator can copy it, and `GET /api/v1/projects` rows carry
  `workspace_id` and `project_id` (hidden with the row for a restricted
  project). (#1208, #1226)
- Added `ai-memory project init`, which creates a `.ai-memory.toml` marker at
  the current worktree root (or the current directory outside Git) after
  asking for `workspace` and `project`. The defaults are the scope the CLI and
  hooks already resolve for the checkout, so accepting them keeps the memory
  where it is. It writes only those two keys, needs an interactive terminal,
  and never overwrites an existing marker. (#1227)
- Added first-class Factory Droid CLI support. `install-mcp --client droid`
  (alias `factory-droid`) merges an HTTP entry into `~/.factory/mcp.json`,
  with the URL written literally and a bearer referenced as
  `Bearer ${AI_MEMORY_AUTH_TOKEN}`: Droid expands variables in headers but
  never in the URL.
  `install-hooks --agent droid` merges native
  `ai-memory hook --event … --agent droid` commands for seven events into
  `~/.factory/hooks.json`, keeps your own entries, changes nothing on a
  re-run, and refuses to create that file while `~/.factory/settings.json`
  holds hooks, which Droid would then stop reading. Sessions are recorded as
  agent `droid` (migration V78). The `SessionStart` hook delivers the pending
  handoff, and Droid's own `SessionEnd` closes the session. Droid's `Read`,
  `Create`, `Edit`, `ApplyPatch`, `LS`, `Grep`, `Glob` and `Execute` tools
  map to the file, search and shell families, so `[capture] ignore_paths`
  applies to them: verified against the Droid 0.240.0 tool schemas; no
  tool-call id in Droid payloads. `setup-agent --agent droid` prints the
  hook config and `uninstall` removes both entries. (#1240)
- Added first-class Meta Muse Code support. `install-hooks --agent muse`
  (aliases `muse-code`, `musecode`) merges native
  `ai-memory hook --event … --agent muse` commands for eight events
  (`SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`,
  `PostToolUseFailure`, `PreCompact`, `Stop`, `SessionEnd`) into the `hooks`
  key of `${XDG_CONFIG_HOME:-~/.config}/muse/settings.json`, with an empty
  matcher, a 30-second timeout and the server URL and data dir baked in,
  since Muse runs hooks with a cleared environment. It keeps
  `"schema_version": 1`, every other key and your own hooks, writes a backup
  when it changes the file, and changes nothing on a re-run. Sessions are
  recorded as agent `muse` under Muse's own session id (migration V80). The
  `SessionStart` hook delivers the pending handoff through
  `hookSpecificOutput.additionalContext`, the `SessionStart` and
  `UserPromptSubmit` hooks print nothing when there is nothing to deliver
  (Muse adds that stdout to the model's context), and Muse's own `SessionEnd`
  closes the session. Muse's `read_file`, `write_file`, `edit_file`,
  `apply_patch`, `search`, `bash`, `monitor`, `web_fetch` and memory tools map
  to the file, search and shell families, with a relative `path` resolved
  lexically against `cwd`, so `[capture] ignore_paths` and the server drop
  list apply; Muse's `tool_use_id` is kept as the call id. `SubagentStart` and
  `SubagentStop` are not wired, because Muse fires them every turn for its
  built-in reminder observers. `install-mcp --client muse` now writes a stdio
  `ai-memory mcp-bridge --agent muse` entry with `"mode": "optional"` and the
  install's `--data-dir`, replacing the streamable-HTTP entry, so Muse is a
  session-aware client: the new `mcp-bridge --agent muse` forwards
  `MUSE_SESSION_ID` and reads no other harness's variable (`--agent
  claude-code`, the default, reads only `CLAUDE_CODE_SESSION_ID`). The entry
  goes under `mcpServers`, or under the legacy `mcp_servers` when the file
  already uses it, never both; a file holding both maps, which makes Muse
  ignore every MCP server, is refused and reported by `doctor`. The settings
  path follows `XDG_CONFIG_HOME`. The Claude Code plugin launcher records an
  event as `muse` when Muse runs it (`MUSE_PLUGIN_ID` with `MUSE_SESSION_ID`),
  skipping the events the Muse settings hooks deliver, its subagent events and
  folderless sessions. `setup` detects Muse by its config or data directory,
  `setup-agent --agent muse` prints the hook config, `doctor` reports the Muse
  wiring, `uninstall` removes the hooks and the entry from either map, the
  Docker wrapper forwards `MUSE_SESSION_ID`, and the routing snippet lists
  Muse Code among the `AGENTS.md` harnesses. Verified offline against Muse
  Code 1.4.7 hook payloads, and end to end with the real Muse binary in an
  isolated network namespace against a stand-in model provider: capture of
  all eight events, exclusions on relative and absolute paths, MCP calls
  routed by session, handoff injection and the timeout unit. Not verified:
  interactive `SessionStart` sources and `SessionEnd` reasons, `PreCompact`
  payloads, `apply_patch` and `monitor` input shapes, macOS and Windows paths,
  and the plugin from the marketplace, which Muse Code 1.4.7 refuses to
  install because its `.mcp.json` uses `${user_config.*}` placeholders.
- Added an optional TypeScript client SDK in `companions/ai-memory-ts` for
  programs that are not coding-agent harnesses: custom agents, bots, editor
  and app extensions, scripts and CI jobs. `AiMemoryClient` wraps the page,
  query, status, recent, handoff, message, briefing, explore,
  session-observation and feedback tools, the read-only `/api/v1` routes (with
  cursor paging and `ETag` revalidation) and `GET /identity`, with typed
  results, a typed error taxonomy and scope checks before any request.
  `pages.update` reads a page, applies a change and writes it back guarded by
  `expectedPageId`, keeping the page's metadata. A separate
  `ai-memory-ts/admin` entry point covers checkpoints, page restore and
  session or project purges, which default to a dry run and need both
  `dryRun: false` and `confirm: true` to delete. The SDK has no runtime
  dependencies, needs Node 22 and a 3.0.0 server, and is built from a checkout
  for now; it is not published to npm and is never part of the server's
  tarballs, Docker image, Homebrew, AUR, RPM or Nix packages, which
  `scripts/check-native-packaging.sh` now enforces. CI runs it against the
  server built from the same commit, including a schema drift test that fails
  when an MCP tool changes without an SDK update. See
  `companions/ai-memory-ts/README.md` and `docs/design-client-sdk.md`. (#1165)

### Changed
- Documented the 2.x to 3.0 upgrade in `docs/upgrading-to-3.0.md`: what
  changes, what replaces the removed token commands, the backup, upgrade and
  check steps, and how to roll back by restoring the backup with the 2.x
  binary. README, `docs/install.md` and `docs/MIGRATION-2.0.md` link to it.
- Changed the web UI's project sidebar to link each folder to its namespace
  listing with a page count, and to show only its five most recently updated
  pages with a "View all N" link. The namespace listing now sorts by recent
  update or by title and shows 25 pages at a time with previous and next links
  (`?sort=title&page=2`). (#1247)
- Changed scoped `/api/v1/search` (a single `workspace` + `project`, or
  `scopes`) to rank with the same hybrid ranker as `memory_query` instead of
  FTS5 alone, so it returns the same order as `memory_query`, and `rank` is
  now the fused rank. Cross-project search without a scope is unchanged.
- Documented hand-written MCP recipes for Warp, Cline, Kilo Code, Goose,
  Qwen Code and OpenHuman in `docs/install.md`, and listed them in `docs/support-matrix.md` as `MCP-only (documented)`: each recipe follows
  the client's own documentation at a named version and has not yet been run
  end to end. Aider is not listed because it has no MCP support.
- Published `docs/benchmarks/longmemeval-s-decomposition-2026-10-12.md`, which
  splits the LongMemEval-S retrieval number into capture cost, ranking quality
  and harness artifacts, and records what the observation stream recovered
  (zero-LLM FTS `hit@5` 0.662 → 0.951 and local embeddings 0.800 → 0.945 on
  all 470 questions). The benchmark,
  evals and comparison docs no longer blame the 2 KB excerpt cap for the gap
  (pages holding exactly the captured text score 0.968 against 0.972 for whole
  sessions) and no longer attribute agentmemory's 0.967 to LongMemEval-S: it is
  the grep baseline on agentmemory's in-house corpus, and its LongMemEval-S
  figure is 95.2% from a component harness with the reranker disabled, not
  like-for-like with ai-memory's pipeline numbers. `docs/comparison.md` gains a
  "From agentmemory" migration note.
- Changed `ai-memory init` on a fresh data dir (no config file and no
  database yet) to create a root bearer token at
  `<data_dir>/auth-tokens/default` and record `[auth] bearer_token_file`, so
  a new install's server requires the token on every route except `/healthz`.
  The hooks, the CLI and `ai-memory mcp-bridge` read it from the data dir;
  client commands of a packaged install, whose config lives outside the data
  dir, find it there too when they read no config file and target a loopback
  server. Existing installs are unchanged and keep running without auth until
  `ai-memory auth enable`; so is an `init` that finds a database or a bearer
  supplied another way. `uninstall` keeps this token, since it is the
  server's credential. The Docker wrapper hands a host command (`install-hooks`,
  `mcp-bridge`, `run`) the token of a server initialized on its volume as an
  owner-only file that `install-hooks` copies from the volume, never on a
  command line. The wrapper deletes that host copy when `install-hooks` finds
  no token in the volume and after a successful `uninstall --apply`. With no human accounts the built-in web UI answers a browser
  with a Basic sign-in prompt instead of redirecting to a login form that
  could not succeed: any username, and the token file's contents as the
  password. `ai-memory setup` prints that URL and the file to read, never the
  token. The macOS menu bar app reads the token from its data dir when none is
  saved in Settings, so an install it initialized turns green without pasting
  anything.
- Changed `ai-memory-wikisync` files to carry a small frontmatter (`title`,
  plus `tags`, `pinned` and a non-default `tier` when set) above the body,
  so metadata survives a round trip. The first run after upgrading rewrites
  unedited files of an earlier export in place. (#1164)
- Changed the header search box in `/web` to search the project you are
  viewing. On a project, page, namespace or history view it now sends
  `workspace` and `project` with the query, and its label and placeholder name
  the project. The search page gained a "Search in" selector (`scope=<workspace>/<project>`)
  that lists only the projects the viewer may read, an "in workspace/project ×"
  chip that clears the scope, a breadcrumb, and a "Search all projects" button
  when a scoped search finds nothing. (#1245)

### Removed
- Removed the 1.x compatibility-token commands `ai-memory user add`,
  `user expire`, `user revive` and `user rotate-token`, and their admin
  endpoints `POST /admin/users` and `POST
  /admin/users/{name}/expire`, `/revive` and `/rotate-token`. They were
  deprecated in 2.0. Tokens they issued keep authenticating after the upgrade
  as ordinary API keys with the label `legacy-user-token`: list them with
  `ai-memory api-key list`, replace one with `api-key rotate <id>` and retire
  one with `api-key revoke <id>`. A token expired before the upgrade stays
  refused. Add people with `user add-human` and machine credentials with
  `api-key add`. Migration V79 drops the unused `users.token_hash` and
  `users.token_expired_at` columns and the triggers that mirrored them into
  `api_credentials`, so user records from `/admin/users` and `ai-memory user
  list --json` no longer carry `token_expired_at`, and `user list` no longer
  shows an `expired` status.
- Removed the `OPENCODE_ZEN_BASE_URL` constant from the `ai-memory-llm`
  crate, deprecated since 2.1.0; use `OPENCODE_GO_BASE_URL`, which holds the
  same URL. The `opencode-zen` provider alias is unchanged.

### Fixed
- Fixed SessionEnd consolidation repeating the same expensive prompt on the
  durable queue when the structured LLM response fails deterministically: a
  truncated or empty response, malformed JSON, or an unexpected response
  shape now ends the queue on the first attempt, keeping the heuristic page
  the hook already wrote; pre-send connection and capacity (`503`) failures
  retain the bounded backoff. (#1131)
- Fixed structured LLM responses stopped at the output budget
  (`finish_reason = "length"`) or returned without usable content: they now
  fail with redacted terminal errors, without copying the response. (#1130)

### Security
- Fixed HTTP Basic sign-in to the web UI minting a 30-day `ai_memory_auth`
  cookie that held the root bearer token. Basic sign-in sets no cookie now;
  a cookie set by an older build keeps working until human auth is enabled.
- Fixed `memory_query`, `/web/search` and `/api/v1/search` showing another
  operator's captured observations on a multi-user server. Observation
  excerpts, the observation ids in `explain`, and raw-observation fallback
  hits now come only from sessions the caller may read under the
  session-timeline owner rule. Another operator's observations still rank
  shared session pages.

[Unreleased]: https://github.com/akitaonrails/ai-memory/compare/v3.3.0...HEAD
[3.3.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v3.3.0
[3.2.10]: https://github.com/akitaonrails/ai-memory/releases/tag/v3.2.10
[3.2.9]: https://github.com/akitaonrails/ai-memory/releases/tag/v3.2.9
[3.2.8]: https://github.com/akitaonrails/ai-memory/releases/tag/v3.2.8
[3.2.7]: https://github.com/akitaonrails/ai-memory/releases/tag/v3.2.7
[3.2.6]: https://github.com/akitaonrails/ai-memory/releases/tag/v3.2.6
[3.2.5]: https://github.com/akitaonrails/ai-memory/releases/tag/v3.2.5
[3.2.4]: https://github.com/akitaonrails/ai-memory/releases/tag/v3.2.4
[3.2.3]: https://github.com/akitaonrails/ai-memory/releases/tag/v3.2.3
[3.2.2]: https://github.com/akitaonrails/ai-memory/releases/tag/v3.2.2
[3.2.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v3.2.1
[3.2.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v3.2.0
[3.1.2]: https://github.com/akitaonrails/ai-memory/releases/tag/v3.1.2
[3.1.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v3.1.1
[3.1.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v3.1.0
[3.0.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v3.0.1
[3.0.0]: https://github.com/akitaonrails/ai-memory/compare/v2.6.7...v3.0.0
