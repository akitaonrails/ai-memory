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

## Why ai-memory

Your coding agent already has a memory feature. Claude Code takes its own
notes, Cursor remembers some things, and every platform is adding more. All
of them share the same walls: the notes live on one machine, belong to one
agent, and vanish from view the moment you switch tools — or teammates.

ai-memory is what's on the other side of those walls.

- **It follows you across agents.** Twenty-plus harnesses — Claude Code,
  Codex, Cursor, Gemini CLI, OpenCode, Grok, Devin, Kimi, Kiro, and more —
  feed one shared memory. Quit Claude Code mid-task, open Codex in the same
  directory, and the next agent picks up a real handoff: where you left
  off, what failed, what's still open. Handoffs are a protocol here, not a
  convention — typed, owned, claimed exactly once.

- **It follows you across machines.** Memory lives in a server you run —
  on the same laptop, a homelab box, or wherever — so the project you left
  on the desktop is the project you resume on the laptop. Same knowledge,
  same open questions.

- **It works for a team.** Point everyone at one server and what one
  person's sessions learn, everyone's agents can retrieve. Knowledge is
  shared per project; personal handoffs stay personal. Multi-user auth,
  per-person attribution, and an audit log are built in — not a paid tier.

- **Your memory is plain markdown.** The source of truth is a git-backed
  wiki of ordinary `.md` files: `grep` it, open it in Obsidian, edit it by
  hand, `rsync` it. The database is a derived index that can always be
  rebuilt from the files. No vector store to babysit, nothing held hostage
  in a binary blob.

- **It captures the work itself, silently.** Lifecycle hooks record what
  actually happened — prompts, tool calls, session boundaries — sanitized
  at a typed privacy boundary before anything is stored, then consolidated
  into readable pages. No "remember this" ceremony. And the default path
  uses **zero LLM calls**: capture, search, and handoffs all work with no
  API key at all.

- **It tells you the truth about itself.** One self-contained binary.
  Purge commands that say exactly what "deleted" means. A measured write
  ceiling (~700/s) instead of a guessed one. An audit log of every
  mutation. Boring, in the way infrastructure should be.

## How it works

```
capture ──▶ consolidate ──▶ recall ──▶ handoff
 hooks        session-end      search     next agent,
 observe      summaries as     + brief    any harness
 silently     wiki pages       injection
```

Agents emit sanitized observations through lifecycle hooks as you work.
At session end, observations become coherent markdown pages in the
project's wiki (optionally LLM-written; useful even without). The next
session — any agent, any machine — gets a bounded brief and can search
everything: full-text, entities, links, and (optionally) vectors, fused
into one ranking. Cross-agent handoffs carry the baton explicitly.

The full design, including the invariants that keep multi-user and
multi-session use safe, is in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Support matrix

Every row below is a first-party integration — MCP registration, lifecycle
hooks, or both — kept honest by CI. The full matrix with per-agent notes and
caveats is in [`docs/support-matrix.md`](docs/support-matrix.md).

