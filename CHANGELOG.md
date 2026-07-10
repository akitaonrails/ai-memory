# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed
- Upgraded `crossbeam-epoch` to the RUSTSEC-2026-0204 fixed release.
- Fixed the Docker wrapper on macOS with rootless Docker so host-config
  commands (`install-mcp`, `install-hooks`, `install-instructions`, and
  related setup/removal commands) keep the `-u 0:0` mapping needed to write
  bind-mounted agent configuration as the invoking host user.
- Stabilized the Windows uninstall purge-preview regression test against
  verbatim temp-path spelling.

## [1.11.1] - 2026-07-09

### Fixed
- The documented auto-improvement safety invariant "never rewrite pinned
  pages" is now enforced in code, not just in the reviewer prompt: the
  apply path — shared by manual approval and `require_approval = false`
  auto-apply — refuses `update` proposals whose target page is pinned,
  recording the proposal as a conflict with an explicit reason. Unpinning
  the page is the explicit way to allow a rewrite ([#157]).

### Added
- Architectural-decision guidance ([#157]): the managed durable-pages
  Agent Skill now teaches agents to record architectural decisions as
  pinned wiki pages under `decisions/<slug>.md` with ADR structure
  (Status / Context / Decision / Consequences, rejected alternatives
  included) and to supersede with a new page instead of editing history.
  `docs/usage.md` gains an ADR section clarifying that ai-memory never
  touches repository files (a `docs/adr/` tree managed by hand or by an
  external ADR MCP server is outside its write surface) and how the two
  coexist.

## [1.11.0] - 2026-07-09

### Fixed
- Mid-session events can no longer scatter observations into basename
  "fragment" projects (`sources`, `desktop`, …). The hook router now uses
  session-sticky attribution: when an event's session already exists, its
  observations inherit the session's project instead of re-deriving one
  from the event's cwd — closing the hole left between the v0.12.2
  cwd-prefix guard (which keys on `repo_path`) and the #103 rule that
  non-git parents never record one, which together left plain-directory
  projects unprotected against every mid-session `cd subdir/`. Explicit
  `.ai-memory.toml` overrides still win, and cwd derivation still decides
  for session-creating events.
- V27 data-repair migration re-runs the idempotent V19 repair on upgrade,
  re-attributing the fragment observations that accumulated since V19 to
  their sessions' projects and deleting the emptied fragment rows.
  Reserved projects (`scratch`, `_global`) are exempt.

### Added
- The maintenance scheduler now sweeps "hollow" project rows — zero
  pages, sessions, observations, and handoffs — once they are older than
  seven days, shortly after startup and then daily. Nothing exists to
  lose in a hollow row (probe/rename residue), so the sweep needs no
  config; rows holding any data are never touched, and reserved projects
  are exempt.

## [1.10.1] - 2026-07-09

### Fixed
- Every CLI command panicked at startup on read-only filesystems
  (sandboxes like ai-jail) when the log directory already existed but the
  log *file* couldn't be created — the half of the failure the existing
  tempdir fallback didn't cover, because it only triggered on directory-
  creation errors and used the panicking appender constructor. File
  logging now degrades instead of failing: `<data_dir>/logs` → the OS
  temp dir → stderr-only, with a warning naming the exact path that
  failed at each step (so sandbox users know what to `--rw-map`), and
  commands keep working regardless ([#158]).

## [1.10.0] - 2026-07-09

### Added
- Zero (Gitlawb/zero) is now a supported agent/client ([#156]).
  `install-mcp --client zero` merges a native HTTP + bearer entry into
  Zero's `~/.config/zero/config.json` (`mcp.servers` map), and
  `install-hooks --agent zero --apply` merges exec-form lifecycle hooks
  (the native `ai-memory hook` command with an args array — JSON payload
  on stdin, no shell) into `~/.config/zero/hooks.json`, preserving
  third-party hooks in the same file by id prefix. All six Zero events
  are covered: `sessionStart`/`sessionEnd`, `beforeTool`/`afterTool`,
  and `specialistStart`/`specialistStop` (mapped to ai-memory's subagent
  events). Zero discards `sessionStart` hook stdout, so capture and
  handoff creation work but handoff injection does not — recover
  handoffs via the MCP `memory_handoff_accept` tool, same policy as
  Grok. `uninstall` strips the hook entries and the MCP registration;
  `setup-agent --agent zero` prints the config for docker-host flows.
  Support tracks Zero's current formats — it is a fast-moving young
  project and upstream churn will be fixed as reported.

## [1.9.1] - 2026-07-08

### Fixed
- `memory_read_page`'s input schema now encodes the "exactly one of
  `path` or `query`" contract as an `anyOf` whose branches demand the
  key's presence *and* a non-null string, so MCP clients that null-fill
  defaulted arguments (observed with OpenCode 1.17.x) are schema-blocked
  from sending the neither-arg call instead of looping on a server
  error. The runtime error for a bare call is now instructive — it names
  both arguments and shows a concrete `{"path": "notes/topic.md"}`
  example so a looping model can self-correct — and the tool/argument
  descriptions state the MUST-pass-one rule up front ([#155]).

## [1.9.0] - 2026-07-07

### Added
- Global preferences scope ([#154]): standing user/team context that
  should apply to every project — technology choices, code style,
  durable personal conventions — now has a dedicated home. Write with
  `memory_write_page` + `scope: "global"`; the page lands in the
  reserved `_global` project (default workspace, following the
  `_meta.md`/`_pending/` reserved-name convention). Default-scoped
  `memory_query` calls union that scope into every project as a new
  `global_scope_hits` response field — one extra scoped search, not the
  O(projects) `global=true` fan-out — while explicitly scoped queries
  (`workspace`/`project`/`scopes`/`global=true`) are unchanged. The
  scope participates by existence: no config, zero effect until the
  first global write. Event capture never creates or attributes to it —
  a directory or marker override named `_global` falls back to the
  server-default project.

### Fixed
- A session that is resumed under the same id after an early `SessionEnd`
  now re-consolidates when it ends again. The end-of-session guard used to
  drop any `SessionEnd` for a session with `ended_at` set, so a resumed
  session's page stayed frozen at the first end's content and the resumed
  work lived only in raw observations. The guard now distinguishes a
  duplicate/stale end (no observations newer than `ended_at` — still
  dropped, which keeps `finalize-session` and late spool drains safe) from
  a genuine re-end with new work behind it, which re-runs the full end
  path: heuristic page rewrite, `ended_at` bump, auto-handoff refresh, and
  the opt-in LLM consolidation ([#152]).
- `bin/ai-memory` no longer fails `install-mcp`, `install-hooks`,
  `setup-agent`, `install-instructions`, `install-skills`, `uninstall`,
  and `backup` under rootless Docker. Rootless Docker maps container UID
  0 back to the real
  host user but routes any other UID (including the host UID the wrapper
  always passed via `-u`) through an unrelated subordinate-UID range, so
  every write to a bind-mounted host path (`~/.claude/settings.json`,
  the hook staging dir, `$PWD/CLAUDE.md`, skill directories) failed with
  `Permission denied` or a misleading "does not exist" error. The wrapper
  now detects rootless Docker via `docker info --format
  '{{.SecurityOptions}}'` and runs as `-u 0:0` for just the commands that
  write host-side files; thin-client commands (`status`, `bootstrap`, …)
  are unaffected since they only touch the `/data` named volume.

## [1.8.0] - 2026-07-04

### Added
- OpenCode Zen/Go is now a first-class LLM provider:
  `AI_MEMORY_LLM_PROVIDER=opencode` (alias `opencode-zen`) routes
  consolidation through `https://opencode.ai/zen/go/v1` using the OpenAI
  chat-completions wire format. Authenticate with `OPENCODE_API_KEY`
  (key from `opencode.ai/auth`); the default model is `claude-sonnet-4-6`.
  `ai-memory llm-test --provider opencode` exercises it end-to-end ([#147]).
- `docker/docker-compose.yml` now loads provider credentials from a
  gitignored `docker/.env` via `env_file` (optional — existing deployments
  without that file keep working), replacing the commented-out inline
  provider blocks ([#147]).

### Fixed
- Gemini structured output no longer fails with `400 INVALID_ARGUMENT …
  "type" … Proto field is not repeating, cannot start list` when a schema
  contains an optional field. `prepare_schema_for_gemini` now collapses
  schemars' Draft-2020-12 `type` arrays (e.g. `["string", "null"]` for
  `Option<T>`) into Gemini's single `type` + `nullable: true` form, so
  consolidation / auto-improve work again with `gemini-2.5-pro` and other
  Gemini models.
- The detached drainer's `logs/hook-drain.log` now rotates once it exceeds
  1 MiB (previous contents move to `hook-drain.log.old`), so an agent
  pointed at a chronically unreachable server can no longer grow the log
  without bound.
- Windows hook-spool drain locks now treat native lock-violation responses as
  expected lock contention, so overlapping background drains skip cleanly
  instead of failing the single-flight guard.
- Windows wiki checkpoints now fall back to the Git CLI for native libgit2
  path-resolution failures when reopening freshly initialised repos, keeping
  delete and purge operations usable under dot-prefixed temp or wrapper paths.

### Changed
- Post-audit cleanup, no behavior change: the `AI_MEMORY_HOOK_PLATFORM`
  override is parsed in one place instead of three copies, the CLI
  `AgentChoice` → domain `AgentKind` mapping is a single `kind()` method
  instead of three per-command match blocks, the companion importer crate
  is now gated in CI (fmt/clippy/test), and
  `crates/ai-memory-store/src/auto_improve.rs` is fully documented (its
  file-wide `missing_docs` allowance is gone). `AI_MEMORY_HOOKS_HOST_ROOT`
  is now documented in `docs/install.md`.

## [1.7.1] - 2026-07-02

### Fixed
- Acknowledged the new `quick-xml` RustSec advisories in CI for the existing
  `syntect` transitive dependency bucket. `plist` still constrains `quick-xml`
  below the fixed 0.41.x branch, and ai-memory does not parse untrusted XML in
  this path; the ignores keep cargo-audit/cargo-deny focused on actionable
  advisories until the upstream dependency chain can update.

## [1.7.0] - 2026-07-02

### Added
- Added `ai-memory finalize-session`, a supported manual Codex finalization
  flow. It defaults to the latest open Codex session in the current
  workspace/project and posts a synthetic `session-end` hook so summaries,
  handoffs, and auto-improvement eligibility use the canonical SessionEnd path.

### Fixed
- Native hook spool delivery no longer relies only on the cancellation-prone
  `session-end` hook to start the detached drainer. `stop` and `pre-compact`
  also request the background `hook-drain` helper after enqueue, and Unix builds
  use a trusted `setsid` launcher when available before falling back to a
  separate process group.
- OpenCode generated plugins now close sessions from the official
  `session.deleted` event and a deduped best-effort `dispose` fallback, so
  OpenCode sessions can produce automatic session summaries and handoffs without
  duplicate `session-end` emissions.

## [1.6.0] - 2026-07-01

### Fixed
- Documented and regression-tested that `install-instructions` updates only the
  ai-memory marker block, preserves unrelated CLAUDE.md / AGENTS.md content,
  writes backups for existing files, and refuses unmanaged same-name skills
  unless explicitly forced.
- Claude Code WindowsNative hook installs now use Claude's exec form
  (`command` executable plus `args` argv array) for the native `ai-memory.exe`
  hook, avoiding shell/Git Bash/PowerShell command-string mangling. Set
  `AI_MEMORY_HOOK_PLATFORM=windows-bash` before `install-hooks` as a fallback for
  older Claude Code builds; exec form requires a real `.exe`, not `.cmd`/`.bat`
  shims.
- Native `session-end` hooks now enqueue and return quickly, then drain the hook
  spool through a hidden detached `hook-drain` process guarded by a real
  single-flight file lock. Background drains use the new bounded
  `AI_MEMORY_HOOK_BACKGROUND_DRAIN_BUDGET_MINUTES` setting (default 5, max 60),
  while `session-start` cleanup remains synchronous and uses one shared
  `AI_MEMORY_HOOK_START_BUDGET_MINUTES` budget for lock wait plus cleanup drain.
  This supersedes the previous inline `session-end` deferred-drain note and
  `AI_MEMORY_HOOK_END_BUDGET_MINUTES` session-end flush budget.
- Pi is now supported through a generated `~/.pi/agent/extensions/ai-memory.ts`
  TypeScript extension that combines lifecycle capture with an HTTP MCP bridge;
  `install-hooks --agent pi --apply` writes it, while `install-mcp --client pi`
  prints bridge guidance instead of writing an ignored native `mcp.json`.
- Generated OpenCode and OMP TypeScript lifecycle hooks now buffer capture
  posts through a bounded best-effort queue instead of spawning one unbounded
  fetch per event, reducing client-side request bursts while preserving direct
  handoff fetches.
- Corrected the Pi vs Oh My Pi / OMP install split: OMP remains supported via
  `--client omp` / `--agent omp` (or `oh-my-pi`) and writes `.omp` config, while
  real `pi` remains a separate install surface. Users who previously used `pi`
  to mean OMP should switch to `omp` or `oh-my-pi`.

## [1.5.0] - 2026-07-01

### Added
- Per-project `drop_subagent_captures` opt-in. A project sets
  `drop_subagent_captures = "true"` in its `.ai-memory.toml`; the host-side hook
  forwards it (as the `drop_subagent` query flag, alongside the existing
  `workspace`/`project`/`project_strategy` marker fields) so the ingest router
  **accepts but does not persist** that project's subagent-session captures,
  keeping only top-level sessions. A multi-agent harness fans one goal out to
  many subagent sessions, each firing lifecycle hooks; on a small shared
  instance that flood can saturate ingest and bloat the store. Scoping the
  opt-in to the project that asked for it avoids a server-global switch that
  would shed subagent captures for every project on the instance. Captures are
  accepted (HTTP 202 / counted in the `/hook/batch` ack) so clients do not retry
  or spool them, but they are not stored. Detection combines a per-event marker
  (`subagentType` for grok, `agent_type`/`agent_id` for Claude Code) with
  stateful, bounded tracking of subagent session ids: the router seeds the set
  from any marked event and from the newly registered
  `SubagentStart`/`SubagentStop` lifecycle hooks (claude-code and grok), and
  clears it on `SubagentStop`, so the unmarked tail of a subagent session (its
  `user_prompt_submit`/`stop`/`session_end`, which carry no marker) is dropped
  too — not just the marker-bearing tool-use events.

### Fixed
- Native Windows/Git Bash installs now normalize hook cwd, stored project
  `repo_path`, and the home-directory guard consistently across slash styles,
  including legacy rows persisted with backslashes. `AI_MEMORY_HOME` now feeds
  the same home guard as `$HOME`, and Git-backed helpers preserve bare-repo
  fallback semantics while limiting CLI git fallbacks to path/open failures.

## [1.4.1] - 2026-06-28

## [1.4.0] - 2026-06-26

### Changed
- `install-instructions` now refreshes a slim markered CLAUDE.md/AGENTS.md
  snippet and installs or updates managed ai-memory Agent Skills by default,
  with `--no-skills` for snippet-only refreshes and `--skills-*` flags for
  scope, agent family, target root, and forced unmanaged replacement. Added
  `install-skills` for refreshing those prompt-packaging skills directly, and
  `memory_install_self_routing` now returns the slim block, managed skill
  payloads, target hints, and overwrite guidance for agents that install
  routing through MCP.

### Added
- The native `session-end` hook now emits a one-line stderr note when the spool
  drain leaves events queued for a later boundary: it reports how many events
  were flushed, how many remain queued, whether any events were dropped as
  undeliverable, and names the knobs to bound the backlog
  (`AI_MEMORY_HOOK_END_BUDGET_MINUTES`, `AI_MEMORY_HOOK_INCREMENTAL_THRESHOLD`),
  turning an otherwise silent, scary cancelled-hook symptom into an actionable,
  self-documenting message. A fully-drained session stays silent. (#130)
- `ai-memory uninstall --only skills` now removes ai-memory-managed Agent Skill
  files from the default project/global `.claude/skills` and `.agents/skills`
  roots after marker validation; custom `--target-dir` skill roots remain a
  manual cleanup path.

### Changed
- Agent-facing routing prompts and auto-scope docs now call out that static MCP
  clients running parallel sessions need explicit scope arguments or a
  session-aware bridge that forwards the real lifecycle-hook session id.

### Fixed
- Thin-client HTTP CLI commands (`status`, `search`, `read-page`, `write-page`,
  `delete-page`, `backup`, `embed`, and related admin commands) now fall back to
  a stored OIDC device-flow token from `auth.json` when `AI_MEMORY_AUTH_TOKEN` /
  `[auth].bearer_token` is absent. This sends a bearer for external OIDC-aware
  gateways/bridges; native ai-memory server auth still uses the static root
  bearer or DB-user tokens, and `/admin/*` remains root-only unless a gateway
  translates accepted OIDC auth into upstream auth that ai-memory accepts.
  Static bearer tokens still take precedence.

## [1.3.0] - 2026-06-24

### Added
- `install-hooks --project-strategy repo-root` bakes a default project strategy
  into the generated hooks, so every session resolves its project from the main
  git repo root (collapsing subdirectories and worktrees) without a per-repo
  `.ai-memory.toml` marker — preventing a persistent `cd` into a subdirectory
  from forking memory into a phantom project. A marker's own `project_strategy`
  still takes precedence, and the default (`basename`) bakes nothing, so existing
  installs are unchanged. Covers every delivery path: POSIX/PowerShell hook
  scripts, the native `hook` command, and the OpenCode / OMP / OpenClaw
  TypeScript integrations. (#128)

## [1.2.2] - 2026-06-23

### Fixed
- Long-session consolidation now favors later same-session corrections when the
  observation projection cap forces sampling, and both consolidation prompts now
  instruct the model to treat the most recent/final state as authoritative when
  observations contradict earlier drafts.

## [1.2.1] - 2026-06-23

### Fixed
- Hook spool batch drains now scale the `/hook/batch` request timeout with the
  number of events in the chunk, reducing false timeout retries after a slow
  server has successfully committed the batch.
- Hook spool filenames now include a per-process monotonic suffix so tight loops
  or long-lived helper processes cannot overwrite events created in the same
  millisecond.
- Contamination audits now treat `%` and `_` in stored `repo_path` values as
  literal path bytes, matching runtime cwd-prefix resolution.
- Auto-improve telemetry rejection aggregates now exclude rejected maintenance
  report proposals (`curator_report` and `auto_improve_report`) from learning
  rejection signals.
- Auto-improve eval stdout capping now reads only `MAX+1` bytes before failing
  closed, avoiding a flaky oversized-output test path while preserving the cap.
- Updated `quinn-proto` to clear the new RustSec memory-exhaustion advisory.

## [1.2.0] - 2026-06-23

### Added
- Added the standalone optional `companions/ai-memory-importer` package for
  dry-run-by-default OMC flat Markdown wiki imports through public HTTP APIs;
  it is isolated from the root Cargo workspace and root `cargo test --workspace`.
- Auto-improvement reviews can now stage bounded patch proposals for existing
  `_rules/` and `procedures/` pages using append, add-section, and checked
  replace-section edits, with base-body hashes guarding materialize-to-stage
  races.
- Auto-improvement patch proposals now honor a per-run edit budget, and final
  `_rules/` / `procedures/` pages have configurable token budgets to prevent
  reviewer runs from growing policy or procedure pages too aggressively.
- Auto-improvement now keeps a scoped rejection buffer for human rejects,
  approval conflicts/failures, and validator rejected candidates, then feeds a
  bounded summary into future reviewer prompts to avoid repeated failed edits.
- Auto-improvement can now run an optional operator-supplied executable eval gate
  under `[auto_improve.eval]` after LLM validation and before staging/approval for
  selected targets (default `_rules` and `procedures`). The gate is disabled by
  default, receives proposal JSON on stdin, fails closed on command/timeout/JSON
  errors or insufficient score delta, and records eval failures as rejected
  candidates without running from hook paths.
- Added read-only auto-improvement telemetry reporting via
  `POST /admin/auto-improve/report` and `ai-memory auto-improve-report`, with
  JSON and human CLI output for recent run counts, proposal outcomes, terminal
  rates, and operational findings without staging pending proposals; optional
  `--stage` / `stage: true` creates one pending audit report page for approval.
- Added `docs/auto-improve-eval-gates.md` plus dependency-free Python and shell
  scorer templates for `[auto_improve.eval]` proposal gates.
- Hook spool drains now use `POST /hook/batch` when the server supports it,
  grouping compatible queued lifecycle events into bounded batches to amortize
  remote request latency. Older servers fall back to the existing per-event
  `POST /hook` path.

### Fixed
- Auto-improvement eval gates now apply timeouts to the full child interaction,
  cap stdout at 64 KiB, cap eval rejection evidence, and make direct root admin
  requests inherit server eval defaults unless the request explicitly overrides
  them.
- Hook project routing now stores git repo paths using the incoming cwd's visible
  path spelling, so macOS `/var` vs `/private/var` aliases and symlinked cwd
  aliases still prefix-match later events into the same project.
- Updated `git2` to the patched 0.21 line to clear new RustSec unsoundness
  advisories in libgit2 bindings.
- Native Claude Code hooks now capture array-shaped `tool_response` content and
  recognize the native `user-prompt-submit` event token, restoring prompt text
  and tool output bodies for native installs.
- The headless hook spool now warns (instead of silently swallowing) when it
  cannot persist a bumped retry-attempt count back to disk, matching the v1.1.3
  enqueue-failure fix. A failed atomic rewrite previously lost the in-memory
  attempt bump, so a poison spool entry never reached `MAX_ATTEMPTS` and kept
  retrying on every drain boundary with no operator signal until the 7-day
  age-out pruned it. The warning is sanitized and carries no path (a raw spool
  path can be a Windows verbatim `\\?\…` path).

## [1.1.3] - 2026-06-20

### Fixed
- Native Windows Claude Code hooks no longer silently drop every captured event.
  On Windows, `install-hooks` canonicalizes the data dir, which yields a verbatim
  extended-length path (`\\?\C:\…`), and that prefix was baked verbatim into the
  generated `--data-dir` for each native hook command. At capture time the
  hook-spool write under a `\\?\`-prefixed data dir never lands, and the failure
  was swallowed (`let _ = enqueue(...)`), so every `UserPromptSubmit` /
  `PreToolUse` / `PostToolUse` / `PreCompact` / `Stop` / `SessionEnd` event was
  lost while each hook still exited 0 — only `SessionStart` (handoff retrieval)
  kept working, masking the loss. The data dir is now de-verbatim'd both when
  rendering the hook command (new installs emit a plain `--data-dir`) and when
  the hook resolves its data dir at capture time (an already-installed hook
  recovers on the next session without re-running `install-hooks`), and future
  spool enqueue failures emit a sanitized stderr warning instead of staying fully
  silent. (issue #116)
- `bin/release` now handles changelog entries containing backslashes, so Windows
  path examples cannot abort the release after the version files are updated.

## [1.1.2] - 2026-06-19

### Added
- Documented a migration checklist for users replacing another memory tool:
  export first, scrub secrets, curate legacy material into reviewed Markdown,
  configure one client at a time, and remove stale hooks/plugins/MCP servers only
  after ai-memory capture and retrieval are verified. (issue #115)

### Fixed
- Handoff selection no longer strands a detailed manual handoff behind a vague
  auto one. A manual handoff (`memory_handoff_begin`) typically has no cwd while
  the SessionEnd auto handoff carries the session cwd; the read filtered by exact
  `cwd` equality, so the next session — whose SessionStart hook always sends a
  cwd — silently skipped the cwd-less manual handoff and consumed the cwd-bearing
  auto one instead, leaving the manual one open but unreachable (handoffs have no
  list/search surface). Selection now prefers a manual handoff over an auto one
  deterministically: `memory_handoff_begin` always stores a null
  `from_session_id` and the SessionEnd auto handoff always a non-null one, so
  manual handoffs are treated as project-wide (always candidates, whatever cwd
  they carry) and an explicit baton beats the heuristic one regardless of whether
  the model passed a cwd. Among manual handoffs the most recent wins. Auto
  handoffs are scoped by a cwd path-boundary (a handoff left in `/repo` reaches a
  session in `/repo/api`, never `/repo-other`), with the most specific cwd then
  the most recent as tiebreaks — the cwd-specificity tiebreak applies only to
  auto handoffs, never reordering manual ones. The path-boundary is computed in
  Rust, not SQL `LIKE`, so `%`/`_` in a path cannot act as wildcards.
  The stored cwd is normalized (trailing slash stripped) at insert time so
  trailing-slash drift between agent payloads cannot break the match.
  Cross-project isolation is unchanged: handoffs remain scoped by `workspace_id`
  + `project_id`.

## [1.1.1] - 2026-06-18

### Fixed
- Internal consolidation and auto-improvement prompts now use a deterministic,
  budgeted observation projection that preserves high-signal anchors instead of
  suffix-only observation windows. Manual handoff fields and item lists are
  capped after sanitizer scrub with visible truncation markers so oversized
  handoffs do not overwhelm the next agent context; raw observations remain
  unchanged.
- `project_strategy = "repo-root"` no longer falls back to `basename(cwd)` for
  git worktrees whose directory lives outside the main repo tree when the server
  runs in a container. The server resolves repo-root via libgit2 on the incoming
  `cwd`, which fails when that host path is not visible inside the container, so
  every such worktree became its own project. Lifecycle hooks and generated
  TypeScript plugins now resolve the main repo root host-side — following the
  worktree commondir pointer with `git rev-parse --git-common-dir` (or the same
  Rust/libgit2 helper for native hooks) — and send it as an explicit `project`,
  so linked worktrees collapse to one stable project regardless of where the
  worktree directory lives or how the server is deployed. The hook-side git
  probe is silent outside real work trees, preserving the existing basename
  fallback there. (issue #110)
- Unscoped MCP queries could resolve to the wrong project on a shared
  install. The cwd-to-project resolver recorded the bare working directory
  as a project's `repo_path` whenever the cwd was not inside a git repo
  (which, under the default basename strategy, was always), so opening a
  session in a broad ancestor such as `$HOME` created a row that
  prefix-matched every project nested beneath it and captured their unscoped
  lookups. A project's `repo_path` is now the git working-tree root, or unset
  when the cwd is not inside a git repo, never the bare cwd; under the
  default basename strategy it is recorded only when the cwd is the
  repository root, never a subdirectory. A read-time guard additionally
  refuses to prefix-match a stored `repo_path` equal to the operator's
  `$HOME`, and `ai-memory serve` heals existing installs on startup by
  clearing any `repo_path` that is not a real git working-tree root (such as
  a legacy `~/projects` or `/work` catch-all), not only `$HOME` and the
  filesystem root, while leaving paths it cannot see locally (a remote or
  multi-user client path, or an unmounted drive) untouched. (issue #103)
- macOS Docker quick-start now produces a working native-agent setup out of the
  box (issue #107). Three independent breakages are fixed: (1) the macOS wrapper
  baked the container-only `host.docker.internal` URL into the *host* agent
  config, so MCP and every capture hook failed silently — `install-mcp`,
  `install-hooks`, and `setup-agent` now render the host-reachable
  `http://127.0.0.1:49374`, decoupled from the `host.docker.internal` URL the
  wrapper still uses for its own in-container thin-client commands; (2) those
  thin-client commands were rejected with `403 forbidden host` because the
  server's loopback-only Host allowlist excluded `host.docker.internal` — the
  Docker image now ships it in the default `AI_MEMORY_ALLOWED_HOSTS` (native
  installs stay loopback-only; exposed deployments still override it); and (3)
  `setup-agent`/`install-hooks` could not locate the hooks bundle that ships
  beside the binary in the release tarball (the probe derived a bogus
  `/private/hooks/…`), so the binary-sibling `hooks/` directory is now on the
  discovery search path and `--source` is no longer required.
- Wiki reindex and checkpoint restore now preserve page `tier` and `pinned`
  metadata from frontmatter instead of forcing every reindexed page back to
  semantic/unpinned. Wiki writes now serialize canonical tier/pinned metadata so
  later watcher reconciliation remains idempotent for episodic and pinned pages.

### Added
- `GET /admin/audit-contamination` (and `ai-memory audit-contamination`) — a
  read-only, SQL-only structural cross-project contamination audit. Flags
  sessions whose `cwd` longest-prefix-resolves to a different project than the
  one they landed in (the auto-scope-bleed signature, resolved with the same
  prefix logic the runtime uses) and observations whose project disagrees with
  their owning session (a regression tripwire that should stay empty on a
  healthy DB). Optional `?workspace=&project=` scope; reports only, never
  mutates, so it is safe to run on any cadence. Purely semantic mislandings
  (no cwd/session anomaly) are out of scope by design.
- Mid-session hook-spool drain: on `post-tool-use`, once the local spool backlog
  crosses a threshold, the hook runs a tightly time-boxed (~250 ms) catch-up
  drain so a heavy session keeps the backlog flat instead of waiting for the next
  session boundary. Tunable via `AI_MEMORY_HOOK_INCREMENTAL_THRESHOLD`
  (default 32 events).
- Added [`docs/macos.md`](docs/macos.md) covering macOS install paths (prebuilt
  release binary, source build, and the Docker wrapper) and the `posix` vs
  `posix-native` hook platform split, with troubleshooting notes for macOS
  wrapper and hook-discovery issues. Linked it from the README support matrix, the docs
  table, and `docs/install.md`, and bundled it into the macOS release tarballs
  alongside `docs/install.md` (mirroring how the Windows zip ships
  `docs/windows.md`).

### Fixed
- Bounded heuristic session-page raw observation dumps and single-page
  consolidation prompts so very large sessions cannot re-include unbounded
  `## Raw observations` history (issue #102).
- Hook spool no longer counts a server `429` (saturation / `hook queue full`)
  against a spooled event's `MAX_ATTEMPTS` retry budget: transient backpressure
  keeps the event queued without burning an attempt (`MAX_AGE_MS` still bounds
  it), so a saturation burst no longer silently discards real observations.

## [1.1.0] - 2026-06-16

### Fixed
- Auto-improvement scheduling now scans every known project each tick instead of
  only the server's startup/default project. Scheduler ticks remain
  non-overlapping; if reviewing all projects takes longer than the configured
  interval, the next tick is delayed until the current tick finishes.

## [1.0.11] - 2026-06-15

### Added
- Added a server-side auto-improvement scheduler. When an LLM provider is
  configured, `[auto_improve.scheduler] enabled = true` reviews newly completed
  sessions in the background, stages validated proposals for audit, and then
  follows the normal auto-improvement approval policy.
- Added persistent scheduler state and per-session scheduler claims so upgrades
  from the v1.0.3-era schema do not process historical session backlog
  automatically, and failed scheduled reviews do not retry forever. Manual
  `ai-memory auto-improve --session-id <uuid>` and MCP `memory_auto_improve`
  remain the catch-up path for older or failed scheduled sessions.

### Changed
- Clarified that scheduling and approval are separate: disable automatic review
  with `[auto_improve.scheduler] enabled = false`; keep proposals pending with
  `[auto_improve] require_approval = true`.

## [1.0.10] - 2026-06-15

### Changed
- `ai-memory auto-improve --session-id <uuid>` and `POST /admin/auto-improve`
  now record validated proposals in the pending-writes audit trail and approve
  them immediately through the normal wiki write path. Set
  `[auto_improve] require_approval = true` to keep proposals pending for manual
  review. The MCP `memory_auto_improve` tool uses the same behavior.
- Existing project wikis need no migration. Existing server configs that
  contain the old `[auto_improve] mode = ...` key keep working; the key is now
  ignored and can be removed when convenient.

## [1.0.9] - 2026-06-15

### Fixed
- Clarified architecture and auto-improvement design docs so CLI/admin
  auto-improvement is described with pending-writes storage and approval
  boundaries.

## [1.0.8] - 2026-06-15

### Added
- Added auto-improvement proposal storage: `ai-memory auto-improve` and
  `POST /admin/auto-improve` store validated proposals in durable SQLite-backed
  pending-write rows with non-indexed `_pending/auto-improve/` sidecars. The new
  `ai-memory pending-writes list|show|diff|approve|reject` commands and
  `/admin/pending-writes*` routes let operators review, apply, or reject stored
  proposal bodies through the normal wiki mutation path.
- Added first-release curator support: `ai-memory curator` and
  `POST /admin/curator` run a rule-based, no-LLM, report-only maintenance
  review for an existing workspace/project. Dry-run is the default and writes
  nothing; `--stage` creates exactly one pending report proposal for approval
  through the existing pending-writes queue, without editing pages, deleting
  content, rewriting links, or changing slots.

## [1.0.7] - 2026-06-15

### Added
- Added the initial auto-improvement reviewer: `ai-memory auto-improve
  --session-id <uuid>` calls `POST /admin/auto-improve`, reads one completed
  session, applies preflight noise filters, samples large sessions via the
  consolidated session page plus high-signal observations, asks the configured
  LLM for structured durable wiki edit proposals, and validates
  path/evidence/confidence plus duplicate existing path/title constraints.
  Thresholds remain configurable: confidence, input budget, proposal cap,
  `auto_improve` attribution, and `_pending/auto-improve` proposal path.
- Added MCP tool `memory_auto_improve` so agents can run learning review for the
  latest completed session (or a named session) without shelling out. The
  canonical MCP instructions and installed CLAUDE.md/AGENTS.md routing snippet
  now teach agents to treat `_rules/`, `gotchas/`, `procedures/`, and
  `decisions/` as actionable guidance for proactive retrieval.

### Changed
- Clarified upgrade guidance: Docker-wrapper users should run
  `ai-memory upgrade` on each agent machine to refresh the wrapper, pulled
  image, and staged hook scripts; native package/source installs should rerun
  `install-hooks --apply` after binary upgrades; remote servers still need a
  separate redeploy; and existing projects can refresh the managed routing block
  to pick up new proactive retrieval and `memory_auto_improve` guidance.

### Fixed
- `auto-improve` now tolerates common malformed LLM proposal shapes found during
  live testing: evidence arrays may contain bare quote strings instead of
  `{ page, quote }` objects, a missing `operation` defaults to the only supported
  `create_or_update` operation, markdown bodies can arrive as `body`,
  `markdown`, or `content`, plural/folder-style kind names are normalized before
  path validation, and proposals still missing required target data are rejected
  by validation instead of turning the whole review into a 502 response.
- Admin destructive ops (`/admin/purge-project`, `/admin/move-project`) now
  propagate the authenticated actor (from the auth middleware's
  `Extension<ActorContext>`) into the admission context, so a `scope-guard`
  admission webhook can authorize them by user. They previously built the
  context with an empty actor, which a per-user scope-guard ACL rejected with
  `403 user '' not allowed to purge_project`, making purge/move unusable on any
  instance running scope-guard. `rename-project` is unaffected (it runs no
  admission chain).
## [1.0.6] - 2026-06-14

### Changed
- Centralized workspace/project scope resolution in `ai_memory_store::ScopeResolver`
  and shared explicit helpers, then migrated MCP, admin, and web API routes onto
  the common no-create/create-on-write policies.
- Centralized auth checks behind `AuthLevel::authorize(Capability::...)` so admin,
  user-management, and admission-skip behavior share one permission framework.
- Tightened wiki/SQLite consistency semantics: markdown remains the source of
  truth, batch writes install files before committing the derived SQL index, and
  runtime SQL failures roll installed files back best-effort.

### Added
- Regression tests for shared scope policies, capability authorization, and
  wiki-file rollback when `write_page` / `apply_batch` store upserts fail.

## [1.0.5] - 2026-06-14

### Fixed
- Multi-user admin authorization is now enforced at the shared `/admin/*`
  router boundary, including user-management routes, so DB-user tokens can use
  normal MCP/API read-write surfaces for attribution but cannot reach any admin
  endpoint. Added regression coverage for every admin route plus DB-user write
  attribution.

## [1.0.4] - 2026-06-14
### Added
- `install-hooks --agent grok` (plus `setup-agent` / `uninstall` coverage) for
  the xAI **Grok Build CLI**. Grok's `~/.grok/hooks/ai-memory.json` shares Claude
  Code's JSON shape and seven-event vocabulary, with a Grok-specific hook bundle
  and native `ai-memory hook --event … --agent grok` commands.
  ai-memory entries merge into a dedicated `ai-memory.json`, leaving any
  third-party `~/.grok/hooks/*.json` untouched. NOTE: Grok ignores hook stdout on
  `SessionStart`, so capture works but handoff injection does not. Grok's
  `session-start` therefore skips the handoff fetch entirely — the fetch is
  destructive (it marks the handoff accepted server-side) and Grok would discard
  the result, silently losing the handoff; recover a prior session's handoff via
  the MCP `memory_handoff_accept` tool instead. Adds `AgentKind::Grok` (`grok`
  wire tag), the `AgentKind::session_start_injects_handoff` gate, and migration
  `V20` extending the `sessions.agent_kind` CHECK so Grok sessions persist on
  upgraded servers (the antigravity/`V11` precedent).

### Fixed
- Actor-scoped MCP tool calls no longer fall through to a user's or process's
  latest hook-published project when the request carries a session id that does
  not match hook activity. This prevents HTTP-remote shared servers from reading
  or writing another same-user project when the MCP transport session id differs
  from the hook session id ([#97]).
- `memory_handoff_begin` and `memory_handoff_accept` now accept an optional
  `workspace` argument alongside `project`, resolving through the same
  workspace+project path as `memory_write_page` (begin, create-if-missing) and
  `memory_handoff_cancel` (accept, find-only). They previously took `project`
  only, so a cross-workspace handoff was routed by the per-actor active-project
  fallback and could be written to — or read from — the wrong project.
  `memory_handoff_cancel` already carried `workspace`.
- Workspace/project resolution now fails closed across the MCP and admin
  surfaces. Explicit MCP project misses no longer fall back to the active/default
  project, write-style MCP calls reject `workspace` without `project`, admin
  read/search/embed/lint/sweep paths use no-create lookups, and `/admin/reorg`
  only moves sessions and graveyards latest pages inside the target workspace.

## [1.0.3] - 2026-06-13
### Added
- Native macOS release tarballs (`ai-memory-macos-aarch64.tar.gz` for
  Apple Silicon and `ai-memory-macos-x86_64.tar.gz` for Intel) are now
  published on every tag, alongside the existing Linux tarballs and the
  Windows zip. The macOS `release-build` CI job also runs on every push,
  so a macOS-only release regression is caught before the tag rather
  than after. Install instructions added to `README.md` and
  `docs/install.md` ([#94]).

## [1.0.2] - 2026-06-12
### Fixed
- Session-page tool-call counts no longer double every entry. The
  no-LLM synthesizer was counting both `PreToolUse` and `PostToolUse`
  observations into the same bucket, so a single Bash call rendered
  as `Bash: 2` and two real calls as `Bash: 4`. It now counts only
  `PostToolUse` (the "completed call" event), matching the user-facing
  meaning of the heading.

## [1.0.1] - 2026-06-12
### Added
- `install-mcp --client vscode-copilot` renders (and `--apply` writes) a
  workspace-scoped `.vscode/mcp.json` for VS Code GitHub Copilot's agent
  mode. The renderer uses VS Code's MCP framework schema — top-level
  `servers` key, `type: "http"`, `url`, and an inline `headers` map for
  the bearer token — and includes a note that VS Code Copilot does not
  yet expose lifecycle hooks, so ai-memory's automatic capture is not
  active there (the MCP tools must be called explicitly from chat).
  Aliases: `copilot`, `github-copilot`. `uninstall --only mcp` strips
  the same entry idempotently.

## [1.0.0] - 2026-06-12
### Added
- Native hook drain and handoff timings can now be raised with
  `AI_MEMORY_HOOK_DRAIN_TIMEOUT_MINUTES`,
  `AI_MEMORY_HOOK_HANDOFF_TIMEOUT_MINUTES`,
  `AI_MEMORY_HOOK_START_BUDGET_MINUTES`, and
  `AI_MEMORY_HOOK_END_BUDGET_MINUTES` for high-latency or large-backlog
  instances. Defaults preserve the existing short hook behavior; invalid,
  zero, or overly large values fall back or clamp safely.

## [0.16.0] - 2026-06-11
### Added
- Native Claude Code hooks on macOS/Linux now use the direct
  `ai-memory hook --event ...` command by default, matching native Windows.
  Native hook commands spool events locally and can authenticate with a stored
  per-developer OIDC device token instead of a shared static hook token.
- `ai-memory auth login oidc-device --issuer <url> --client-id <id>` stores a
  generic OIDC device-flow token for native hook authentication.

### Fixed
- `ai-memory uninstall --only hooks` now recognizes and removes native
  `ai-memory hook ...` commands as well as legacy script commands.

## [0.15.0] - 2026-06-11
### Added
- Wiki recovery checkpoints now have first-class operator commands:
  `ai-memory checkpoints` lists recent wiki git commits and
  `ai-memory restore-page --path <page.md> --from <rev>` restores one page
  from a checkpoint, writes a new post-restore checkpoint, and reindexes the
  restored page into SQLite. Startup also creates a one-time upgrade baseline
  checkpoint for existing wiki trees that had no git commits yet.

### Fixed
- Release publication now limits the GitHub Release asset download step to
  `ai-memory-*` artifacts, avoiding Docker Buildx side artifacts that can make
  tag workflows fail after binaries and Docker images are already published.

## [0.14.0] - 2026-06-11
### Added
- Tagged releases now publish a native Windows x86_64 zip artifact
  (`ai-memory-windows-x86_64.zip`) with `ai-memory.exe`, hooks, default
  config template, checksums, and Windows install docs, giving native
  Windows agents a no-toolchain path to the fast direct-binary hook mode.

## [0.13.0] - 2026-06-08
### Added
- New `ai-memory reindex` lifecycle command rebuilds the derived SQLite page
  index from the on-disk wiki. It recreates workspace/project rows from
  per-scope `_meta.md` manifests while preserving the UUIDs encoded in the
  wiki tree, then reindexes page markdown into pages, links, and FTS. The
  command refuses to run unless the SQLite store is clean; operators should
  stop the server, back up data, move/remove `db/memory.sqlite`, run
  `reindex`, and recompute embeddings separately with `embed` when needed.
- Wiki startup now backfills per-workspace and per-project `_meta.md` manifests
  containing the human workspace/project names (and `repo_path` for projects)
  so the markdown tree is self-describing enough to rebuild the derived DB.

### Fixed
- Wiki reindexing now treats `log.md` / `log-YYYY-MM.md` as raw hook ledgers
  only when their content opens with the hook log prefix, so ordinary markdown
  pages with reserved-looking names are no longer silently dropped. `_meta.md`
  manifests and direct watcher events also reject symlinks before reading.

## [0.12.3] - 2026-06-07

## [0.12.2] - 2026-06-07
### Fixed
- The hook router no longer auto-creates fragment "projects" for
  subdirectory cwds. When a tool call's cwd sits inside an existing
  project's `repo_path` tree (for example a `Read` of
  `manga-plus/reader/src/main.rs` while the session is attributed to
  `manga-plus`), the resolver now picks the existing parent project
  instead of materialising a `src` or `reader` project for the
  subdirectory name. The schema column `projects.repo_path` has
  always been there for exactly this kind of cwd matching; the
  resolver finally queries it. Sub-projects declared via
  `.ai-memory.toml` still win because their longer `repo_path` ranks
  ahead. New `find_project_by_cwd_prefix` reader helper covers the
  query. Three regression tests in `ai-memory-hooks::router::tests`
  pin the parent / sub-project / cold-start paths.
- V19 data-repair migration re-attributes pre-existing orphan
  observations and handoffs to their session's project, then deletes
  the now-truly-empty fragment project rows that were left behind
  by the bug above. The session is the source of truth — observations
  belong to the session that emitted them and the FK enforces
  session existence. The migration is idempotent (re-running on a
  repaired DB is a no-op) and runs once per data directory at server
  startup via the existing refinery chain. `scratch` is explicitly
  preserved per CLAUDE.md invariant #15a (defensive cwd-less
  default).

## [0.12.1] - 2026-06-07
### Fixed
- `GET /favicon.ico` now lives at the absolute host root, outside
  `--base-path` and outside the `/web` nest, so the browser's
  automatic favicon fetch actually reaches it. The 0.12.0 build mounted
  the route inside the web router and ended up serving the icon only
  at `/web/favicon.ico` — invisible to the browser's auto-fetch (the
  icon still appeared via the in-page `<link rel="icon">` tag, so the
  user-visible behaviour stayed correct, but the dedicated route was
  unreachable). The new mount is also exempt from bearer auth and the
  host allowlist so a fresh tab gets the icon without an HTTP Basic
  prompt; the embedded PNG is the same one any visitor to `/web`
  already sees, so the info-leak surface is nil. Surfaced by the
  post-merge audit live test ([#79]).

## [0.12.0] - 2026-06-07
### Added
- New `ai-memory hook` subcommand emits one lifecycle event natively (reads
  the JSON payload from stdin, POSTs to `/hook`, GETs `/handoff` on
  `session-start`) without spawning a shell. On native Windows, Claude Code
  now defaults to the native hook command — measured ~3.5-5× faster per
  tool-call hook (~735 ms shell → ~150-205 ms native on an i7-6700HQ).
  Opt back into the previous Git Bash + `.sh` path with
  `AI_MEMORY_HOOK_PLATFORM=windows-bash`. See
  [`docs/windows.md`](docs/windows.md#native-hook-command-claude-code-on-windows)
  ([#84]).
- New `GET /favicon.ico` route on the web UI serves the same logo bytes as
  the header, so the browser tab carries an icon without an extra asset
  embed ([#79]).
- Thin-client CLI commands (`status`, `write-page`, `search`, `read-page`,
  `embed`, `lint`, `backup`, …) now respect the server's base-path mount
  via `AI_MEMORY_BASE_PATH` or the path component of `AI_MEMORY_SERVER_URL`
  (URL path wins), so deployments hosted behind a reverse proxy under a
  subpath stop 404'ing — including the container `HEALTHCHECK`
  (`ai-memory status`). Empty / unset means root mount, byte-identical to
  the prior behaviour ([#82]).

### Changed
- The embedded web UI ships a single transparent 768×768 PNG (~126 KB,
  down from a 992 KB JPEG mislabelled as PNG) used for both the header
  logo and the favicon. README branding stays on the existing
  light/dark pair via `<picture>` ([#79]).

### Fixed
- `install-hooks --apply` (and `ai-memory upgrade`, which calls it) now
  MERGES into per-event hook arrays instead of replacing them, so
  third-party hooks registered under the same event (e.g. a context-mode
  `SessionStart` guard) survive re-apply. ai-memory-owned entries are
  still swapped for the fresh ones; re-runs stay idempotent. Resolves
  #80 ([#83]).
- FTS5 searches for filenames carrying ASCII punctuation no longer error
  or silently miss. `current.md` (which used to surface
  `fts5: syntax error near "."`) and `ui-refresh` (which silently returned
  zero hits despite `follow-ups/ui-refresh-scroll-restoration.md` existing)
  both work end-to-end. Punctuated tokens are now quoted as both
  whole-form and split-form phrases, OR'd, to satisfy the asymmetry
  between the content tokenizer (`tokenchars '/_-'` keeps them inside
  tokens) and the path index (which pre-expands `/_-.` to spaces)
  ([#81]).

## [0.11.0] - 2026-06-05
### Added
- New `[auto_scope]` config block (`mode`, `session_ttl_secs`,
  `max_entries`) selects how the hook-published "currently active project"
  pointer is shared across concurrent MCP callers. The default `single` mode
  preserves the historical process-wide slot. Opt-in `per_session` keys the
  pointer by `session_id` to isolate concurrent agent runs of the same
  operator; opt-in `per_actor` keys by `(user, session_id)` to isolate
  across operators as well, pairing with multi-user mode where `user`
  comes from the `users` row that owns the bearer token. `per_actor`
  also keeps a user-only fallback slot so authenticated MCP requests
  from clients that cannot forward a session id do not inherit another
  user's latest project; same-user session isolation still requires a
  client/bridge that sends `X-Memory-Actor-Session-Id` or
  `Mcp-Session-Id` on MCP tool calls. Per-key entries carry an insertion
  timestamp and are TTL-evicted (default 1 hour) and
  capped (default 4096) so adversarial / runaway clients cannot grow the
  map without bound. Both opt-in modes still publish to the single slot
  in parallel, so any caller without actor context falls back gracefully
  to the most recent project rather than an empty pointer. All MCP read
  tools (`memory_query`, `memory_recent`, `memory_read_page`,
  `memory_status`, `memory_briefing`, `memory_explore`, `memory_lint`,
  `memory_forget_sweep`, `memory_handoff_*`) now thread the request's
  `ActorContext` into scope resolution, so opt-in isolation takes effect
  for the full read surface.

### Fixed
- Claude Code lifecycle hooks now emit structured JSON on stdout. Fire-and-
  forget hooks return `{}`, and `SessionStart` wraps pending handoff text in
  `hookSpecificOutput.additionalContext`, avoiding Claude Code's repeated
  "Hook output does not start with {" debug spam while preserving handoff
  injection.
- `POST /admin/rename-project` now returns `404 Not Found` when the project row
  has been deleted (typically by a concurrent `purge-project`) between the
  handler's id lookup and the writer's `UPDATE`. The pre-fix path silently
  responded `200 OK` with `pages: 0` for an operation that affected zero rows,
  which contradicted the concurrent purge's also-`200 OK` destruction of the
  same project and gave operators no signal that the rename had been undone.

## [0.10.0] - 2026-06-04
### Added
- New `POST /admin/delete-page` HTTP endpoint deletes a single page with
  explicit `(workspace, project)`. Like `purge-project`/`rename-project`, it
  uses no-create lookup — a delete on a typo'd or wrong scope now returns
  `404 workspace 'X' not found` instead of silently auto-creating the
  container and returning misleading `deleted: true`.
- New `ai-memory delete-page --path <P> --workspace <W> --project <P>` CLI
  subcommand, a thin client of `/admin/delete-page`. Mirrors the
  write-page/read-page CLI shape so terminal users get a complete
  delete-single-page surface for the first time.
- New `memory_handoff_cancel` MCP tool marks an exact open handoff id expired,
  giving agents a safe way to discard a mistakenly-created pending handoff
  before the next session consumes stale context.

### Fixed
- MCP tool descriptions and routing snippets now draw a sharper boundary
  between read-only `memory_briefing` and session-ending
  `memory_handoff_begin`, reducing accidental dangling handoffs when an agent
  was only asked for project status.
- Custom `--web-ui-dir` SPAs mounted at a non-root `--web-slug` now serve the
  injected shell at the trailing-slash root too (for example `/web/`), matching
  `/web` and deep client routes instead of returning a refresh-only 404.
- OpenCode and OMP generated hooks now derive `project_strategy = "repo-root"`
  project names from the host-visible `.ai-memory.toml` marker directory before
  sending hook payloads, so dockerized servers no longer fall back to git
  discovery inside paths they cannot see.
- `memory_delete_page` (MCP) now accepts `workspace` alongside `project` and
  routes scope through `effective_ids_for_read_args`, the same path the read
  tools use. Previously a project name that lived in multiple workspaces
  could silently route the delete to the wrong slot and return `deleted:
  true` for a page that was never touched. Operators on shared (multi-
  workspace) servers should explicitly pass `workspace + project` to make
  the target unambiguous.

## [0.9.0] - 2026-06-02
### Added
- `openai-compat` LLM providers can now opt into strict JSON Schema structured
  output with `AI_MEMORY_LLM_COMPAT_STRICT=true`. Strict mode sends
  `response_format=json_schema` first for compatible Ollama, vLLM, LM Studio,
  llama.cpp, and gateway endpoints, while the tolerant JSON-object parser
  remains the default and the fallback for strict raw-call failures ([#70]).
- The read-only web browser now renders `[[wiki links]]` as clickable internal
  links to the target page. Supports `[[path]]`, `[[path|label]]`,
  `[[project:path]]`, and `[[workspace/project:path]]`, resolved against the
  current page's project unless the target carries its own scope; bare targets
  get a `.md` suffix. External schemes, path traversal, and links inside fenced
  or inline code are left as literal text ([#68]).
- `ai-memory serve --transport http` can host the entire HTTP surface under a
  configurable subpath with `--base-path` / `AI_MEMORY_BASE_PATH`; `/mcp`,
  `/hook`, `/admin/*`, `/api/v1`, and the web UI all move under that prefix.
  The web UI mount can also be changed with `--web-slug`, and custom
  `--web-ui-dir` SPAs receive injected `<base href>` plus
  `ai-memory-base-path` metadata for same-origin API calls behind reverse
  proxies ([#65]).
- `ai-memory move-project` can move projects across workspaces via the admin
  API. Fresh destinations use a lossless true move that keeps the same
  `project_id`, sessions, observations, handoffs, embeddings, and page history;
  existing same-named destination projects use copy-purge merge with explicit
  `on_conflict` handling. Admission webhooks can subscribe to the new
  `move_project` event and receive destination names in the context ([#60]).
- Page FTS now indexes normalized page paths, so searches can find pages by
  filename or slug even when the slug does not appear in the title/body ([#62]).
- Admission webhooks can now observe, mutate, or reject engine write/delete/
  purge operations, with authenticated actor context, loop-prevention skip
  lists for trusted re-entry, and non-blocking observer webhooks for mirrors
  and backups ([#55]).
- New `memory_delete_page` MCP tool deletes a single page by exact path,
  updates the SQLite index directly, and fires `op=delete` admission hooks
  before removal ([#55]).

### Fixed
- Backups no longer dereference symlinks under `wiki/`, preventing a planted
  wiki symlink from pulling arbitrary readable host-file contents into
  `backup.tar.gz`.
- `ai-memory restore` now validates tar entries before extraction and accepts
  only regular files/directories under the expected backup paths
  (`wiki/`, `db/memory.sqlite`, and `config.toml`), rejecting links, special
  files, unsafe paths, and unexpected archive entries.
- In multi-user mode (`[auth].token_pepper` configured), operational
  `/admin/*` endpoints now require the root token; DB-user tokens receive
  403 while single-user installs keep the historical permissive admin behavior.
- LLM provider clients now cap provider response bodies before JSON, text, or
  SSE parsing, and truncate error bodies from bounded buffers instead of
  buffering arbitrary-size responses.
- Non-blocking admission webhooks now have a process-level in-flight cap and
  webhook timeouts are clamped to a safe maximum, preventing observer hooks
  from growing unbounded background work during write bursts.
- Hook cwd/project resolution caching is now bounded with LRU-style eviction,
  preventing unbounded process-lifetime growth from streams of unique cwd
  values.
- `memory_write_page` tool description and routing prompts now steer agents
  toward writing the page title as a `# H1` on the first line of `body` and
  omitting the `title` argument. ai-memory already auto-derived the title from
  `# H1` (or path stem) when `title` was missing — the change is documentation
  only, but it eliminates a known source of MCP `JSON parsing` errors when the
  LLM failed to escape quotes/colons in `title` ([#67]).
- Custom `--web-ui-dir` frontends no longer serve raw `/index.html` without
  base-path injection; direct index requests and SPA fallback routes now return
  the injected shell, while static assets remain untouched ([#65]).
- `move-project` true moves now run through a wiki mutation gate: normal
  page writes/reindexes validate the `(workspace_id, project_id)` pair before
  touching disk, while true moves hold the exclusive side across the directory
  rename and DB re-stamp. Stale old-workspace writes now fail without creating
  orphan files, and V18 aborts if existing split-brain rows are present ([#60]).
- `move-project` copy-purge conflict detection now treats body, frontmatter,
  title, tier, and pinned status as the page identity under `on_conflict=block`,
  preventing metadata-only overwrites from slipping through ([#60]).
- `memory_write_page` calls that specify `project` without `workspace` now
  default to the active workspace published by hooks, and project-only reads use
  the same active-workspace resolution so the write can be read back without an
  explicit workspace ([#61]).
- `memory_read_page` now accepts explicit `workspace` + `project` for sibling
  projects and falls back to the stored DB body only when the markdown file is
  missing, not when the disk source of truth is corrupt or unreadable ([#63]).
- `openai-oauth` now speaks the current ChatGPT/Codex responses stream format
  for bootstrap/consolidation requests and avoids sending the unsupported
  `max_output_tokens` field on that endpoint ([#64]).
- `ai-memory write-page` now resolves an omitted `--project` through the same
  current-project heuristic as `read-page` and `search`, preventing writes from
  landing in `scratch` while the read-back targets the cwd-derived project
  ([#66]).

## [0.8.1] - 2026-05-30

### Fixed
- **Consolidation no longer fails on long sessions** (~5,000+ observations or
  multi-hour agent runs). Two bugs surfaced trying to consolidate a real
  16-hour / 7,234-observation session:
  - **Prompt confusion (regression from the v0.8 `slot_kind` work):** the
    multi-page consolidator prompt listed `slot_kind` values
    (`state` / `invariant`) immediately above the `tier` values
    (`working` / `episodic` / `semantic` / `procedural`). The LLM read them
    as one list and emitted `tier: "state"` in structured responses, which
    deserialisation rejected. Prompt now leads with `tier` (with explicit
    "EXACTLY ONE OF FOUR strings" emphasis), then `kind`, then `slot_kind`
    under its own clearly-scoped section that states "completely unrelated
    to tier" and "only for `_slots/*` paths."
  - **No token budget on the observation dump:** `build_request` and
    `build_batch_request_with_slots` dumped every observation into the
    prompt buffer, which exceeded the provider's 200k-token context on long
    sessions (the sabadell run produced a 235k-token request → 400 from
    the provider). New `window_observations_to_budget` walks the slice
    from most-recent backward, keeping each entry whose render cost fits
    in a 400k-char budget (~100k tokens), leaving room for the system
    prompt + schema + LLM output. When entries are skipped, a prepended
    note tells the LLM the context is partial so its summary doesn't
    pretend to cover the early session. Both `PreCompact` and
    `memory_consolidate` triggers benefit from the fix — both were silently
    failing into the `warn!()` catchall on sessions this long.
  - 5 unit tests guard the windowing invariants (empty input, fits-under-
    budget passthrough, most-recent-preserved, single-too-large-obs drops
    everything, observation-boundary alignment). No schema change, no
    config knob, backward-compatible for sessions that already fit.

## [0.8.0] - 2026-05-30

### Added
- **Multi-user attribution (v0.8 Phase 1, rolling out across milestones
  P1.1–P1.8).** ai-memory's data model stays single-tenant — every
  authenticated request sees every page — but writes can now be
  attributed to a named user. Five `ai-memory user` subcommands
  (`add`, `list`, `expire`, `revive`, `rotate-token`) manage a `users`
  table; the auth middleware resolves every request to one of four
  tiers (Anonymous, Root, DB user, 401), injects an
  `Extension<ActorContext>` + `Extension<AuthLevel>` for downstream
  consumers, and gates the root-only admin user-management endpoints.
  Tokens are 32 bytes of OS CSPRNG, stored only as
  `SHA-256(token || ":" || token_pepper)` (per-server pepper from
  `[auth].token_pepper`, auto-generated on `ai-memory init`); see
  [`docs/users.md`](docs/users.md) for the SHA-256-not-argon2id
  rationale, the four-rung auth ladder, and the backward-compat
  migration for pre-v0.8 installs. New v0.8 fields on `[auth]`:
  `root_username` / `root_email` / `root_name` (label for the bearer
  token's writes) and `token_pepper`. Per-page `author_id` + web UI
  surfacing lands in P1.6/P1.7; this milestone set ships P1.1
  (`ActorContext` + `UserId` in core), P1.2 (table + writer/reader
  ops + V14 migration), P1.3 (auth middleware), P1.4 (root-gated
  `POST/GET /admin/users` + `…/expire|revive|rotate-token`), and P1.5
  (CLI subcommands). **No behaviour change for existing single-user
  installs**: without `[auth].token_pepper` the multi-user lookup
  stays dormant, user-management endpoints 503 with a clear
  `multi-user not enabled` message pointing at `ai-memory init`,
  and the existing `bearer_token`-only flow keeps authenticating
  exactly as before.
- **`memory_query { global: true }` — cross-project global search** that
  reaches every project in every workspace in one call, with each hit
  annotated by its workspace + project so the agent can tell where it
  came from. Use when the agent doesn't know which project holds a
  cross-cutting note (shared infra/ops, a sibling app). Mutually
  exclusive with `scopes`/`project`/`workspace`. Routing snippet +
  `MEMORY_INSTRUCTIONS` now teach both broadening modes (`scopes` for
  named siblings, `global=true` for unknown locations) and explicitly
  warn that `memory_query` returns snippets — use `memory_read_page`
  for full bodies. The prompt-surface contradiction the original PR
  shipped ("there is no global 'search everything' mode" right after
  the bullet advertising `global=true`) was caught in the post-merge
  audit and rewritten; the prompt regression test now refuses any
  variant of that legacy phrasing
  ([#56], thanks @djalmajr).
- **Cross-project wiki links + dependency graph.** Wikilinks gain an
  explicit scope qualifier: `[[project:path.md]]` for a sibling project
  in the same workspace, `[[workspace/project:path.md]]` for another
  workspace. Bare links are unchanged (resolve within the source's own
  project). `links.to_workspace` / `links.to_project` join the primary
  key so the same `to_path` can land in two different projects without
  colliding. `memory_lint` now reports dangling cross-project refs
  (typo'd project vs missing/renamed target page), `memory_briefing`
  exposes `cross_project_dependents` / `cross_project_dependencies`
  per project, and `GET /api/v1/graph` returns the resolved cross-
  project edges for a graph view. Migration V13 rebuilds the `links`
  table preserving existing rows as `(to_workspace=NULL,
  to_project=NULL)` — same "local" semantics as before
  ([#57], thanks @djalmajr).

### Changed
- **FTS5 queries OR-join bare multi-word inputs** instead of the
  pre-existing AND default. A natural-language query like
  `"have we discussed cross project search strategy"` previously
  required every word to co-occur in one page — near-zero recall for
  multi-word queries, which the caller silently mistook for "never
  recorded". OR + BM25 ranking (callers already `ORDER BY rank`) keeps
  the best-matching pages at the top of the list, so the user-visible
  top-N is still AND-ish; OR just adds a relevant tail instead of
  returning nothing. Explicit FTS5 syntax (`OR`/`AND`/`NOT`/`NEAR`,
  quoted phrases, parens) is detected and preserved verbatim so the
  exact-match escape hatch stays available. 5 new unit tests guard the
  preservation contract (post-merge audit). Migration V12 rebuilds the
  FTS tables with `unicode61 remove_diacritics 2` so accent-free
  Portuguese queries (`"descricao da sessao"`) match accented stored
  text (`"descrição da sessão"`); contentless FTS — source rows
  untouched ([#58], thanks @djalmajr).
- **MCP write tools now honour the session's project (and create
  named projects on demand).** Three correctness fixes on
  `memory_write_page` / `memory_lint` / `memory_forget_sweep`:
  - A `memory_write_page { project: "X" }` for a project name that
    doesn't exist used to silently fall through to the session's
    active project (find-only resolution); writes meant for a fresh
    project polluted the current one. A new `write_target_ids`
    helper uses **get-or-create** for an explicit project name, so
    a named write always lands where the agent asked.
  - `memory_lint` + `memory_forget_sweep` previously always targeted
    the server's baked `--project` regardless of the session, so a
    cross-project lint or retention sweep could never reach the
    project the user was actually working in. Both now resolve
    through the same find-only `effective_ids_for_read_args` path
    the read tools use, with the hook-published active project as
    the fallback.
  - Both `lint` / `sweep` and the new `write_page` add explicit
    `workspace` + `project` args (defaulted to current session,
    documented with the v0.5.2 "**Omit unless the user explicitly
    names a *different* project.**" tail). 2 regression tests cover
    "Bug B" (explicit-project write must create + land) and
    "Bug C" (sweep must evaluate the named project, not the baked
    default) ([#59], thanks @djalmajr).

## [0.7.1] - 2026-05-29

### Fixed
- **`install-hooks --agent codex` no longer panics with `index not found`**
  when `~/.codex/config.toml` carries an `[mcp_servers]` table that has other
  MCP servers (context7, node_repl, …) but no `ai-memory` entry — a
  perfectly valid setup since ai-memory can integrate via hooks alone.
  `infer_codex_mcp_config` used `toml_edit`'s panicking `Index` impl with
  bare `[]` chains; it now walks the table via `.get()` and returns `None`
  on any missing key. Mirrors the safe pattern the JSON variant has used
  all along. Adds 4 regression tests covering missing-entry,
  missing-table, empty-doc, and bare-entry inputs
  ([#53], thanks @Otavio-Machado-Santos).
- **`install-hooks --agent claude-code` no longer silently stages 0 scripts
  and points `settings.json` at an empty directory.** On macOS — and any
  install where the binary lives outside the repo and the system package
  paths (`/usr/local/share`, `/usr/share`) are absent — `resolve_hooks_dir`
  fell through to the data-local candidate, which was *also* the staging
  destination. The wipe-then-copy flow inside `stage_hook_scripts_in` then
  deleted the very scripts it was about to read, leaving 0 copied; the
  caller proceeded to rewrite `settings.json` anyway, disabling capture
  with no error. The function now (a) canonicalizes source and destination
  paths, skips the wipe + copy when they match and verifies in-place,
  preserving any scripts a prior `setup-agent` run extracted there, and
  (b) bails with an actionable error pointing at `--hooks-dir` or
  `ai-memory setup-agent` whenever zero scripts are present in either
  branch. Adds 3 regression tests
  ([#52], thanks @Otavio-Machado-Santos).
- **macOS thin-client wrapper no longer crashes with "Permission denied" in
  the log file appender.** The `bin/ai-memory` wrapper passed
  `-u $(id -u):$(id -g)` to the one-shot helper container, which on macOS
  collides with the data volume owner (uid 1000 inside the container vs
  uid 501/502 on the host). The wrapper now skips `-u` on Darwin so the
  container runs as its default uid 1000 — Docker Desktop's file-sharing
  layer handles host ownership transparently — while Linux and other
  Unix systems continue to receive `-u`. Same change also hardens the
  `${TTY_ARGS[@]}` / `${NETWORK_ARGS[@]}` / `${ENV_ARGS[@]}` /
  `${USER_ARGS[@]}` expansions for `set -u` compatibility on macOS's
  default bash 3.2 ([#51], thanks @abnersajr; supersedes [#50]).

## [0.7.0] - 2026-05-29

### Added
- **`memory_read_page` MCP tool** (`read-only`) for fetching the FULL body of a
  wiki page — pass `path` for a direct lookup or `query` to fetch the top FTS5
  hit's full body. Complements `memory_query`'s 24-word snippets when an agent
  needs to read an entire decision page end-to-end. Also exposed as
  `GET /admin/read-page?workspace=…&project=…&path=…` (admin HTTP) and the new
  `ai-memory read-page` CLI subcommand (thin HTTP client). All three surfaces
  scope to the current project by default and route user-supplied paths through
  `PagePath::new`, so traversal attempts (`../etc/passwd`) are rejected with
  400. ARCHITECTURE.md's MCP-tool table grows from 12 to 13 rows ([#49]).
- `_slots/*.md` pages can now declare `slot_kind: state` or
  `slot_kind: invariant` frontmatter. `state` remains the default for existing
  slots; `invariant` marks high-resistance project context or preferences that
  consolidation should not rewrite unless observations directly contradict the
  existing slot content ([#47], closes [#14]).

### Fixed
- **Windows PowerShell hooks no longer hang or stall the agent.** The shared
  `hooks/lib/ai-memory-hook.ps1` read stdin via `[Console]::In.ReadToEnd()`,
  which blocks indefinitely when the agent does not close the stdin pipe
  (observed on Claude Code `PreCompact`); because the `Invoke-WebRequest`
  timeout only starts after the read returns, a stuck read meant the hook
  never POSTed anything. Stdin is now read asynchronously, guarded by
  `[Console]::IsInputRedirected` with a 2s cap, so the hook can never freeze.
  HTTP timeouts were also raised from 1s to 3s (POST) / 2s (handoff GET) to
  tolerate remote servers over higher-latency links. The full raw payload is
  still forwarded (parity with `_lib.sh`), so observation title/body stay
  intact. Affects every agent still on the PowerShell hook runner
  (Codex, Cursor, Gemini CLI, Antigravity, OpenCode on Windows) ([#48]).
- Page upserts now treat frontmatter/title/tier/pinned changes as real page
  updates instead of short-circuiting solely on unchanged body text, keeping
  the SQLite index consistent with markdown frontmatter-only edits ([#47]).

## [0.6.1] - 2026-05-28

### Added
- `Cache-Control: private, max-age=N` headers on all `/api/v1` read endpoints
  (lists/search/recent/briefing/overview: 30–60s; single-page reads: 300s).
  Errors stay uncached. A polling SPA no longer hits the DB on every request.
- **ETag + conditional GET** on the single-page read endpoint
  (`GET /api/v1/workspaces/{ws}/projects/{p}/pages/{*path}`): the response
  carries `ETag: "<sha256>"` over the markdown body, and a follow-up request
  with matching `If-None-Match` returns `304 Not Modified` with no body.
- **`--cors-allow-origin`** flag (repeatable) and
  `AI_MEMORY_CORS_ALLOW_ORIGINS=a,b,c` env var. When set, a `CorsLayer` is
  attached **only to `/api/v1`** (`/mcp`, `/hook`, `/admin`, and `/web` are
  intentionally untouched) so a separately-hosted SPA can call the API. Each
  origin must include a scheme; `*` is rejected at startup (CORS spec forbids
  credentials + wildcard). Empty list = same-origin only, unchanged behaviour.

## [0.6.0] - 2026-05-28

### Added
- Read-only **`/api/v1`** JSON surface for third-party frontends: workspaces,
  projects, pages (list + read with frontmatter, body, resolved links, and
  back-links), recent, briefing, search (GET single/global + POST multi-scope
  capped at 25 scopes), and workspace/project `overview` aggregates (handoff +
  briefing + memory-health drill-down). Mounted before the bearer +
  host-allowlist middleware so existing auth applies automatically. Read-only
  by construction — zero writer calls in the handlers ([#7]).
- **`--web-ui-dir`** flag on `ai-memory serve` to host any static SPA at
  `/web` (same origin as the API, behind the same auth), with `index.html`
  SPA fallback via `tower-http::ServeDir`. Validates the directory exists
  and contains `index.html` before binding. When the flag is absent, the
  built-in server-side `/web` browser stays the default ([#7]).
- MCP read tools (`memory_query`, `memory_recent`, `memory_status`,
  `memory_briefing`, `memory_explore`) accept optional `workspace` +
  `scopes` args for explicit multi-project queries; existing single-`project`
  behaviour is unchanged and remains the default ([#7]).
- New reader queries powering the API: per-page outgoing links + incoming
  back-links, workspace-aggregated briefing, memory-health (stale /
  duplicate / orphan) counts and drill-down lists, workspace summaries
  with last-update timestamps ([#7]).

### Fixed
- Antigravity `pre-tool-use` hook now emits the documented
  `{"decision":"allow"}` JSON contract instead of an empty `{}`, while
  keeping the `ai_memory_post_hook` call fully suppressed
  (`>/dev/null 2>&1 || true`) so the `queued` body never bleeds into the
  hook's stdout. Identical logic for `.sh` and `.ps1`; other hook scripts
  remain silent and unchanged ([#44], thanks @ArtroxGabriel).

### Docs
- New **[`docs/frontend-api.md`](docs/frontend-api.md)** integration guide
  for `/api/v1`: auth flow, response schemas (`PageHit`, `BriefingSnapshot`,
  `HealthDetail`, `PageLinks`, …), error model, limits/pagination,
  custom-UI hosting, a worked `fetch`/`curl` example, and pointers to the
  canonical source-of-truth files.

## [0.5.2] - 2026-05-28
### Added
- `ai-memory status` / `status --json` now includes passive process-scoped LLM
  and embedding provider health based on the last real provider call, without
  active probing or token spend ([#46]).

### Changed
- Agent-facing prompts (`MEMORY_INSTRUCTIONS`, the `CLAUDE.md`/`AGENTS.md`
  routing snippet, and the per-tool `project`/`cwd` arg docstrings) now lead
  with a clear "default to the current project — do not pass `project` or
  `cwd` args unless the user names a *different* project" rule, plus a
  reminder that the SessionStart auto-fetched handoff block already covers the
  current project. Reduces cross-agent friction where a fresh agent surfaced
  the wrong project's handoff because the LLM over-eagerly passed scoping
  args. Doc-only, no behaviour change.

### Fixed
- Claude Code hook installs on native Windows now render Git Bash-compatible
  `bash -c` commands that keep the POSIX `.sh` hook scripts and convert
  drive-letter paths to Git Bash paths, matching Claude Code's actual hook
  runner instead of emitting PowerShell commands ([#45]).
- `ai-memory llm-test --provider anthropic-oauth` now parses and maps to the
  Anthropic OAuth provider instead of being rejected by clap ([#43]).

## [0.5.1] - 2026-05-27
### Changed
- Docker release publishing now builds Linux x86_64 and aarch64 artifacts once,
  reuses those artifacts for Docker images, and smoke-tests both amd64 and arm64
  images after assembling the multi-arch manifest.
- The AUR `ai-memory-bin` package now supports aarch64 using the prebuilt Linux
  aarch64 release artifact.
- Docker source builds now use the vendored Tailwind CSS artifact, avoiding
  cross-architecture Tailwind CLI cache collisions during multi-arch releases.

## [0.5.0] - 2026-05-27
### Fixed
- Docker release images now publish both `linux/amd64` and `linux/arm64`
  manifests, so Apple Silicon and ARM64 Linux hosts can pull the image without
  forcing x86 emulation ([#41]).

## [0.4.0] - 2026-05-27
### Added
- `anthropic-oauth` LLM provider: use a Claude Pro/Max subscription via
  `claude setup-token` instead of an API key. In-Rust, reuses the existing
  Anthropic Messages client (incl. structured output). **Unofficial and
  against Anthropic's usage policies — use at your own risk** (docs warn
  prominently).
- Opt-in `AI_MEMORY_CONSOLIDATE_ON_SESSION_END`: when set and an LLM provider
  is configured, SessionEnd additionally runs LLM consolidation on top of the
  always-written rule-based summary page (non-fatal on failure) ([#40]).

### Changed
- Docs recommend a small/fast model (Haiku/mini class) for the OAuth /
  subscription LLM backends — consolidation/lint/explore is summarisation, not
  hard reasoning, and small models are far easier on subscription rate limits.
- Aligned every prompt surface + doc with actual SessionEnd behavior: it always
  writes a rule-based summary page + handoff; LLM consolidation runs on
  PreCompact, on demand via `memory_consolidate`, and at session end only
  behind the new opt-in flag ([#40]).

### Fixed
- Windows own-write detection: `inode_of` now returns the real NTFS file index
  (was always `0`, which collapsed the watcher's own-write set) ([#37]).
- `ai-memory upgrade` no longer fails with `invalid value 'lib' for --agent` —
  the hook-refresh loop skips the shared `lib/` helper dir ([#38]).
- Native packaging CI now supports non-root runners whose `systemd-tmpfiles`
  lacks `--dry-run`, while still operating only inside a temporary alternate
  root.

## [0.3.2] - 2026-05-27
### Fixed
- AUR release publishing now runs with `HOME=/home/aurbuild` and an explicit
  `GIT_SSH_COMMAND`, so the workflow uses the configured AUR deploy key.

## [0.3.1] - 2026-05-27
### Changed
- Reissued the release after the initial AUR publish failure. This release was
  superseded by 0.3.2 for the AUR SSH home fix.

## [0.3.0] - 2026-05-27
### Added
- Arch Linux native packaging assets: source and prebuilt AUR package
  definitions, system/user systemd units, sysusers/tmpfiles entries, native
  config/env templates, CI-safe alternate-root packaging checks, and a manual
  disposable-distrobox integration harness for validating real service startup
  before publishing.
- Tag-triggered release automation now validates that `vX.Y.Z` matches
  `Cargo.toml`, publishes a native Linux release tarball, keeps Docker image
  publishing behind Docker Hub secrets, and optionally publishes both AUR
  package bases when `AUR_SSH_PRIVATE_KEY` is configured.
- `memory_write_page` MCP tool for explicit durable annotations, so agents can
  write permanent wiki knowledge without abusing single-use handoffs.
- `openai-oauth` LLM provider for ChatGPT/Codex accounts, including
  `ai-memory auth login|logout|status` device-flow commands and token storage
  in `<data_dir>/auth.json`.
- `copilot` LLM provider for GitHub Copilot Chat accounts. It stores a GitHub
  token via `ai-memory auth login copilot`, exchanges it for a short-lived
  Copilot API token, and sends Copilot Chat requests with `vscode-chat`
  integration headers.

### Fixed
- `install-mcp`, `install-hooks`, and `setup-agent` now honor configured
  `AI_MEMORY_SERVER_URL` defaults; `install-hooks` also reuses an existing
  ai-memory MCP entry when present, preventing remote MCP setups from
  regenerating loopback-only lifecycle hooks during installs/upgrades.
- Filesystem watcher now reindexes a project when backends report only a
  parent-directory event, improving external editor capture on macOS/FSEvents.
- OpenAI strict structured-output schema normalization now strips generated
  `$ref` annotation siblings and rewrites generated enum `oneOf` schemas to
  `anyOf`, unblocking `memory_consolidate multi_page=true` on OpenAI models.
- OpenAI-compatible embedding calls now truncate oversized page bodies, surface
  provider errors returned in HTTP 200 bodies, retry bounded HTTP 429 responses,
  and may reuse `LLM_API_KEY` when a custom embedding base URL is configured.
- `ai-memory embed --force` without `--project` now re-embeds every project in
  the workspace and purges stale/superseded embedding rows in the same scope.
- Windows hook `cwd` values sent to a Linux server now resolve projects by the
  final path component instead of treating the full backslash path as the
  project name.

## [0.2.0] - 2026-05-26
### Added
- `ai-memory bootstrap` now prunes collected sources before POSTing to the
  server and supports `--chunk-input-tokens` to process large repositories via
  sequential LLM calls instead of one oversized prompt.
- Opt-in extension event metadata for `/hook`: custom integrations can
  pass `extension=<namespace>` (and optionally `source_event=<name>`) to
  preserve a validated third-party source event while storage keeps the
  canonical `ObservationKind` closed. Unknown events without an extension
  still collapse to `other` with no source-event metadata.
- `.ai-memory.toml` marker file lets a directory tree declare its
  `workspace` (required) and `project` (optional) without depending on
  `basename($cwd)`. Lifecycle hook scripts walk up from `cwd` to find
  the closest marker and forward `cwd` plus the declared names as
  query params on `POST /hook` and `GET /handoff`. Markers can also set
  `project_strategy = "repo-root"` to derive project identity from the
  main git repository root, so linked worktrees share one project. Server
  accepts the new params as optional overrides;
  absent marker means the previous behaviour (`workspace = "default"`,
  `project = basename(cwd)`) — fully backward compatible. See
  [`docs/marker-file.md`](docs/marker-file.md).
- Oh My Pi / OMP is now a first-class integration: `install-mcp --client pi`
  and `--client omp` write native `~/.omp/agent/mcp.json` config, while
  `install-hooks --agent omp` and `--agent pi` write the TypeScript extension
  used for lifecycle capture and handoff injection.
- Graph-aware retrieval: `memory_query` now combines FTS5, wikilink-neighbor
  expansion, optional vector RRF, and bounded raw-observation fallback.
- Observation FTS indexing and unresolved-link diagnostics surfaced through
  admin/CLI status paths.
- `_slots/` wiki pages are automatically pinned and surfaced in briefing /
  explore snapshots.
- Server-side scheduled maintenance for forget sweep and lint, with optional
  embedding backfill scheduling.
- Experimental native Windows support: PowerShell Docker wrapper,
  `ai-memory.cmd`, `.ps1` lifecycle hooks in parity with `.sh` hooks, Windows
  Tailwind hash/download support, and [`docs/windows.md`](docs/windows.md).
- Google Gemini LLM provider via `AI_MEMORY_LLM_PROVIDER=gemini`, with
  `gemini-2.5-flash` as the default hosted Google model and `GEMINI_API_KEY`
  / `GOOGLE_API_KEY` support.
- Google Gemini embeddings via `AI_MEMORY_EMBEDDING_PROVIDER=google` or
  `gemini`, with `gemini-embedding-001` as the default embedding model and
  `GEMINI_API_KEY` / `GOOGLE_API_KEY` support.
- Antigravity CLI (`agy`) support for MCP config (`serverUrl`) and lifecycle
  capture through its `PreInvocation`, `PreToolUse`, `PostToolUse`, and `Stop`
  hook events.
- README support matrix for operating systems, agent integrations, LLM
  providers, and embedding providers.
- `ai-memory uninstall` — removes ai-memory's hooks, MCP registration, and
  CLAUDE.md/AGENTS.md instruction block across all detected agents (dry-run by
  default; `--apply` to execute, with timestamped backups). `--purge-data`
  wipes wiki/db/raw via the reset guard. `--only hooks|mcp|instructions` to
  narrow. MCP matching is endpoint-based by default; pass `--mcp-url` when the
  server was installed with a custom endpoint and `--mcp-name` only to narrow
  removal to one matching entry. Docker/volume teardown is printed as a hint,
  not executed.

### Changed
- Same-body page upserts are now true no-ops, avoiding periodic watcher
  reconcile writes, FTS churn, and misleading recent-page timestamps.
- Graph-neighbor expansion for hybrid search now batches all seed pages into
  one SQL query instead of issuing incoming/outgoing lookups per seed.
- Embedding backfill stores embeddings in chunks instead of one writer
  command and SQLite transaction per page.
- Hook ingestion now bounds in-flight processing and returns HTTP 429 when
  saturated instead of spawning unbounded background tasks.
- Documented the vector backend policy and the measured criteria required
  before adding `sqlite-vec`.
- Clarified Gemini CLI support docs: MCP registration, lifecycle hooks,
  SessionStart handoff injection, and SessionEnd capture are now called out
  consistently across README and install guides.
- Added OpenClaw lifecycle support via a generated native plugin package and
  updated Cursor / Claude Desktop / OpenClaw support docs against current
  upstream MCP and hook documentation.
- Docker images now bundle both POSIX and PowerShell hook scripts.
- `ai-memory uninstall --purge-data` now previews the `wiki/`/`db/`/`raw/`
  wipe in dry-run (mirroring `reset`) and refuses **up front** if an
  `ai-memory` process is alive (all-or-nothing) instead of removing the
  wiring and then skipping the purge. The data wipe is now shared with
  `reset` via a single internal helper.
- `ai-memory uninstall` only deletes generated plugin/extension files after
  re-validating their ai-memory-generated content, and never treats a matching
  filename or MCP server name alone as proof of ownership.

### Fixed
- `serve` now warns and starts when stored embedding rows were created with a
  different `(provider, model, dim)` than the current config. Hybrid search
  ignores stale rows until `ai-memory embed --force` or scheduled backfill
  re-embeds them, avoiding the previous startup deadlock.
- Session capture now persists every documented agent kind (`cursor`,
  `gemini-cli`, `claude-desktop`, `openclaw`, `omp` / `pi`) instead of
  failing the `sessions.agent_kind` database CHECK for agents added after
  the initial schema.
- `memory_handoff_begin` and `memory_handoff_accept` now resolve the active
  project the same way the briefing/search tools do, so MCP handoffs land in
  the project currently reported by hooks instead of the server's baked
  default project.
- Natural-language `memory_query` text containing bare colons, such as
  `pick: handoff`, no longer trips FTS5 column syntax errors while explicit
  FTS operators like `quick OR slow` remain supported.
- Marker-file routing now reaches the generated OpenCode and OMP
  TypeScript hook integrations, not only the POSIX/PowerShell script
  hooks. POSIX helpers also preserve the outer hook `cwd` when nested
  tool payloads contain their own `cwd`, and encode `+` correctly in
  marker-derived query parameters.
- `backup --to` now streams the tarball to disk instead of buffering the full
  archive in CLI memory.
- Hyphenated FTS5 queries such as `ai-memory` are normalized safely instead of
  being parsed as column operators.
- Gemini 2.5 Flash requests disable default dynamic thinking so hidden thought
  tokens do not consume `maxOutputTokens` and truncate strict JSON responses.
- `install-mcp --client claude-code` now prints the direct-edit JSON path as
  `~/.claude.json`, matching the `--apply` path and `claude mcp add` behavior.
- Hook routing now evicts a stale project-cache entry and retries once when a
  live server sees a cached project deleted underneath it, such as after
  `purge-project`, so capture resumes without restarting the server.
- Session-start handoff hooks now include `cwd` even without a marker file, so
  default `project = basename(cwd)` projects receive pending handoffs without
  requiring `.ai-memory.toml`.
- `ai-memory uninstall` now removes only ai-memory commands from mixed nested
  hook entries, preserves third-party commands in the same matcher, and removes
  legacy Codex inline-table MCP entries.
- Generated POSIX hook commands now shell-quote script paths and env values
  with metacharacters, fixing custom hook directories containing spaces and
  preventing shell-active token/URL fragments.
- OpenClaw's generated plugin now forwards marker-file routing params just like
  the OpenCode and OMP generated integrations.
- The Linux/macOS Docker wrapper now lets thin-client commands such as
  `status` and `bootstrap` reach the local quick-start server bound on the
  host's `127.0.0.1:49374`.

## [0.1.3] - 2026-05-24

### Added
- `ai-memory lint --no-llm` (and `memory_lint` `no_llm` arg) to run only the
  rule-based lint pass while leaving the LLM enabled for `memory_explore` /
  `memory_consolidate` ([#4]).

### Fixed
- `memory_lint` LLM contradiction pass silently never contributed: the
  `LintFinding` struct expected `severity`/`message` but the prompt asked for
  `summary`/`detail`. The prompt is now aligned to the canonical shape and the
  struct tolerates both (defaults `severity`, aliases `summary`→`message`,
  captures optional `detail`) ([#4]).
- Reasoning models (MiniMax M2.7, DeepSeek, Qwen, Kimi) that emit
  `<think>…</think>` / `<analysis>…</analysis>` blocks before the JSON broke
  structured-output parsing (`key must be a string at line 1 column 2`). The
  openai-compat provider now strips reasoning blocks and surrounding markdown
  fences before extracting the JSON object, so lint / consolidate / bootstrap
  work with reasoning models ([#5]).
- openai-compat base URLs with non-`v1` version segments (e.g. Z.AI's `/v4`)
  or a full endpoint path no longer produce `…/v1/v1/…` 404s
  ([#6], thanks @lucasliet).

## [0.1.2] - 2026-05-24

### Changed
- HTTP transport now defaults to **stateless** mode (`json_response`, no
  `Mcp-Session-Id` required), so stateless MCP clients (OpenCode
  `type: "remote"`, `curl`) work without an `mcp-remote` stdio shim
  ([#3]). New `serve --transport http --http-stateful` flag restores the
  previous session+SSE behaviour for clients that need it.

## [0.1.1] - 2026-05-24

### Added
- Wiki-structure migration framework: `wiki_migrations` SQL table (V06),
  `WikiMigration` trait, migration registry, and `run_pending` runner
  invoked at server startup before the watcher starts.
- MCP read tools (`memory_query`, `memory_recent`, `memory_status`,
  `memory_briefing`, `memory_explore`) accept an optional `project`
  argument to target a specific project on a shared server.

### Fixed
- OpenCode hook events (`tool.execute.*`, `session.*`) were rejected with
  "missing session_id" because OpenCode sends `sessionID` (capital `ID`)
  and the extractor only matched `sessionId`. All spellings are now
  accepted ([#1]).
- MCP read tools were locked to the server's static `--project` (default
  `scratch`), so on a shared HTTP server they returned empty memory even
  while hooks populated the correct per-cwd project. The hook router now
  publishes the active project to a shared pointer that the read tools
  use as their default; an explicit `project` argument overrides it ([#2]).

## [0.1.0] - 2026-05-23

### Added
- Per-project UUID-namespaced wiki layout: pages live at
  `<wiki_root>/<workspace_id>/<project_id>/<page-path>`. Rename is now
  a single column update; purge is `remove_dir_all` on the project dir.
- CLI becomes a thin HTTP client: `bootstrap`, `status`, `search`,
  `reorg`, `lint`, `forget-sweep`, `embed`, `commit`, `backup`,
  `write-page` all delegate to the running server via `/admin/*` routes.
  The server is the sole writer of wiki + SQLite.
- `purge-project` command with cascade-delete indexes and per-project
  isolation guard (refuses to delete files claimed by sibling projects).
- `rename-project` command: column-only rename, no file moves.
- `memory_install_self_routing` MCP tool: installs the agent-routing
  snippet into CLAUDE.md / AGENTS.md / `.cursorrules` in one call.
- Read-only HTTP wiki browser (`/web`) with project tree, page view,
  and full-text search.
- Bearer token auth (`AI_MEMORY_AUTH_TOKEN` / `generate-auth-token`),
  Host-header allowlist, and 10 MB body cap for the HTTP server.
- `backup` / `restore` commands using `.tar.gz` archives with live-process
  guard (refuses to run if another `ai-memory` is active on the same data dir).
- Per-cwd project routing in hooks: observations route to the project
  matching the agent's working directory, not the server default.
- `opencode` / `openclaw` aliases for the OpenCode MCP client.
- Dockerised CLI wrapper (`bin/ai-memory`) with auto-restart for the
  local container and nudge for remote upgrades.
- `bootstrap` serialises parallel runs to prevent duplicate project creation
  and handles the case where the CWD has no git repo.
- Monthly log-md rotation to keep `log.md` from growing unbounded.
- `memory_consolidate` PreCompact checkpointing falls back to rule-based
  summarisation when no LLM is configured.
- `docs/lifecycle-ops.md`: safety matrix for state-touching commands
  (reset, restore, purge-project, rename-project).
- `docs/wiki-migrations.md`: when and how to write a wiki migration.

### Changed
- `bin/ai-memory` forwards `AI_MEMORY_SERVER_URL` and no longer creates
  `-w` mount-conflict directories.
- `bootstrap` resolves the repo root via `libgit2`, removing the
  `git` binary dependency.
- Admin routes consolidated: dry-run support, correct status codes,
  deduplicated handlers.
- Host-header allowlist sourced from `Config.allowed_hosts`; logged at
  startup so operators can verify the effective list.

### Fixed
- `AI_MEMORY_HOST_CWD` handling and dry-run no-project side effects.
- Web page view: strip leading H1 from body to prevent title duplication.
- `install-mcp` Codex config key was `bearer_token`, not
  `http_headers` / `headers`.
- Consolidator used server startup default project instead of the
  session's actual project.

[Unreleased]: https://github.com/akitaonrails/ai-memory/compare/v1.11.1...HEAD
[1.11.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.11.1
[1.11.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.11.0
[1.10.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.10.1
[1.10.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.10.0
[1.9.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.9.1
[1.9.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.9.0
[1.8.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.8.0
[1.7.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.7.1
[1.7.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.7.0
[1.6.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.6.0
[1.5.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.5.0
[1.4.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.4.1
[1.4.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.4.0
[1.3.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.3.0
[1.2.2]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.2.2
[1.2.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.2.1
[1.2.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.2.0
[1.1.3]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.1.3
[1.1.2]: https://github.com/akitaonrails/ai-memory/compare/v1.1.1...v1.1.2
[1.1.1]: https://github.com/akitaonrails/ai-memory/compare/v1.1.0...v1.1.1
[1.1.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.1.0
[1.0.11]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.0.11
[1.0.10]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.0.10
[1.0.9]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.0.9
[1.0.8]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.0.8
[1.0.7]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.0.7
[1.0.6]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.0.6
[1.0.5]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.0.5
[1.0.4]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.0.4
[1.0.3]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.0.3
[1.0.2]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.0.2
[1.0.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.0.1
[1.0.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v1.0.0
[0.16.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.16.0
[0.15.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.15.0
[0.14.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.14.0
[0.13.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.13.0
[0.12.3]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.12.3
[0.12.2]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.12.2
[0.12.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.12.1
[0.12.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.12.0
[0.11.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.11.0
[0.10.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.10.0
[0.9.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.9.0
[0.8.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.8.1
[0.8.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.8.0
[0.7.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.7.1
[0.7.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.7.0
[0.6.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.6.1
[0.6.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.6.0
[0.5.2]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.5.2
[0.5.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.5.0
[0.4.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.4.0
[0.3.2]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.3.2
[0.3.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.3.1
[0.3.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.3.0
[0.2.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.2.0
[0.1.3]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.1.3
[0.1.2]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.1.2
[0.1.1]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.1.1
[0.1.0]: https://github.com/akitaonrails/ai-memory/releases/tag/v0.1.0
