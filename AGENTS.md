<!-- ai-memory:start -->
## Long-term memory (ai-memory)

This project uses [ai-memory](https://github.com/akitaonrails/ai-memory)
for cross-session continuity.

**Default to the current project - always.** Every ai-memory tool
auto-scopes to the project resolved from your session's working
directory. **Do NOT pass `project`, `workspace`, or `cwd` arguments unless
the user explicitly references a *different* project by name** (e.g. "what
did we decide in the `other-app` project?"). Phrases like "this project",
"here", "we", "our work", and "where did we leave off" all mean the
*current* project, so call tools with no scoping args.

This default assumes the MCP client can identify the current agent
session. Static MCP clients in parallel sessions for the same user cannot
forward the real agent session id automatically; pass explicit
`workspace` + `project` / `scopes`, or use a session-aware bridge that
forwards the lifecycle-hook session id on MCP calls.

**Lifecycle hooks already capture sanitized, bounded prompt and tool-lifecycle
observations automatically.** They are not complete native transcripts;
managed `ai-memory run` launches add the portable visible-event ledger. Do not
manually write routine notes. Only write durable memory when the user explicitly asks
to remember or annotate something permanently.

### Use the installed ai-memory Agent Skills

Detailed tool-routing guidance lives in the installed ai-memory Agent
Skills. When a task matches an installed ai-memory Agent Skill, load and
follow that skill before calling ai-memory tools. The skills cover memory
retrieval, handoffs, durable pages, learning maintenance, and routing
install or refresh work.

### When you write a project rule, write it here

If you're about to write a durable project rule ("always X", "never
Y", "all PRs must ..."), write it in the project's canonical agent instruction file.
Many projects use CLAUDE.md for Claude Code and
AGENTS.md for Codex / OpenCode / Cursor / Gemini CLI / Grok Build CLI / Kimi Code,
but if the project says one file is canonical, use that file.

If the rule is a standing *user/team* preference that should apply to
every project (tech choices, code style, personal conventions), save it
to ai-memory's reserved global scope instead — the durable-pages skill
covers how. Default memory reads surface global-scope pages in every
project automatically.

### Refreshing this snippet

This block is maintained by ai-memory. Two ways to refresh it with the
latest binary's recommended copy:

- **From the agent** (no terminal needed): ask "refresh the ai-memory
  routing in this project". The agent calls `memory_install_self_routing`,
  picks the right filename for itself (Claude Code -> `CLAUDE.md`; Codex /
  OpenCode / Cursor / Gemini / Grok -> `AGENTS.md`; Kimi Code -> `AGENTS.md`),
  uses its Write / Edit tool to replace or append the returned
  `markered_block` while preserving
  non-ai-memory user content, then writes or updates each returned
  `managed_skills` item under the selected skill root from `target_hints`
  using its `relative_path`.
- **From the CLI**: `ai-memory install-instructions` (defaults to
  `CLAUDE.md`; pass `--target AGENTS.md` for non-Claude agents or projects
  that use `AGENTS.md` as the canonical instruction file).

Both are idempotent: re-runs replace the block delimited by the ai-memory
start/end HTML-comment markers, without disturbing the rest of the file.
<!-- ai-memory:end -->

# AGENTS.md — ai-memory contributor guide

This file is the single canonical instruction file for AI coding agents
working in this repository (Claude Code, Codex, OpenCode, Cursor, Gemini
CLI, Kimi Code, and other AGENTS-aware harnesses). `CLAUDE.md` is only a
short pointer here — do not duplicate rules into it.

## Project overview

ai-memory is a self-contained Rust binary that gives AI coding agents
long-term, cross-session memory over MCP and lifecycle hooks. Quit Claude
Code mid-task, open Codex in the same directory, and continue without
re-explaining context.

Core design:

- **Markdown-in-git is the source of truth.** The wiki lives at
  `<data_dir>/wiki/`, is editable by hand, and every consolidation pass
  produces a git commit (via `git2`).
- **SQLite is the derived index** (`<data_dir>/db/memory.sqlite`, WAL
  mode): FTS5 search, sessions, observations, handoffs, users, audit log,
  embeddings, and the optional managed-workstream ledger. One writer
  actor owns the writer connection; reads go through a read-only pool.
- **Capture is automatic** through agent lifecycle hooks that POST
  sanitized, bounded observations to the server (`/hook`). The server
  compiles session observations into durable wiki pages (Karpathy-style
  "compile, not retrieve").
- **Retrieval** is FTS5 + link-neighbor RRF, with optional vector RRF
  when an embedding provider is configured, plus bounded raw-observation
  fallback.
- **LLM is opt-in.** Zero-LLM mode still captures, searches (FTS5), and
  writes rule-based summaries. Providers (Anthropic, OpenAI, OpenAI/Codex
  OAuth, GitHub Copilot, Gemini, OpenAI-compatible endpoints) enable
  consolidation, lint, and the auto-improvement loop.
- **Per-project isolation by construction**: every row and page is keyed
  by `(workspace_id, project_id, path)`, resolved from the caller's cwd,
  a `.ai-memory.toml` marker file, or explicit scope arguments.

The full operational map is [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md);
historical rationale is in [`docs/design-decisions.md`](docs/design-decisions.md).
Read [`docs/auto-improvement-loop.md`](docs/auto-improvement-loop.md) before
changing auto-improvement review, pending proposal storage, approval flows,
or prompt routing for learning review.

## Technology stack

- **Rust edition 2024, toolchain 1.95** (pinned in `rust-toolchain.toml`),
  workspace resolver 3. Current workspace version: 1.17.3.
- **Async runtime:** `tokio` (full features).
- **MCP/HTTP:** `rmcp` 1.7 (server SDK) + `axum` 0.8 for MCP HTTP, hooks,
  admin, `/api/v1`, and the built-in `/web` UI; `tower` / `tower-http`.
- **Store:** `rusqlite` (bundled SQLite, backup API), `refinery`
  migrations, FTS5, `parking_lot`.
- **Wiki:** atomic markdown writes (tmp + rename + fsync),
  `notify-debouncer-full` watcher, `git2` with vendored libgit2
  checkpoints.
- **LLM:** typed providers in `ai-memory-llm` behind `LlmProvider` /
  `Embedder` traits; `reqwest` (rustls) for provider HTTP.
- **Config:** `figment` (TOML + `AI_MEMORY_*` env); CLI via `clap` 4
  derive; `clap_complete` for shell completions.
- **Auth/secrets:** `secrecy`, `subtle` (constant-time compare),
  `getrandom`, `base64`, `sha2`.
- **Time/IDs:** `jiff`, `uuid` (v4/v5/v7).
- The build is self-contained (bundled SQLite, vendored libgit2); only a
  standard C toolchain is needed.

Workspace lints (`Cargo.toml`): `unsafe_code = "forbid"`,
`missing_docs = "warn"`, all default clippy lints at warn. Release
profile: thin LTO, `codegen-units = 1`, stripped symbols.

## Repository layout

```
crates/
├── ai-memory-core/        domain types, errors, ids. NO IO.
├── ai-memory-store/       SQLite + writer actor + reader pool + decay math.
├── ai-memory-wiki/        atomic markdown writes, file watcher, git.
├── ai-memory-mcp/         rmcp transport + tool router + admin routes.
├── ai-memory-hooks/       payload schemas, sanitizer, /hook ingress.
├── ai-memory-llm/         provider auth boundary + LlmProvider / Embedder traits.
├── ai-memory-consolidate/ Karpathy ingest / lint / sweep / auto-improve pipeline.
├── ai-memory-web/         read-only /web UI and /api/v1 JSON routes.
├── ai-memory-workstream/  read-only native transcript + launch adapters (`ai-memory run`).
└── ai-memory-cli/         `ai-memory` binary entry point + thin HTTP subcommands.
evals/                     live A/B harness; workspace member, not shipped.
companions/ai-memory-importer/  standalone OMC wiki importer; NOT a root
                           workspace member — build/test it with
                           `--manifest-path companions/ai-memory-importer/Cargo.toml`.
hooks/                     per-agent lifecycle hook bundles (shell/native).
bin/                       host wrapper scripts (`ai-memory`, `deploy`, `release`).
docker/                    Dockerfile, compose files, TLS proxy templates.
packaging/                 AUR/systemd/sysusers/tmpfiles native packaging assets.
scripts/                   packaging checks, hook installer, acceptance scripts.
tests/                     e2e smoke (`e2e/handoff_smoke.sh`), hook shell tests, fixtures.
docs/                      architecture, design decisions, install/deploy/usage guides.
```

Each crate has a single responsibility and exposes a typed API; no
circular dependencies. Inter-crate boundaries enforce the invariants
below.

## Build and test commands

Rust 1.95 is required (the pinned toolchain installs automatically via
rustup). Before claiming any Rust change is ready, run the full local
gate — the same gates CI (`.github/workflows/ci.yml`) and `bin/release`
enforce:

```bash
cargo fmt --all -- --check                                # formatting
git diff --check                                          # whitespace
TAILWIND_SKIP=1 cargo test --workspace                    # tests
TAILWIND_SKIP=1 cargo clippy --workspace --all-targets -- -D warnings
cargo deny check                                          # dependency policy (if installed)
```

- `TAILWIND_SKIP=1` skips the Tailwind asset build in `ai-memory-web`'s
  build script; use it for local test/clippy runs. CI's Linux/macOS test
  job runs the full build without it.
- Run the companion importer separately:
  `cargo test --manifest-path companions/ai-memory-importer/Cargo.toml`
  (plus fmt/clippy on the same manifest). Root `--workspace` commands do
  not cover it.
- Useful focused runs: `cargo test -p ai-memory-store`, etc.
- Shell-level checks: `tests/hooks/test_lib.sh`,
  `tests/e2e/handoff_smoke.sh`, `scripts/check-native-packaging.sh`.
- CI additionally runs `cargo build --release --bin ai-memory` on
  Linux/macOS, a Docker image smoke test, `cargo audit` (with the ignores
  listed in `ci.yml`), gitleaks secret scanning, and a non-gating Windows
  test job.

## Code style guidelines

- Match the surrounding file's conventions: naming, comment density,
  module structure. Comments explain *why*, never restate *what*.
- Small, scoped, behavior-preserving changes. No adjacent feature work,
  no opportunistic refactors, no speculative abstractions or new public
  surface without a shipped caller, persisted data, or an explicit
  requirement.
- No dead code or half-built public surface. Future work is documented in
  `docs/` design notes, not shipped as unreachable stubs.
- Prefer explicit fallbacks over `unwrap`, `expect`, or `unreachable!` in
  runtime paths; panics are acceptable in tests only.
- `unsafe` is forbidden by workspace lint; do not add it.
- Typed boundaries are load-bearing: IDs, `PagePath`, `AgentKind`,
  sanitization, workspace/project resolution, auth capability, and
  provider dialects are parsed/normalized once and reused.
- Keep CLI commands thin: parse args, resolve config once, call typed
  library functions, render output. Provider-specific behavior belongs in
  `ai-memory-llm`, not in CLI/admin handlers.

## Cross-cutting invariants (do not violate)

These are carved into the architecture; each traces to a documented
prior-art bug (see `docs/ARCHITECTURE.md` and `docs/issues-*.md`):

1. **One config-read path.** `Config::load()` runs once at startup; never
   call `std::env::var` outside it.
2. **Single-writer SQLite actor.** All writes go through one `mpsc`
   channel to one dedicated thread (`WriterHandle`). Batch hot-path work
   into one command/transaction; avoid N+1 reads.
3. **Indexes commit in the same transaction as the data.** No
   index-after-return background tasks.
4. **Typed 3-tuple identity** `(workspace_id, project_id, path)` on every
   domain row.
5. **Hooks are fire-and-forget and bounded.** Script hooks hard-timeout
   at ≤200 ms; the server returns 202 immediately or 429 when saturated.
   No unbounded `tokio::spawn` fan-out or queues on hook paths.
6. **Privacy strip is a typed boundary.** `Sanitized<NewObservation>` has
   no constructor other than `sanitize()`; the hook router's sanitizer is
   the only path from untrusted text into the store.
7. **JSON-schema structured outputs only** for LLM calls; no XML or
   wrapper libraries.
8. **`{provider, model, dim}` denormalized next to every embedding**;
   stale vectors are warned about and ignored on config mismatch.
9. **Live-process check before destructive ops** (`reset`, `backup`,
   `restore` consult `sysinfo`).
10. **Atomic file writes** (tmp + rename + fsync); the watcher ignores
    its own writes by filename prefix.
11. **Absolute canonical data dir**, logged loudly at startup.
12. **No global singletons / `lazy_static` configs**; all dependencies
    explicit.
13. **Zero-LLM default path**; the system fully works with no provider.
14. **Provider auth resolves before provider construction**; provider
    clients consume typed `ProviderAuth` material and never read env vars
    directly.
15. **Tracing subscribers explicitly filter their own module** — no
    feedback loops.

Additional boundary rules:

- **Scope resolution:** new MCP/admin/web routes must use
  `ai_memory_store::ScopeResolver` or its explicit helpers
  (`lookup_existing_scope`, `create_explicit_scope`,
  `resolve_many_existing_scopes`) — never hand-rolled workspace/project
  lookup chains. Read/search/embed/retention/destructive paths use
  no-create lookups and fail closed on partial or missing scope; only
  explicit write/create paths may create workspaces or projects.
- **Auth:** preserve boundaries through
  `AuthLevel::authorize(Capability::...)`; do not open-code username
  comparisons or ad-hoc root checks. In multi-user mode every `/admin/*`
  route is root-only; DB-user tokens never bypass admin gates or
  admission webhooks.
- **Wiki mutations** must go through `Wiki::write_page`,
  `Wiki::apply_batch`, or the existing destructive helpers so
  sanitization, admission, attribution, rollback, and index updates stay
  together. Never write wiki files directly from handlers.

## Testing instructions

- Add focused regression tests for every bug fix and behavior change.
  Parsers, ID derivation, and retention/decay math especially.
- Filesystem tests use temp dirs or injected roots (`tempfile`); never
  depend on the real user home directory being writable.
- PRs touching scope resolution need table-driven tests for partial
  scope, missing explicit scope, active-project precedence, and
  cross-workspace isolation.
- PRs touching permissions need tests for root, DB-user, and anonymous
  behavior.
- New disk+SQL mutations need recovery/rollback tests.
- The recall-eval framework lives at
  `crates/ai-memory-consolidate/tests/recall_eval.rs`.
- Tests run with `cargo test --workspace` (use `TAILWIND_SKIP=1` locally).

## Security considerations

- **Default posture:** loopback-only bind (`127.0.0.1:49374`), no auth —
  safe for a single-user machine. Any non-loopback bind should set a
  bearer token (`AI_MEMORY_AUTH_TOKEN`) and `AI_MEMORY_ALLOWED_HOSTS`
  (DNS-rebinding guard). TLS is deliberately delegated to a reverse
  proxy (see `docs/https-via-proxy.md`).
- **Never commit secrets.** gitleaks runs in CI with `.gitleaks.toml`;
  keep real tokens out of docs, fixtures, and tests.
- **Sanitization is the trust boundary:** all untrusted hook payload text
  passes through the `ai-memory-hooks` sanitizer before storage; do not
  create paths that bypass it (or hook backpressure, or the single-writer
  actor).
- **Capture exclusions** (`[capture] ignore_paths` in the nearest
  `.ai-memory.toml` marker) drop recognized file-tool events before they
  reach spool, transport, logs, or storage — preserve this behavior in
  native hook commands and generated integrations.
- **Auth ladder:** static root bearer token → DB-user tokens
  (attribution only, no admin) → OIDC device tokens at the hook edge.
  `/admin/*` becomes root-only the moment the first DB user exists.
- **Dependency policy:** `cargo deny check --all-features` and
  `cargo audit` run in CI; do not add dependencies without checking the
  project doesn't already have the capability, and match existing
  versions/idioms.
- **Destructive operations** (`purge-project`, `reset`, `restore`) must
  keep their confirmation flags and live-process checks.

## Project maintenance rules

- **CHANGELOG is a merge gate.** Any change affecting user-visible
  behavior, installation, supported platforms/agents/providers,
  deployment, env/config, or public tool/admin surfaces must add a
  `CHANGELOG.md` entry under `## [Unreleased]` (correct
  `Added`/`Changed`/`Fixed` heading, past-tense, trailing `(#NNN)`
  reference) and update the relevant README/docs references in the same
  commit. Internal refactors and test-only churn are exempt.
- **No version bumps or release tags without explicit user approval.**
  Do not bump crate/package versions automatically.
- **PR evaluation:** report pros, cons, and recommended fix, then ask for
  approval before merging or pushing PR changes.
- **MCP tool surface changes** require updating `MEMORY_INSTRUCTIONS`,
  `ai_memory_core::SNIPPET_BODY`, README/docs tool references, and the
  regression tests asserting every tool appears in both prompt surfaces.
  The tool count is currently 16 (see `docs/ARCHITECTURE.md`).
- **Semantic versioning:** patch = fixes; minor = additive (new CLI
  subcommands, MCP tools, config keys); major = breaking (on-disk format
  without migration, removed subcommands, breaking MCP schema changes).
- Keep `CLAUDE.md` as a pointer to this file.

## Documentation map

- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — operational map: data
  flow, crate breakdown, schema, invariants, config reference.
- [`docs/design-decisions.md`](docs/design-decisions.md) — full v1 spec
  and milestone plan.
- [`docs/install.md`](docs/install.md) — installation cookbook for every
  supported agent client.
- [`docs/lifecycle-ops.md`](docs/lifecycle-ops.md) — read before touching
  purge/rename/backup/restore/reset/reindex/restore-page.
- [`docs/auto-improvement-loop.md`](docs/auto-improvement-loop.md) —
  learning-loop design, approval gates, curator boundaries.
- [`docs/users.md`](docs/users.md) — multi-user attribution and the
  four-rung auth ladder.
- [`docs/managed-workstreams.md`](docs/managed-workstreams.md) —
  `ai-memory run` cross-harness continuity.
- [`docs/companion-crates.md`](docs/companion-crates.md) — boundary for
  optional companion projects (e.g. the importer).
