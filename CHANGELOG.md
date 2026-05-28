# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/akitaonrails/ai-memory/compare/v0.5.2...HEAD
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
