<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/logo-dark.png">
    <img alt="ai-memory" src="docs/logo-light.png" width="480">
  </picture>
</p>

> Long-term memory for AI coding agents. Quit Claude Code mid-task,
> start OpenAI Codex in the same directory, continue without
> re-explaining the architecture, the failed approaches, or the open
> questions.

[![Release](https://img.shields.io/github/v/release/akitaonrails/ai-memory)](https://github.com/akitaonrails/ai-memory/releases/latest)
[![Rust](https://img.shields.io/badge/rust-1.95+-blue)](rust-toolchain.toml)
[![License](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

## Support Matrix

| Area | Status | Notes |
|---|---|---|
| Linux | Supported | Primary Docker/server target and CI platform. Published Docker images support `linux/amd64` and `linux/arm64`. Native Arch/AUR packages include system and user systemd units. |
| macOS | Supported | Workspace tests run in CI; tagged releases publish native `ai-memory-macos-aarch64.tar.gz` and `ai-memory-macos-x86_64.tar.gz` binaries. The native binary is the recommended path on Apple Silicon. See [`docs/macos.md`](docs/macos.md). |
| Windows via WSL2 | Supported | Use the Linux install path inside WSL2 when the agent runs there. |
| Native Windows | Experimental | Tagged releases publish `ai-memory-windows-x86_64.zip` with `ai-memory.exe`; Docker Desktop wrapper and source builds are also available. Local supported profiles default to host-native hook commands; Claude Code may use its Windows exec form, while other agents use native single command strings matching their hook schema. PowerShell/Git Bash scripts are compatibility fallbacks. See [`docs/windows.md`](docs/windows.md). |
| Claude Code | Supported | MCP config + lifecycle hooks; native commands enforce capture exclusions. `install-mcp --session-aware` optionally enables per-session auto-scope isolation through a local stdio bridge. Optionally captures the assistant's final turn on `Stop` when installed with `--capture-assistant` and the server enables `capture_assistant` (double opt-in, off by default). |
| Codex | Supported | MCP config + lifecycle hooks; native commands enforce capture exclusions. No automatic true session-end hook, so run `ai-memory finalize-session` when you need a final summary/handoff. |
| Command Code | Supported | MCP config (`~/.commandcode/mcp.json`) + its four stable lifecycle-hook events (`~/.commandcode/settings.json`); native commands enforce capture exclusions and `SessionStart` injects handoffs. `Stop` is only a turn boundary, so use `ai-memory finalize-session --agent command-code` after the final turn. `ai-memory run command-code` adds exact v3 native-session resume and visible-event import; experimental unsandboxed Mods remain excluded. |
| Devin CLI | Supported | MCP config + lifecycle hooks. Hooks use Devin's `PostCompaction` event, inject handoffs via `hookSpecificOutput.additionalContext`, and omit subagent events because Devin does not expose them. |
| OpenCode | Supported | Remote MCP config + generated TypeScript plugin; generated plugin enforces capture exclusions. |
| Cursor | Supported | MCP config + lifecycle hooks. |
| Gemini CLI | Supported | MCP config + lifecycle hooks. |
| Oh My Pi / OMP | Supported | Use `--client omp` / `--agent omp` (or `oh-my-pi`) for native `.omp` MCP config + TypeScript extension; generated extension enforces capture exclusions. |
| Pi | Supported | Generated `~/.pi/agent/extensions/ai-memory-pi.ts` extension provides lifecycle capture and an HTTP MCP bridge; generated extension enforces capture exclusions. |
| Crush | Managed-only | `ai-memory run crush` resumes its project-local session database and supplies portable context through a temporary supported global-context file; no lifecycle-hook installer is provided. |
| Managed workstreams | Opt-in | `ai-memory run` provides transparent cross-harness continuity for Claude Code, Codex, OpenCode, Pi, Crush, Kimi Code, Command Code, both incompatible Kiro CLI engines, OMP, Grok Build CLI, and Antigravity CLI. Direct launches remain unchanged. See [`docs/managed-workstreams.md`](docs/managed-workstreams.md). |
| Claude Desktop | MCP-only | Uses `mcp-remote`; no lifecycle hooks. |
| OpenClaw | Supported | MCP config + native plugin lifecycle hooks; generated plugin enforces capture exclusions. |
| Antigravity CLI | Supported | MCP config (`serverUrl`) + lifecycle hooks (`agy` alias). Only `PreInvocation` with `invocationNum = 0` maps to SessionStart; later model calls cannot consume a next-session handoff. No automatic true session-end hook, so run `ai-memory finalize-session --agent antigravity-cli` after the final turn when you need a summary, handoff, and opt-in SessionEnd consolidation. `ai-memory run antigravity` (aliases `antigravity-cli`, `agy`) adds managed workstream resume via `--conversation`; conversation text is not decoded, so the ledger for this harness comes from hook capture. |
| Grok Build CLI | Supported | MCP config (`install-mcp --client grok` → `$GROK_HOME/config.toml`, default `~/.grok/config.toml`) + lifecycle hooks (`install-hooks --agent grok` → `$GROK_HOME/hooks/ai-memory.json`, default `~/.grok/hooks/ai-memory.json`, Grok-specific hook bundle). Capture works; no hook handoff injection — Grok ignores `SessionStart` stdout, so recover handoffs via MCP `memory_handoff_accept`. `ai-memory run grok` adds managed workstream resume with the context packet delivered natively through `--rules`. Skills root: `.grok/skills` / `$GROK_HOME/skills` (default `~/.grok/skills`). |
| Swival CLI | MCP-only | `install-mcp --client swival --apply` merges a native HTTP entry into the project-root `.swival/mcp.json`, preserving sibling servers. Lifecycle and managed-workstream support are not claimed because Swival's callback contract does not expose a stable session identifier. |
| Zero | Supported | `install-mcp --client zero` (native HTTP + bearer in `~/.config/zero/config.json`) + lifecycle hooks via `install-hooks --agent zero --apply` (exec-form native commands in `~/.config/zero/hooks.json`, JSON payload on stdin, no shell). Capture works incl. specialist (subagent) events; no handoff injection — Zero discards `sessionStart` stdout, so recover handoffs via MCP `memory_handoff_accept`. |
| ZCode | MCP-only | `install-mcp --client zcode --apply` merges a native HTTP entry (strict schema: `type`/`url`/`headers` only) into the `mcp.servers` map of `~/.zcode/cli/config.json`, preserving sibling servers. Lifecycle hooks are not yet available — see #512. |
| Kimi Code | Supported | MCP config (`url` entry in `~/.kimi-code/mcp.json`) + lifecycle hooks (`[[hooks]]` in `~/.kimi-code/config.toml`, 10 events including subagent start/stop and `PostToolUseFailure` for tool-failure capture); both paths honor `$KIMI_CODE_HOME`. Handoffs inject via `UserPromptSubmit` stdout (Kimi Code discards `SessionStart` hook stdout); `ai-memory run kimi` adds managed workstream resume. |
| Kiro CLI | Supported | MCP config uses `install-mcp --client kiro-cli` (alias `kiro`) and Kiro's Bedrock-compatible schema flavor. `install-hooks --agent kiro-cli` merges v2 hooks into existing agent configs; the explicit `--agent kiro-cli-v3` target writes the incompatible standalone v3 registration. Both preserve unrelated entries, honor `$KIRO_HOME`, enforce capture exclusions, and inject pending handoffs at session start. Kiro has no true SessionEnd hook; use `ai-memory finalize-session --agent kiro-cli`, with `--session-id <uuid>` for concurrent sessions. `ai-memory run kiro` manages v2; add `--v3`, `--mode`, or `--agent-engine v3` for version-safe v3 resume. |
| Pool | Hooks-only | Poolside Agent CLI (`pool`). Lifecycle-hook capture via `install-hooks --agent pool` (alias `poolside`): Pool reads project-scoped hooks from the repo-root `.poolside/settings.yaml`, so ai-memory stages the scripts and prints a ready-to-paste `hooks:` snippet rather than writing project-local files; native commands enforce capture exclusions. Five Claude-shaped events (`SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop`) and no true session-end — `Stop` is a turn boundary, so run `ai-memory finalize-session --agent pool` after the final turn. `SessionStart` stdout injection is not demonstrated, so capture works but handoff injection does not — recover handoffs via MCP `memory_handoff_accept`. No first-party `install-mcp` client and no managed workstream (`ai-memory run pool`) are claimed: Pool's native session-store contract is not demonstrated (see [`docs/managed-harness-contributions.md`](docs/managed-harness-contributions.md)). Verified against Poolside CLI v1.0.16. |
| ZCode | Hooks-only | ZCode (z.ai, `zcode`, alias `zai`). Lifecycle-hook capture via `install-hooks --agent zcode --apply`: exec-form native commands (`type: "process"`, no shell) merged into the root `hooks` block of `~/.zcode/cli/config.json` around any third-party hooks; native commands enforce capture exclusions. Six documented triggers (`SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `Stop`); `PostToolUseFailure` fires instead of `PostToolUse` when a tool throws and lands on the same capture channel with the error preserved. `PermissionRequest` is deliberately not installed — its hook chain races the interactive permission client, so passive capture of that event is unreliable by design. No true session-end — `Stop` is a per-turn boundary, so run `ai-memory finalize-session --agent zcode` after the final turn. Unlike Pool and Zero, `SessionStart` stdout injection works (`hookSpecificOutput.additionalContext`, verified live against the embedded engine v0.16.5), so the prior session's handoff is delivered automatically. No first-party `install-mcp` client and no managed workstream are claimed yet. |
| VS Code Copilot | MCP-only | `.vscode/mcp.json` for Copilot agent mode; no lifecycle hooks (Copilot does not expose them yet). |
| Zed | MCP-only | Native remote MCP under `context_servers` in Zed's user `settings.json`; no lifecycle hooks or managed-workstream support. |
| Hermes Agent | Community | Core hook ingestion recognizes `agent=hermes` and Hermes' documented shell-hook `tool_name` / `tool_input` payload for concrete session attribution, tool-family titles, and capture exclusions. A community-maintained [`ai-memory-hermes-plugin`](https://github.com/MrLuciano/ai-memory-hermes-plugin) is available, but no first-party installer is shipped; review its compatibility matrix, install/uninstall scripts, and secret handling before using it. Hermes ignores session-start hook stdout, so recover handoffs through MCP. |
| LLM/auth providers | Supported | Anthropic, OpenAI, OpenAI OAuth/Codex, GitHub Copilot, Gemini, OpenCode Zen/Go, OpenAI-compatible endpoints, and generic OIDC device auth for native hooks. |
| Embedding providers | Supported | OpenAI, Voyage, Google Gemini, and keyless OpenAI-compatible endpoints such as Ollama, LM Studio, and vLLM. |

## What it is

LLM coding agents lose context when a session ends. ai-memory gives them a
shared, persistent wiki compiled from sanitized lifecycle observations. When a
session ends, relevant observations become a coherent summary; the next agent
receives a bounded handoff. Optional `ai-memory run` launches add a portable
visible-event ledger and native per-harness resume for higher-fidelity
cross-harness continuity.

The wiki is plain markdown in a git repo - `grep`-able, openable in
Obsidian, backed up with `rsync`. No vector database to babysit, no
`write_note` ceremony, no manual context-loading. The full design is
in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md); the influences and
priors are at the [bottom](#influences-and-prior-art).

## Key features

- **Zero-friction lifecycle capture.** Hooks fire-and-forget bounded,
  sanitized prompt, tool-lifecycle, and session-boundary observations. Direct
  launches keep this lightweight path; it is not a complete native transcript.
  User prompts and post-compaction summaries retain up to 16 KiB;
  notifications and tool excerpts retain up to 2 KB, with a 16 KiB durable
  backstop for every observation body.
- **Opt-in managed workstreams.** `ai-memory run claude`, then `ai-memory run
  codex --yolo`, then `ai-memory run command-code`, transparently resumes one
  logical workstream with native per-harness sessions, a portable visible-event
  ledger, and full-ledger search. Delivered packets are origin-marked; Claude
  transcript import rejects a packet that Claude persisted and read back through a tool.
  `ai-memory run` with no harness continues the newest usable Claude Code,
  Codex, OpenCode, Pi, Crush, Kimi Code, Command Code, or Kiro CLI v2/v3
  session for this checkout.
  On first
  explicit use, an interactive launcher can adopt a previous session from the
  same checkout; later switches cannot select unrelated native history. Native
  arguments pass through unchanged except the wrapper-owned `--yolo` and
  `--fresh`; direct
  commands are unaffected. `kimi-code` and `kimi-cli` are accepted aliases for
  the installed `kimi` command; `commandcode`, `cmdc`, and `cmd` select the
  cross-platform `command-code` executable (`cmdc` on native Windows); and
  `kiro-cli` selects the installed `kiro-cli` command. Kiro defaults to v2;
  `ai-memory run kiro --v3` selects v3, while a
  returning linked v3 workstream selects its engine transparently.
- **Per-repository capture exclusions.** A nearest-marker `[capture]`
  `ignore_paths` policy drops matching recognized file-tool events before they
  reach the local spool or server. See [the capture policy reference](docs/marker-file.md#capture-exclusions).
- **Opt-in capture scope.** `install-hooks --capture-mode allowlist` inverts
  the default so a repository without a marker emits no lifecycle event at
  all, dropped by the native hook before the spool. Forgetting a marker then
  costs recall rather than confidentiality. Enforced by native `ai-memory
  hook` commands only — see [allowlist mode](docs/marker-file.md#allowlist-mode-the-marker-as-an-opt-in).
- **Optional per-operator memory slots.** On shared servers,
  `[slots] per_user = true` keeps engine-written `_slots/` context in a bounded
  namespace derived from the authenticated operator. Session briefs and
  consolidation prompts receive shared slots plus the caller's own; exact wiki
  reads and searches remain project-wide, so this is context-injection
  isolation rather than RBAC. See [multi-user operation](docs/users.md#per-operator-memory-slots).
- **Cross-agent handoffs.** Quit Claude Code mid-task, start Codex
  in the same directory hours later - the next agent sees a
  "where you left off" block before its first prompt.
- **Per-project isolation by construction.** Each project lives at
  `<wiki_root>/<workspace_id>/<project_id>/…` keyed by stable UUIDs.
  Workspace defaults to `"default"`. Project is derived from `$cwd`:
  CLI subcommands (`bootstrap`, `write-page`, `lint`, …) walk to the
  main git repo root so all worktrees of the same repo share one
  project identity; the hook router defaults to `basename($cwd)` and
  can opt into the repo-root rule. Drop a
  [`.ai-memory.toml` marker file](docs/marker-file.md) in any
  ancestor directory to override either field explicitly — perfect for
  multi-client consultancies, work/personal split, mono-repos, or
  linked git worktrees.
  Same page path can exist in two projects without collision; a
  rename is one column update; a purge is one `rm -rf`.
- **Global preferences scope.** Standing user/team context — tech
  choices, code style, durable personal rules — lives in the reserved
  `_global` scope (`memory_write_page` with `scope: "global"`). Default
  `memory_query` reads union it into every project as
  `global_scope_hits`, so preferences travel with you into new projects
  without naming a magic project or paying the all-projects
  `global=true` fan-out. Event capture never writes there.
- **Entity-assisted recall.** Consolidation stores up to 10 specific nouns per
  page in canonical `entities:` frontmatter. Exact, prefix, and compound-word
  matches form a project-scoped RRF stream, so a query can recover a page even
  when its body uses different wording. The stream is lexical and adds no
  query-time LLM call.
- **Authority-aware recall.** FTS5, entity-match RRF, graph-neighbor RRF, and
  optional vector RRF generate candidates by relevance. Before truncation, a
  bounded adjustment favors maintained `_rules/`, `decisions/`, `procedures/`, and
  `gotchas/` pages over closely matching episodic session evidence. Tier,
  `pinned`, and explicit `canonical` / `active` / `source-of-truth` or
  `superseded` / `historical` / `test-fixture` / `do-not-answer-from` tags
  contribute without becoming absolute filters, so targeted history searches
  still find session pages. These signals affect retrieval provenance only;
  retrieved text remains untrusted historical evidence and never gains
  instruction authority from its namespace, tier, tags, pin, or rank.
- **Clear routing alongside code-intelligence tools.** Run ai-memory beside a
  structural MCP server, LSP, or other live-code tool without synchronizing
  their stores. Use memory for prior decisions, rationale, failed attempts,
  procedures, and handoffs; use the current checkout and structural provider
  for symbols, callers, dependencies, and impact analysis. Verify historical
  code claims against the checkout before acting, and treat source, builds,
  tests, and observed runtime behavior as operational truth. See
  [Historical memory and live code intelligence](docs/usage.md#historical-memory-and-live-code-intelligence).
- **Karpathy-style LLM wiki.** Pages are compiled from observations
  at session-end (or PreCompact; clients without a true session-end event can
  use `ai-memory finalize-session --agent <agent>` for a manual final close),
  not retrieved over raw logs.
  Supersession chain + git-versioned markdown means you can
  time-travel with `ai-memory checkpoints`, `restore-page`, or raw `git log`.
- **Built-in `/web` browser.** Read-only HTML UI for the wiki -
  project list, folder tree, FTS5 search, markdown rendering, dark
  mode. Mounted on the same axum server as MCP.
- **Server-wide MCP client activity.**
  `GET /admin/activity/by-client?since_days=7` shows which MCP clients are
  calling memory tools, split into reads and writes. Counts use bounded UTC-day
  buckets, so arbitrary client names cannot grow the database with request
  volume; shared deployments keep the endpoint root-only. See
  [MCP client activity](docs/users.md#mcp-client-activity).
- **Multi-agent + multi-machine ready.** Supported clients: Claude
  Code, Codex, Command Code, Devin CLI, OpenCode, Cursor, Claude Desktop (via `mcp-remote`),
  Gemini CLI, Antigravity CLI, Grok Build CLI, Kimi Code, OpenClaw, Oh My Pi
  / OMP (`omp` / `oh-my-pi`), Pi via generated bridge extension, VS Code
  GitHub Copilot agent mode (MCP-only, workspace `.vscode/mcp.json`), Kiro CLI
  (MCP + v2 lifecycle hooks), Pool (hooks-only, project
  `.poolside/settings.yaml` snippet), ZCode (hooks-only, exec-form native
  commands in `~/.zcode/cli/config.json`), and Zed (MCP-only, user `settings.json`).
  Server runs local (loopback) OR on a homelab box (LAN/VPN/cloud)
  with bearer-token auth. Shared servers can opt into
  [`[auto_scope]` modes](docs/auto-scope.md) for per-user or
  session-aware current-project routing; Claude Code has a built-in opt-in
  bridge via `install-mcp --session-aware`.
- **Thin-client CLI.** `ai-memory status`, `bootstrap`, `checkpoints`,
  `restore-page`, `purge-project`, `purge-session`, `rename-project`, `move-project`,
  `move-session`,
  `audit-contamination`, `lint`, `curator`, `auto-improve`,
  `auto-improve-report`, `pending-writes`, `embed`, `forget-sweep`, `backup`,
  `finalize-session` are
  all HTTP clients of the running server - never touch SQLite or
  wiki files directly. `status` also reports passive LLM/embedding
  provider health from the last real provider call. Server is the
  single source of truth. `finalize-session` lists matching open
  sessions through `GET /admin/open-sessions`, then posts synthetic
  `session-end` hooks back to the server. On shared deployments it defaults to
  the caller's own plus unattributed sessions; root can pass `--all-owners` for
  explicit cross-operator recovery. When concurrent sessions share an agent and
  scope, pass `--session-id <uuid>` to target one exact open session; it cannot
  be combined with `--all`.
- **LLM is opt-in.** Zero-LLM mode still gives you FTS5, manually declared
  entity, and graph-neighbor search plus rule-based summarisation. Add a
  provider when you want consolidated pages, lint contradictions, or staged
  auto-improvement proposals.

## Use cases

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
  Codex, OpenCode, Pi, Crush, Kimi Code, Command Code, Kiro CLI v2/v3, OMP,
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
  the wiki at `http://<server>:49374/web` - HTTP Basic dialog if
  auth is on, paste the token as password. Per-project tree view,
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

## Quick start

### Arch Linux (AUR)

For native Arch installs, use the AUR packages. They install
`/usr/bin/ai-memory`, packaged hook sources, and both system-level and
user-level systemd units.

```bash
yay -S ai-memory-bin    # prebuilt Linux x86_64/aarch64 binary
yay -S ai-memory        # builds from source
```

Single-user workstation:

```bash
mkdir -p ~/.config/ai-memory ~/.local/share/ai-memory
ai-memory --data-dir ~/.local/share/ai-memory \
  --config ~/.config/ai-memory/config.toml init
systemctl --user enable --now ai-memory.service
ai-memory install-mcp --client claude-code --apply
ai-memory install-hooks --agent claude-code --apply
```

System service installs use `/var/lib/ai-memory` and `/etc/ai-memory/` via the
packaged unit. Full user-service, system-service, auth, and provider setup is in
[`docs/install.md#arch-linux-native-packages-aur`](docs/install.md#arch-linux-native-packages-aur).

### Docker

You need: Docker + an agent CLI from the [Support Matrix](#support-matrix), or
anything else that speaks MCP.

The published Docker image includes `linux/amd64` and `linux/arm64` variants,
so Apple Silicon Macs and ARM64 Linux hosts can pull `akitaonrails/ai-memory`
without `--platform linux/amd64` emulation.

The default quick-start has **no authentication** - the server binds
to loopback only, so on a single-user laptop nothing else can reach
it. Adding a bearer token is a one-line change once you're ready to
expose the server on the LAN; see [Security](#security) below.

```bash
# 1. Install the ai-memory CLI wrapper (a small shell script that
#    runs the binary inside docker with your $HOME mounted). This is
#    the only thing that needs to live on the host filesystem.
mkdir -p ~/.local/bin
wrapper_tmp="$(mktemp -d)"
trap 'rm -rf "$wrapper_tmp"' EXIT
wrapper_base=https://github.com/akitaonrails/ai-memory/releases/latest/download/ai-memory-wrapper
curl -fsSL "$wrapper_base" -o "$wrapper_tmp/ai-memory-wrapper"
curl -fsSL "$wrapper_base.sha256" -o "$wrapper_tmp/ai-memory-wrapper.sha256"
expected="$(awk 'NR == 1 { print $1 }' "$wrapper_tmp/ai-memory-wrapper.sha256")"
if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$wrapper_tmp/ai-memory-wrapper" | awk '{ print $1 }')"
else
    actual="$(shasum -a 256 "$wrapper_tmp/ai-memory-wrapper" | awk '{ print $1 }')"
fi
[ -n "$expected" ] && [ "$actual" = "$expected" ] || { echo "wrapper checksum mismatch" >&2; exit 1; }
install -m 0755 "$wrapper_tmp/ai-memory-wrapper" ~/.local/bin/ai-memory
rm -rf "$wrapper_tmp"
trap - EXIT
# Most distros put ~/.local/bin on PATH automatically. If `which
# ai-memory` comes up empty, add this to ~/.bashrc / ~/.zshrc:
#     export PATH="$HOME/.local/bin:$PATH"

# 2. Start the server. `--restart unless-stopped` makes it come back
#    on docker daemon restart and on machine boot (provided your
#    docker service is enabled at boot — `sudo systemctl enable
#    docker` on most distros). Loopback-only bind (`127.0.0.1:49374`)
#    so nothing outside this machine can reach it. Omit the LLM /
#    EMBEDDING lines for zero-LLM mode — FTS5 search still works
#    without any keys.
docker run -d --name ai-memory \
    --restart unless-stopped \
    -p 127.0.0.1:49374:49374 \
    -v ai-memory-data:/data \
    -e AI_MEMORY_LLM_PROVIDER=anthropic \
    -e ANTHROPIC_API_KEY=sk-ant-... \
    -e AI_MEMORY_EMBEDDING_PROVIDER=openai \
    -e OPENAI_API_KEY=sk-... \
    akitaonrails/ai-memory:latest

# 3. Wire your agent CLI in two commands. The wrapper takes care of
#    mounts and each client's config-path detection. Re-run with
#    `--agent codex`, `--agent command-code`, `--agent devin`, `--agent opencode`, `--agent gemini-cli`,
#    `--agent grok`, `--agent kimi-code`, `--agent kiro-cli`, `--agent omp`,
#    `--agent oh-my-pi`, `--client cursor`,
#    `--client gemini-cli`, `--client grok`, `--client kiro-cli`, etc.
#    for additional agents; full list in docs/install.md.
ai-memory install-mcp   --client claude-code --apply
ai-memory install-hooks --agent  claude-code --apply
# Grok Build CLI example:
# ai-memory install-mcp   --client grok --apply
# ai-memory install-hooks --agent  grok --apply
# Kiro CLI v2 example (requires an existing Kiro agent config):
# ai-memory install-mcp   --client kiro-cli --apply
# ai-memory install-hooks --agent  kiro-cli --apply
# Kiro CLI v3 example (standalone hook registration):
# ai-memory install-hooks --agent  kiro-cli-v3 --apply
# Command Code stable MCP + lifecycle example:
# ai-memory install-mcp   --client command-code --apply
# ai-memory install-hooks --agent  command-code --apply
# Pool (Poolside Agent CLI) example — stages scripts and prints the
# .poolside/settings.yaml snippet to paste into each repo (no MCP client yet):
# ai-memory install-hooks --agent  pool --apply
# ZCode (z.ai) example — merges exec-form native commands into the root
# hooks block of ~/.zcode/cli/config.json (handoff injection works; no true
# session-end, so run `ai-memory finalize-session --agent zcode` at the end):
# ai-memory install-hooks --agent  zcode --apply
```

On Linux/macOS, that's it. Start a Claude Code session as usual - every
prompt and tool call now lands in ai-memory, and the next session you
open in this project will see a handoff with where you left off.
On macOS, the native release binary is also supported and recommended when you
do not need Docker; see [`docs/macos.md`](docs/macos.md).

If two Claude Code sessions use the same server concurrently, enable
per-session routing on the server and replace only the `ai-memory` MCP entry
with the optional bridge:

```toml
# <ai-memory data dir>/config.toml
[auto_scope]
mode = "per_session"
```

```bash
ai-memory install-mcp --client claude-code --session-aware --apply
```

The bridge still connects to the configured local or remote HTTP server and
forwards its bearer token, but also attaches Claude's lifecycle session id to
every MCP request. Existing static HTTP installs remain the default. See
[`docs/auto-scope.md`](docs/auto-scope.md) for Claude's `/clear` and
implicit-resume limitations.

The `install-mcp` / `install-hooks` commands use
`AI_MEMORY_SERVER_URL` / `AI_MEMORY_AUTH_TOKEN` when set; otherwise
they default to `http://127.0.0.1:49374` (matching the server above)
and no bearer token. If hooks are installed after an ai-memory MCP
entry already exists, `install-hooks` reuses that endpoint so a remote
MCP setup cannot silently regenerate loopback-only hooks. Both commands
are idempotent - re-runs replace ai-memory's entry, preserve every
other server / hook you have configured, and write a timestamped
`.bak-<ts>` next to the file before each modifying write. The hook
scripts are staged into `~/.local/share/ai-memory/hooks/<agent>/`
automatically; re-running overwrites them so future image updates ship
updated hooks. Drop `--apply` to print the snippet instead of mutating.
For Claude Code, `CLAUDE_CONFIG_DIR` relocates MCP registration to
`$CLAUDE_CONFIG_DIR/.claude.json`, hooks to
`$CLAUDE_CONFIG_DIR/settings.json`, and global managed skills to
`$CLAUDE_CONFIG_DIR/skills`. The Docker wrapper forwards this variable when
the directory is under its existing `$HOME` bind mount. Use the native binary
when the Claude config root is outside `$HOME`. Uninstall checks both the
active relocated paths and Claude's home defaults, so enabling the variable
does not leave an older default-path ai-memory installation behind.
If your agent often starts inside repository subdirectories or linked
worktrees, add `--project-strategy repo-root` to `install-hooks` so captures
collapse to the main git repo name; see [`docs/install.md`](docs/install.md)
and [`docs/marker-file.md`](docs/marker-file.md) for details. Later bare
`--apply` refreshes, including `ai-memory upgrade`, preserve that choice;
pass `--project-strategy basename` explicitly to remove it.

The Docker wrapper also bridges thin-client commands such as
`ai-memory status` and `ai-memory bootstrap` back to the host's
loopback server. With the local Docker quick start above, no
`AI_MEMORY_SERVER_URL` override is needed.

Managed workstreams are optional. They execute the harness on the host while
the server may remain local or remote:

```bash
ai-memory run claude
# later, continue the same workstream in another harness
ai-memory run codex --yolo
# omit the name to continue the newest usable local harness session
ai-memory run
# or resume the newest managed checkout without changing directories first
ai-memory continue
```

To remove ai-memory later, run `ai-memory uninstall --apply` from the
same host environment. It removes ai-memory-owned config entries, instruction
blocks, default-root managed skill files, and generated plugin files only after
matching their ai-memory signatures; custom skill roots installed with
`--target-dir` are cleaned up manually. Use `--mcp-url` if you installed MCP
with a custom endpoint, and `--mcp-name` only when you need to narrow removal to
one matching entry.

### Install Notes

- **SELinux:** on enforcing Linux hosts, the wrapper automatically adds
  `--security-opt label=disable` only to short-lived helper commands that touch
  bind-mounted host files. It does not alter the long-lived server container
  or relabel `$HOME`; do not add `:z`/`:Z` to the whole home bind. Rootless
  engines also get `-u 0:0` for those commands. Docker and podman report
  rootless mode and SELinux support under different `info` keys; both are
  read. The same treatment applies whenever `AI_MEMORY_DATA_DIR` selects a
  host directory or an explicit `--config` reads a host file. See
  [`docs/install.md`](docs/install.md#selinux-enforcing-hosts).
- **Windows:** use the Linux path inside WSL2, or the native Windows wrapper
  from PowerShell/cmd. Local supported profiles default to host-native commands:
  Claude Code may use its supported `ai-memory.exe` exec form, while other
  agents use native single command strings matching their hook schema. The
  Docker wrapper protects `.ps1` fallback commands from nested PowerShell
  expansion with `-EncodedCommand`; rerun
  `install-hooks --agent <agent> --apply` after upgrading so existing hook
  entries receive the current form.
  PowerShell/Git Bash script bundles are compatibility fallbacks and do not
  enforce capture-policy v1. Do not mix path worlds. See
  [`docs/windows.md`](docs/windows.md).
- **Docker compose:** `docker compose -f docker/docker-compose.yml up -d`
  is supported; agent setup is the same as step 3 above.
- **Remote server:** set `AI_MEMORY_SERVER_URL=http://<server-ip>:49374`
  and `AI_MEMORY_AUTH_TOKEN=<token>` on the client before installing
  MCP/hooks. Explicit `--server-url` flags still work, but are no longer
  required when the env vars are set. Any non-loopback server should use
  bearer auth.
- **Managed-launch wrapper:** `ai-memory run`, `ai-memory show`,
  `ai-memory continue`, and `ai-memory workstreams` must be intercepted by the
  current host wrapper so local checkouts, native harnesses, and session stores
  remain accessible. An old wrapper may pass these commands into Docker and
  fail to find a checkout or host executable. Run
  `ai-memory upgrade` on the agent machine to refresh it. The host-native runner
  inherits `AI_MEMORY_SERVER_URL`, `AI_MEMORY_AUTH_TOKEN`, and the host `PATH`.
- **Upgrades:** for Docker-wrapper installs, run `ai-memory upgrade` on each
  agent machine. It refreshes the local wrapper, pulls the latest image, and
  re-stages hook scripts under `~/.local/share/ai-memory/hooks/<agent>/`.
  Native package/source installs should rerun
  `ai-memory install-hooks --agent <agent> --apply` after upgrading the binary.
  Remote/homelab servers must still be redeployed separately; local wrapper
  upgrade only updates the client machine. Existing project prompt files keep
  working. Refresh the managed ai-memory routing package
  (`ai-memory install-instructions`, or `--target AGENTS.md` for AGENTS-based
  projects) when you want new tool guidance. The refresh writes the slim
  markered snippet and managed Agent Skills from the same binary-owned assets.
  Managed skill payloads use LF line endings on every release platform, while
  user-authored files keep their existing line endings.

For every client in the [Support Matrix](#support-matrix), plus curl-based hook
installs, source builds, CLI environment variables, and the full subcommand
reference, see [`docs/install.md`](docs/install.md).

Tab completion for the CLI is available in bash, zsh, fish, PowerShell, and
elvish:

```bash
ai-memory completions fish > ~/.config/fish/completions/ai-memory.fish
```

See [`docs/shell-completions.md`](docs/shell-completions.md) for the other
shells' install paths.

## Security

Loopback-only (`127.0.0.1:49374`) with no auth is the default because
it is safe for a single-user laptop: no process outside the machine can
reach the server.

Unauthenticated non-loopback HTTP now fails closed. Set
`AI_MEMORY_AUTH_TOKEN` or bind loopback; `--allow-insecure-no-auth` is an
intentional, dangerous exception for plain HTTP only. Authentication does not
encrypt bearer tokens: for LAN or remote access, use the ready
[Caddy](docker/compose.tls.caddy.yml) or
[Cloudflare Tunnel](docker/compose.tls.cloudflared.yml) templates described in
the [HTTPS reverse-proxy guide](docs/https-via-proxy.md).

Enable bearer auth when the server is exposed beyond loopback, when
untrusted local processes share the machine, or when the data dir holds
sensitive project history:

```bash
TOKEN=$(ai-memory generate-auth-token)

docker run -d --name ai-memory \
    --restart unless-stopped \
    -p 0.0.0.0:49374:49374 \
    -v ai-memory-data:/data \
    -e AI_MEMORY_AUTH_TOKEN="$TOKEN" \
    -e AI_MEMORY_ALLOWED_HOSTS="<server-ip>,localhost,127.0.0.1" \
    akitaonrails/ai-memory:latest

ai-memory install-mcp   --client claude-code --apply \
    --server-url "http://<server-ip>:49374/mcp" --auth-token "$TOKEN"
ai-memory install-hooks --agent  claude-code --apply \
    --server-url "http://<server-ip>:49374" --auth-token "$TOKEN"
```

Bearer auth protects `/mcp`, `/hook`, `/handoff`, `/admin/*`, and
`/web/*`. Browser access to `/web` uses HTTP Basic auth with the token
as the password. When `/web` is exposed through an HTTPS reverse proxy, set
`AI_MEMORY_AUTH__SECURE_COOKIE=true`; it makes the browser cookie HTTPS-only.
Close or redirect direct HTTP access to that hostname. Non-loopback binds should also set
`AI_MEMORY_ALLOWED_HOSTS` to guard against DNS rebinding.

Busy shared hook servers can also set `AI_MEMORY_HOOK_RATE_PER_SEC` (tokens per
second per actor/session source) and optionally `AI_MEMORY_HOOK_RATE_BURST` to
bound one runaway session without blocking unrelated hook sources. Unset or `0`
rate leaves the limiter disabled.

For shared servers where each developer should authenticate their own hook
writes, native Claude Code hooks can use a stored OIDC device token instead of
embedding a shared static token:

```bash
ai-memory auth login oidc-device \
    --issuer "https://issuer.example.com/realms/team" \
    --client-id "ai-memory-cli"

ai-memory install-hooks --agent claude-code --apply \
    --server-url "http://<server-ip>:49374"
```

OIDC hook auth requires the native `ai-memory hook ...` command path. The Docker
wrapper keeps shell-script hooks by default; set up OIDC from a native release
binary or source install. Thin-client HTTP commands such as `ai-memory status`
and `ai-memory search` also use the stored OIDC access token when no static
`AI_MEMORY_AUTH_TOKEN` / `[auth].bearer_token` is configured; the static bearer
still wins when present. This is for OIDC-aware gateways/bridges; native
ai-memory server auth still accepts static root bearer / DB-user tokens, and
`/admin/*` remains root-only unless a gateway translates accepted OIDC auth into
upstream auth that ai-memory accepts.

OIDC/Keycloak session ids are login-provider sessions, not ai-memory agent
sessions. Shared servers that rely on `[auto_scope]` session isolation still
need explicit `workspace` + `project` / `scopes`, or a bridge that forwards the
real lifecycle-hook session id on MCP requests.

**Want HTTPS?** ai-memory deliberately does not terminate TLS itself —
the right answer is a battle-tested reverse proxy in front of it.
[`docs/https-via-proxy.md`](docs/https-via-proxy.md) is the deployment
guide, with copy-paste docker compose templates in
[`docker/compose.tls.caddy.yml`](docker/compose.tls.caddy.yml) (Caddy
with Let's Encrypt or internal CA) and
[`docker/compose.tls.cloudflared.yml`](docker/compose.tls.cloudflared.yml)
(Cloudflare Tunnel — no open ports). Both are recommended once you
turn on multi-user or bind beyond loopback. The Quick Start happy
path of single-user on loopback doesn't need TLS — that case is
called out explicitly in the guide so you don't add ceremony where
it doesn't earn its keep.

**Multi-user attribution (v0.8, optional).** When more than one human
shares a server, ai-memory can attribute each write to a named user.
The bearer token continues to authenticate at the wire level; users
created via `ai-memory user add` get their own tokens that resolve to
their identity in audit logs, page frontmatter, `/api/v1` responses, and the
page view UI. Data stays single-tenant — there is no per-page RBAC. A
`[auth].token_pepper` is required for DB-user authentication, but creating the
first user row is what immediately switches every `/admin/*` endpoint to
root-only, including status/search/read-page and user-management routes.
`ai-memory init` generates a pepper for new installs without changing
single-user behavior until a user is added. An SSO gateway can instead use a
dedicated `[auth].actor_proxy_bearer_token` and trusted `X-Memory-Actor-*`
headers; its credential is deliberately separate from the root bearer so a
missing identity cannot become root. See
[`docs/users.md`](docs/users.md) for the full walkthrough and the
four-rung auth ladder.

See [`docs/deploy.md`](docs/deploy.md) for the full homelab pattern
with bearer auth, host allowlisting, and TLS/reverse-proxy options.

## Using Memory

Day to day, you mostly do not think about ai-memory. Lifecycle hooks
capture prompts, tool calls, compaction checkpoints, and session
boundaries. SessionStart hooks fetch pending handoffs before your first
prompt in the next agent.

Useful entry points:

- Ask "where did we leave off?" to continue from the pending handoff.
- Ask "have we discussed X?" or "search memory for Y" to query the wiki.
- Ask "catch me up" for a prose digest of recent project activity.
- Run `ai-memory bootstrap` once when adopting ai-memory in an existing
  project with months of history.
- Start the server with `--enable-web` and visit `/web` for a read-only
  browser view of the markdown wiki. `--enable-web` also mounts a
  read-only JSON frontend API at `/api/v1` (workspaces, projects, pages,
  recent, briefing, search) so custom web UIs can read the memory without
  opening SQLite or wiki files directly. Rendered pages keep external links
  clickable, but image sources must be relative or root-relative; absolute
  external images are neutralized so viewing stored Markdown cannot act as a
  remote beacon:

  ```text
  GET  /api/v1/workspaces
  GET  /api/v1/projects?workspace=...
  GET  /api/v1/workspaces/{workspace}/projects/{project}/pages
  GET  /api/v1/workspaces/{workspace}/projects/{project}/pages/{path}
  GET  /api/v1/workspaces/{workspace}/projects/{project}/recent?limit=...
  GET  /api/v1/workspaces/{workspace}/projects/{project}/briefing?limit=...
  GET  /api/v1/workspaces/{workspace}/overview?limit=...
  GET  /api/v1/workspaces/{workspace}/projects/{project}/overview?limit=...
  GET  /api/v1/workspaces/{workspace}/projects/{project}/handoffs?state=...&limit=...
  GET  /api/v1/search?q=...&workspace=...&project=...&limit=...
  POST /api/v1/search   { "q": "...", "scopes": [{ "workspace": "...", "project": "..." }] }
  ```

  `overview` bundles the open handoff + briefing + memory-health for a workspace
  or project in one call (the data a project overview screen needs). The
  handoff history defaults to the caller's own plus shared rows; root can use
  `all_owners=true` for recovery across operators.

  **Full integration guide:** see [`docs/frontend-api.md`](docs/frontend-api.md)
  for auth setup, response schemas, error model, limits/pagination,
  custom-UI hosting, a worked `fetch`/`curl` example, and the canonical
  source-of-truth files. Read that first if you're building a frontend.

  To serve your own static frontend instead of the built-in UI, point
  `--web-ui-dir` at the frontend's build output (same-origin with
  `/api/v1`, `/mcp`, `/admin/*`, so the existing auth applies):

  ```bash
  ai-memory serve --transport http --bind 127.0.0.1:49374 \
    --enable-web --web-ui-dir ../ai-memory-ui/dist
  ```

  A reference implementation — a SolidJS knowledge browser with
  screenshots and e2e tests — lives at
  [djalmajr/ai-memory-ui](https://github.com/djalmajr/ai-memory-ui).

  Richer products such as import/migration pipelines and write-capable
  browser chat/editors should live as optional companion crates or projects
  that call ai-memory's public HTTP/MCP surfaces. The first implemented
  companion is the standalone OMC wiki importer at
  [`companions/ai-memory-importer`](companions/ai-memory-importer), which is
  intentionally not a root workspace member and is not included in root
  `cargo test --workspace`. See
  [`docs/companion-crates.md`](docs/companion-crates.md) for the boundary.

  When a reverse proxy hosts ai-memory under a URL subpath, set
  `--base-path` (or `AI_MEMORY_BASE_PATH`) so every HTTP surface moves
  together. Example: `--base-path /wiki` serves MCP at `/wiki/mcp`, hooks at
  `/wiki/hook`, the API at `/wiki/api/v1`, and the default browser at
  `/wiki/web`. Set `--web-slug /` if you want the browser or custom SPA at
  `/wiki` itself.

Install the managed routing package once so agents proactively call the
right MCP tool for those prompts:

```bash
ai-memory install-instructions
```

That command writes or updates the slim `<!-- ai-memory:start -->` block and
the managed ai-memory Agent Skills that carry the detailed routing guidance.
See [`docs/usage.md`](docs/usage.md) for handoff examples, proactive query
routing, bootstrap details, web UI screenshots, and the raw-wiki inspection
commands. CLI URL/auth configuration lives in
[`docs/install.md`](docs/install.md#configuring-the-cli-url-and-auth).

### Entity retrieval

Consolidation extracts specific technologies, components, services, files, and
domain nouns into each page's canonical frontmatter. Hand-edited wiki pages can
declare the same bounded index explicitly:

```yaml
---
title: Queue choice
entities:
  - nats jetstream
  - delivery guarantees
---
```

Names are lowercased, whitespace-normalized, de-duplicated, capped at 10 per
page and 64 characters each, and rebuilt from Markdown during a clean-store
`ai-memory reindex`.
Entity lookup is project-scoped, ignores expired pages by default, and reports
`entity_rank`, its raw inverse-frequency `entity_weight`, `matched_entities`,
and its RRF contribution under
`memory_query(..., explain: true)`.

## LLM Providers

ai-memory runs without an LLM: hooks still capture sessions, search uses
FTS5 + declared entities + graph neighbors, and summaries fall back to
rule-based output. Add an LLM provider
when you want LLM consolidation (on PreCompact, on demand via
`memory_consolidate`, or opt-in at session end with
`AI_MEMORY_CONSOLIDATE_ON_SESSION_END`), richer linting, and bootstrap.
Substantive session ends always write a rule-based summary page + handoff either
way. A session containing only `SessionStart` / `SessionEnd` boundaries is
closed without a page, handoff, or provider job. When that empty session had
accepted startup context, its session-bound handoff is returned to the open
pool for the next receiver instead of being lost.
When the session-end opt-in is enabled, provider work is durably queued after
those deterministic writes and handled by one bounded server worker, so hook
drain latency does not cancel it. Failed jobs retry with backoff and survive a
server restart. A resumed native session is ended again only after its
observation generation advances; the persisted generation watermark makes
duplicate SessionEnd delivery and system clock skew converge without repeated
provider work. The end watermark and automatic handoff commit atomically, and
an interrupted keyed replay finishes the wiki commit, queue insert, and key
completion without duplicating that handoff. On the next SessionStart, the
newest cwd-eligible automatic handoff wins; accepting it expires older eligible
automatic handoffs without consuming manual or sibling-directory work. A new
automatic handoff also expires prior open automatic handoffs from its exact
cwd, so repeated SessionEnds cannot accumulate there before a receiver starts.

To keep consolidation style project-specific, write
`_prompts/consolidation.md` in that project's wiki. Its body can express
preferences such as "prefer Portuguese titles" or "omit routine CI noise".
Automatic, single-page, and multi-page consolidation use the page; a manual
`memory_consolidate` call can pass `instructions` to override it once. ai-memory
sanitizes and caps the value at 2,000 characters, JSON-encodes it in the user
message, and treats it as untrusted advisory data. It cannot supply facts,
request tool use or disclosure, or override the consolidation schema and
faithfulness rules. TTL-expired preference pages are ignored. With no active
page or argument, no preference block is appended.

Recommended defaults:

| Provider | Default | Use when |
|---|---|---|
| `anthropic` | `claude-haiku-4-5` | Best default for consolidation quality and rule classification. |
| `anthropic-oauth` | `claude-sonnet-4-6` | Use a Claude Pro/Max subscription via `claude setup-token`, no API key. |
| `openai` | `gpt-5.4-mini` | Cheaper and faster hosted option. |
| `openai-oauth` | `gpt-5.5` | ChatGPT Pro/Plus/Codex backend via `ai-memory auth login openai-oauth`; no Platform API key. |
| `copilot` | `gpt-5.5` | GitHub Copilot Chat backend via `ai-memory auth login copilot` or `COPILOT_GITHUB_TOKEN`; requires a Copilot subscription. |
| `gemini` | `gemini-3.5-flash` | Google-hosted option with a generous free tier. |
| `openai-compat` | no default | OpenRouter, Atlas Cloud, OrcaRouter, Ollama, vLLM, LM Studio, and other compatible endpoints. |

`openai-oauth` stores a refresh token in `<data_dir>/auth.json` and talks to
the ChatGPT/Codex Responses backend, not `api.openai.com`. For Docker quick
starts, run `ai-memory auth login openai-oauth` with the wrapper so the token
lands in the same `ai-memory-data` volume as the server.

`anthropic-oauth` hits the same `/v1/messages` endpoint as `anthropic` but
authenticates with an OAuth bearer token instead of an API key. Run
`claude setup-token` once, then set `AI_MEMORY_LLM_PROVIDER=anthropic-oauth` and
`ANTHROPIC_OAUTH_TOKEN=<token>` (or `CLAUDE_CODE_OAUTH_TOKEN`, which `claude
setup-token` writes automatically). No `ANTHROPIC_API_KEY` is needed. The Docker
wrappers forward either token by name to short-lived helper commands such as
`llm-test`; configure the long-lived server container separately as shown in the
installation guide.

For both Anthropic providers, ai-memory omits `temperature` for Claude
4.7 and later models and Claude Mythos Preview because those models reject
non-default sampling parameters. `llm-test` sends the same representative 0.2
value as the normal pipeline before the provider applies that compatibility
rule.

**⚠️ Unofficial and against Anthropic's usage policies — use at your own risk;
it may get your account rate-limited or banned. See
[the warning in `docs/install.md`](docs/install.md#anthropic-via-claude-subscription-oauth).**

`copilot` stores a GitHub user token in the same auth file, exchanges it for a
short-lived Copilot API token via GitHub's `/copilot_internal/v2/token`, and
uses the Copilot Chat endpoint with `vscode-chat` integration headers. You can
also set `COPILOT_GITHUB_TOKEN`, `GH_TOKEN`, or `GITHUB_TOKEN` on the server.

> [!TIP]
> **For the OAuth/subscription backends (`anthropic-oauth`, `openai-oauth`,
> `copilot`), pick a small, fast model** via `AI_MEMORY_LLM_MODEL` — e.g.
> `claude-haiku-4-5` or `gpt-5-mini`. ai-memory's LLM work (consolidation,
> lint, explore) is summarisation, not hard reasoning, so a Haiku/mini-class
> model is plenty and is much easier on subscription rate limits. Save the
> high-effort thinking models for your coding agent.

> [!TIP]
> **OpenAI-compatible structured output is schema-constrained by default.**
> ai-memory sends each operation's JSON Schema through
> `response_format=json_schema`, which recent Ollama, vLLM, LM Studio, and
> llama.cpp releases honour. It falls back to the tolerant parser when an
> endpoint explicitly rejects that field or returns a malformed shape. Set
> `AI_MEMORY_LLM_COMPAT_STRICT=false` only for an incompatible endpoint.

For small-context local models, configure both consolidation limits. The input
target accounts for the complete rendered prompt, including bounded slot and
current-page context plus the structured-output schema; the output limit is
sent to the provider. Their sum must fit the model context window, with extra
headroom because provider tokenizers differ:

```toml
[consolidation]
max_input_tokens = 6500
max_output_tokens = 1000
```

The equivalent environment variables are
`AI_MEMORY_CONSOLIDATION__MAX_INPUT_TOKENS` and
`AI_MEMORY_CONSOLIDATION__MAX_OUTPUT_TOKENS`. Provider failures during an
automatic PreCompact/PostCompaction checkpoint fall back to the deterministic
rule-based page; admission, storage, and scope errors still fail closed. The
validated minimums are 6,000 input and 1,000 output tokens.

Every chat provider bounds each completion request at 300 seconds
(`AI_MEMORY_LLM_TIMEOUT_SECS` to override, or `llm_timeout_secs = 900` in
config.toml; the quick openai-oauth token refresh keeps the default ceiling).
The default tolerates a local engine cold-loading a large model; slow hosted
gateways whose long completions exceed the ceiling fail every request with
`http: error sending request`, so raise the value there instead of watching
consolidation exhaust its retries.

Reranking is optional and off by default. With an LLM provider configured,
`AI_MEMORY_RERANKER=llm` makes project and explicit-scope `memory_query`
calls over-fetch from the hybrid stage, fuse scopes, and make at most one LLM
call to reorder the best candidates. This can promote a relevant page that
RRF ranked below the requested cut, at the cost of LLM latency and usage. The
request sends the query plus at most 30 bounded page titles and search snippets
to the configured provider; all values are JSON-encoded and treated as
untrusted data. A timeout, provider error, or incomplete/invalid score set
preserves the normal order. `global=true` and supplemental global-preference
hits keep their existing non-RRF ranking. Concurrent provider calls are capped
at four; saturated queries keep their local ranking without waiting.

Embeddings are optional and separate from the LLM provider. Set
`AI_MEMORY_EMBEDDING_PROVIDER=openai`, `voyage`, `google`/`gemini`, or
`openai-compat` when you want vector retrieval in addition to FTS5 + entity +
graph-neighbor retrieval. `openai-compat` targets self-hosted engines
(Ollama, LM Studio, vLLM): it needs no API key and requires explicit
`AI_MEMORY_EMBEDDING_BASE_URL`, `AI_MEMORY_EMBEDDING_MODEL`, and
`AI_MEMORY_EMBEDDING_DIM`. The optional `EMBEDDING_API_KEY` credentials the
embedder alone and is checked before `OPENAI_API_KEY` and `LLM_API_KEY`, so
embeddings can run on a different provider than the LLM. Both the FTS-only and
hybrid paths apply the same bounded page-authority adjustment after candidate
generation; embeddings improve relevance recall but do not decide which source
is canonical.

See [`docs/install.md#llm-provider-tiers`](docs/install.md#llm-provider-tiers)
for env vars and Ollama/OpenRouter/Atlas Cloud/OrcaRouter examples, and
[`docs/llm-provider-comparison.md`](docs/llm-provider-comparison.md)
for the empirical model comparison.

## Architecture

One Rust binary runs an MCP/HTTP server and owns one data directory:

```text
<data_dir>/
├── wiki/    # markdown source of truth, git-versioned
├── raw/     # immutable sanitized managed-workstream transcript segments
├── db/      # SQLite indexes, including FTS5, entities, and embeddings
├── models/  # reserved for local embedding models
└── logs/    # rolling tracing output
```

Hooks POST observations to the server. The server serializes writes
through one SQLite writer, compiles session observations into markdown
pages, and serves retrieval through FTS5, entity-match and graph-neighbor RRF,
optional vector RRF, bounded source-authority adjustment, and bounded
raw-observation fallback for non-global searches.

See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) for the data-flow
diagram, crate breakdown, schema notes, and invariants.

## Docs

| File | What it is |
|---|---|
| [`docs/install.md`](docs/install.md) | **Installation cookbook.** Every agent CLI, every alternative (curl, source build, no-docker, no-auth), and the server-on-a-different-machine (homelab/LAN) walkthrough. Read after the Quick start if your setup doesn't match the happy path. |
| [`docs/usage.md`](docs/usage.md) | Handoffs, proactive memory queries, slim routing snippet + managed Agent Skills, migration from other memory tools, web UI, raw-wiki inspection, and rules-vs-facts workflow. |
| [`docs/managed-workstreams.md`](docs/managed-workstreams.md) | Optional `ai-memory run` continuity across Claude Code, Codex, OpenCode, Pi, Crush, Kimi Code, Command Code, Kiro CLI v2/v3, OMP, Grok Build CLI, and Antigravity CLI: automatic harness selection, native resume, argument forwarding, ledger search, privacy, and recovery. |
| [`docs/managed-harness-contributions.md`](docs/managed-harness-contributions.md) | Protocol and acceptance bar for contributors adding managed resume, read-only transcript import, and startup context delivery to another harness. |
| [`docs/marker-file.md`](docs/marker-file.md) | `.ai-memory.toml` workspace/project routing for multi-client trees, mono-repos, worktrees, and work/personal separation. |
| [`docs/auto-scope.md`](docs/auto-scope.md) | `[auto_scope]` modes for shared servers: default single-slot routing, session-aware isolation, and multi-user `per_actor` behavior. |
| [`docs/macos.md`](docs/macos.md) | macOS install paths: native release binary (recommended), source build, the Docker wrapper, hook-platform notes, and current macOS limitations. |
| [`docs/windows.md`](docs/windows.md) | Windows install modes: full WSL2, native Windows with Docker Desktop, prebuilt native release zip, native source builds, and current hook/MCP harness caveats. |
| [`docs/mcp-install.md`](docs/mcp-install.md) | Per-client MCP and lifecycle notes, handoff-injection limits, and community bridge guidance. |
| [`docs/deploy.md`](docs/deploy.md) | Homelab deploy: bin/deploy, bearer-token auth, pointers to the TLS guide. |
| [`docs/users.md`](docs/users.md) | **Multi-user attribution (v0.8).** Four-rung auth ladder, `ai-memory user add/list/expire/revive/rotate-token` walkthrough, backward-compat migration for pre-v0.8 installs, token storage rationale. |
| [`docs/https-via-proxy.md`](docs/https-via-proxy.md) | **HTTPS via a reverse proxy.** When you need TLS (multi-user, non-loopback) and when you don't (loopback / stdio). Copy-paste docker compose templates for Caddy + Let's Encrypt, Caddy + internal CA (LAN-only), Cloudflare Tunnel (no open ports), and external cert files; plus native-Caddy + nginx recipes. The "thinking you're secure when you're not" failure modes explicitly called out. |
| [`docs/lifecycle-ops.md`](docs/lifecycle-ops.md) | **Read before running purge / rename / backup / restore / reset / reindex / restore-page.** Safety matrix for state-touching commands, per-project disk layout (how isolation actually works), checkpoint-based page recovery, and operator workflows for "fresh start", "snapshot before risky op", "drop one project", and rebuilding SQLite from wiki files. |
| [`docs/auto-improvement-loop.md`](docs/auto-improvement-loop.md) | Auto-improvement design notes: Hermes-inspired scheduled review, auto-approval default, manual review opt-in, pending proposal storage, and curator work. |
| [`docs/companion-crates.md`](docs/companion-crates.md) | Boundary and implementation plan for optional companion projects, including the standalone importer at [`companions/ai-memory-importer`](companions/ai-memory-importer), without widening core ai-memory. |
| [`docs/llm-provider-comparison.md`](docs/llm-provider-comparison.md) | Empirical notes behind the recommended LLM defaults. |
| [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) | Operational summary: data flow, crate layout, cross-cutting invariants, schema. |
| [`docs/design-decisions.md`](docs/design-decisions.md) | The full v1 spec. |
| Research docs under `docs/` | Karpathy LLM Wiki notes, Hermes Agent, agentmemory / basic-memory / cognee deep-dives, lessons-learned from upstream issues. |

## Influences and prior art

- **[Karpathy LLM Wiki](https://gist.github.com/karpathy/442a6bf555914893e9891c11519de94f)** - the compile-not-retrieve pattern.
- **[agentmemory](https://github.com/rohitg00/agentmemory)** - most of the right ideas; this project is the Rust successor.
- **[basic-memory](https://github.com/basicmachines-co/basic-memory)** - the markdown-on-disk source-of-truth model.
- **[cognee](https://github.com/topoteretes/cognee)** - pipeline composition and triplet embeddings.
- **[Hermes Agent](https://github.com/NousResearch/hermes-agent)** - the self-improvement loop: post-turn review, approval gates, and curator boundaries.
- **[A-MEM](https://arxiv.org/abs/2502.12110)** - Zettelkasten-style atomic notes with link evolution.

## License

MIT - see [LICENSE](LICENSE).

## Acknowledgements

This codebase is being built collaboratively with Claude Code
(Anthropic Claude Opus 4.7) following the plan documented in
`docs/design-decisions.md`.