| Area | Status |
| --- | --- |
| Linux | Supported |
| macOS | Supported |
| Windows via WSL2 | Supported |
| Native Windows | Experimental |
| Claude Code | Supported |
| Codex | Supported |
| Command Code | Supported |
| Devin CLI | Supported |
| OpenCode | Supported |
| Cursor | Supported |
| Gemini CLI | Supported |
| Oh My Pi / OMP | Supported |
| Pi | Supported |
| Crush | Managed-only |
| Managed workstreams | Opt-in |
| Claude Desktop | MCP-only |
| OpenClaw | Supported |
| Antigravity CLI | Supported |
| Grok Build CLI | Supported |
| Swival CLI | MCP-only |
| Zero | Supported |
| ZCode | Supported |
| Kimi Code | Supported |
| Kiro CLI | Supported |
| Pool | Hooks-only |
| VS Code Copilot | MCP-only |
| Zed | MCP-only |
| Hermes Agent | Community |
| LLM/auth providers | Supported |
| Embedding providers | Supported |

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
```

On Linux/macOS, that's it. Start a Claude Code session as usual - every
prompt and tool call now lands in ai-memory, and the next session you
open in this project will see a handoff with where you left off.
On macOS, the native release binary is also supported and recommended when you
do not need Docker; see [`docs/macos.md`](docs/macos.md).

Wiring another agent is the same two commands with a different name —
`--client codex`, `--agent codex`, and so on for every row of the support
matrix. The full per-agent guide, including Windows and remote servers, is
[`docs/install.md`](docs/install.md).

Two agents in the same project at once, or teammates on one server? That
works out of the box: the "current project" pointer is isolated per caller
by default (v1.39+). See [`docs/auto-scope.md`](docs/auto-scope.md) for the
optional session-aware Claude Code bridge and the details.

Managed workstreams are optional and add cross-harness *session* continuity
on top of shared memory:

```bash
ai-memory run claude
ai-memory run codex --yolo   # later: same workstream, different harness
ai-memory continue           # resume the newest managed checkout
```

`ai-memory uninstall --apply` removes everything ai-memory installed,
and only what it installed. Install commands are idempotent and write
timestamped backups next to any file they touch.

## Everyday use

Day to day, you mostly do not think about ai-memory. Hooks capture
prompts, tool calls, and session boundaries; session end turns them into
readable wiki pages; the next session starts with a handoff.

- Ask "where did we leave off?" to continue from the pending handoff.
- Ask "have we discussed X?" or "search memory for Y" to query the wiki.
- Ask "catch me up" for a prose digest of recent project activity.
- Run `ai-memory bootstrap` once when adopting an existing project with
  months of history.
- Start the server with `--enable-web` for a read-only browser view of
  the wiki and a JSON API under `/api/v1`.

The full tour — search modes, entities, feedback, briefings, the web
API — is in [`docs/usage.md`](docs/usage.md) and
[`docs/use-cases.md`](docs/use-cases.md).

## Teams and multiple machines

Run the server somewhere reachable — a homelab box, a LAN host — and
point every machine and every teammate at it. Knowledge is shared per
project; personal handoffs stay personal; every write is attributed and
audited. Multi-user auth (passwords, API credentials) is built in.

Start with [`docs/users.md`](docs/users.md) for accounts and ownership,
and [`docs/deploy.md`](docs/deploy.md) for the server itself — including
capacity numbers measured rather than guessed, and the one rule that
matters: one server per data directory, never two.

## Security

The quick-start default is loopback-only with no auth — nothing outside
your machine can reach it. From there, hardening is incremental: a bearer
token for the LAN, per-user accounts, OIDC device auth for hooks, TLS via
a reverse proxy. Capture is sanitized at a typed privacy boundary before
anything is stored, and per-repository `[capture]` rules can exclude
paths or invert to allowlist mode.

The full model is in [`docs/security.md`](docs/security.md),
[`docs/users.md`](docs/users.md), and
[`docs/https-via-proxy.md`](docs/https-via-proxy.md).

## LLM providers

Optional. Everything works with zero LLM calls; adding a provider
upgrades session summaries and enables semantic search. Anthropic,
OpenAI (incl. OAuth/Codex), GitHub Copilot, Gemini, OpenCode Zen, and
any OpenAI-compatible endpoint (Ollama, LM Studio, vLLM) are supported
for consolidation; OpenAI, Voyage, Gemini, and keyless OpenAI-compatible
endpoints for embeddings. Configuration lives in
[`docs/llm-providers.md`](docs/llm-providers.md).

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
| [`docs/users.md`](docs/users.md) | **Multi-user attribution and human login.** Four-rung bearer ladder, password sessions, `ai-memory user` / `api-key` walkthrough, brownfield `aim_` migration. |
| [`docs/https-via-proxy.md`](docs/https-via-proxy.md) | **HTTPS via a reverse proxy.** When you need TLS (multi-user, non-loopback) and when you don't (loopback / stdio). Copy-paste docker compose templates for Caddy + Let's Encrypt, Caddy + internal CA (LAN-only), Cloudflare Tunnel (no open ports), and external cert files; plus native-Caddy + nginx recipes. The "thinking you're secure when you're not" failure modes explicitly called out. |
| [`docs/lifecycle-ops.md`](docs/lifecycle-ops.md) | **Read before running purge / rename / backup / restore / reset / reindex / restore-page.** Safety matrix for state-touching commands, per-project disk layout (how isolation actually works), checkpoint-based page recovery, and operator workflows for "fresh start", "snapshot before risky op", "drop one project", and rebuilding SQLite from wiki files. |
| [`docs/auto-improvement-loop.md`](docs/auto-improvement-loop.md) | Auto-improvement design notes: Hermes-inspired scheduled review, auto-approval default, manual review opt-in, pending proposal storage, and curator work. |
| [`docs/companion-crates.md`](docs/companion-crates.md) | Boundary and implementation plan for optional companion projects, including the standalone importer at [`companions/ai-memory-importer`](companions/ai-memory-importer), without widening core ai-memory. |
| [`docs/llm-provider-comparison.md`](docs/llm-provider-comparison.md) | Empirical notes behind the recommended LLM defaults. |
| [`docs/llm-provider-fallback.md`](docs/llm-provider-fallback.md) | Proposed opt-in fallback-chain design for transient LLM-provider failures; not yet a supported configuration surface. |
| [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) | Operational summary: data flow, crate layout, cross-cutting invariants, schema. |
| [`docs/design-decisions.md`](docs/design-decisions.md) | The full v1 spec. |
| Research docs under `docs/` | Karpathy LLM Wiki notes, Hermes Agent, agentmemory / basic-memory / cognee deep-dives, lessons-learned from upstream issues. |
- [`docs/support-matrix.md`](docs/support-matrix.md) - the full agent/platform matrix with notes.
- [`docs/use-cases.md`](docs/use-cases.md) - scenario walkthroughs.
- [`docs/llm-providers.md`](docs/llm-providers.md) - provider configuration.
- [`docs/security.md`](docs/security.md) - the full security model.
- [`docs/research-2026-landscape.md`](docs/research-2026-landscape.md) - how the field looks and where we sit in it.
- [`docs/ROADMAP-2.0.md`](docs/ROADMAP-2.0.md) - the plan for the 2.0 release, one item at a time.
- [`docs/okf.md`](docs/okf.md) - the wiki is natively an Open Knowledge Format (OKF v0.2) bundle; design and field mapping.
- [`docs/typed-edges.md`](docs/typed-edges.md) - typed relation edges (`causes` / `fixes` / `contradicts`) and how lint uses them.
- [`docs/temporal.md`](docs/temporal.md) - ingestion-time validity on the entity index and `as_of` time-travel queries.
- [`docs/local-embeddings.md`](docs/local-embeddings.md) - in-process embeddings with no API key (`embedding_provider = "local"`).
- [`docs/experience.md`](docs/experience.md) - the opt-in cross-session abstraction pass: knowledge visible only across trajectories.
- [`docs/MIGRATION-2.0.md`](docs/MIGRATION-2.0.md) - upgrading an existing store to 2.0: the backup-gated automatic migration and how to restore.
- [`docs/benchmarks/`](docs/benchmarks/README.md) - published retrieval-quality numbers with provenance, reproducible from the in-repo harness.

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
