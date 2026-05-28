# Installation cookbook

The [README quick-start](../README.md#quick-start) covers the happy
path (docker + Claude Code). This page covers everything else:

- [Server on a different machine](#server-on-a-different-machine)
  (homelab, LAN box, remote server)
- [Configuring the CLI URL and auth](#configuring-the-cli-url-and-auth)
- [Arch Linux native packages (AUR)](#arch-linux-native-packages-aur)
  (systemd system service or user service)
- [Configuring other agent CLIs](#configuring-other-agent-clis)
  (Codex, OpenCode, OMP, Cursor, Claude Desktop, Gemini CLI, Antigravity CLI, OpenClaw)
- [Installing hooks without docker](#installing-hooks-without-docker)
  (curl-based installer)
- [Running ai-memory without docker](#running-ai-memory-without-docker)
  (cargo install, building from source)
- [LLM provider tiers + self-hosted Ollama](#llm-provider-tiers)
- [Subcommand reference](#subcommand-reference)
- [Operating without auth](#operating-without-auth) (local-only)
- [Keeping ai-memory up to date](#keeping-ai-memory-up-to-date)

> **Shorthand.** Most snippets use `$TOKEN` and `homelab:49374`. If
> you're following along verbatim:
> ```bash
> export TOKEN=$(docker run --rm akitaonrails/ai-memory:latest generate-auth-token)
> ```
> and replace `homelab` with `localhost` if the server runs on the
> same machine as the agent CLI.

The Docker image is published for `linux/amd64` and `linux/arm64`; Apple
Silicon Macs and ARM64 Linux hosts should not need `--platform linux/amd64`.

---

## Server on a different machine

When the ai-memory server runs on a LAN box (homelab, headless server)
and you use Claude Code / Codex / etc. on a laptop:

### Server side (the homelab host)

```bash
docker run -d --name ai-memory \
    --restart unless-stopped \
    -p 0.0.0.0:49374:49374 \
    -v ai-memory-data:/data \
    -e AI_MEMORY_AUTH_TOKEN="$TOKEN" \
    -e AI_MEMORY_ALLOWED_HOSTS="<server-ip>,localhost,127.0.0.1" \
    -e AI_MEMORY_LLM_PROVIDER=anthropic \
    -e ANTHROPIC_API_KEY=sk-ant-... \
    akitaonrails/ai-memory:latest
```

See [Security](../README.md#security) in the README for why
`AI_MEMORY_AUTH_TOKEN` and `AI_MEMORY_ALLOWED_HOSTS` are both
required for any non-loopback bind.

### Client side (the laptop)

```bash
export AI_MEMORY_SERVER_URL="http://<server-ip>:49374"
export AI_MEMORY_AUTH_TOKEN="$TOKEN"

ai-memory install-mcp   --client claude-code --apply
ai-memory install-hooks --agent  claude-code --apply
```

The CLI commands (`bootstrap`, `status`, `search`, `lint`, etc.) inherit the
two env vars automatically. So do `install-mcp`, `install-hooks`, and
`setup-agent`: with `AI_MEMORY_SERVER_URL` set, `install-mcp` derives the
`/mcp` endpoint and `install-hooks` uses the bare server origin.

---

## Configuring the CLI URL and auth

The `ai-memory` binary is a thin HTTP client. It never opens the wiki
or SQLite directly; state-touching commands go through the running
server, which is the sole writer.

Configuration is two optional environment variables:

| Variable | Default | When to set it |
|---|---|---|
| `AI_MEMORY_SERVER_URL` | `http://127.0.0.1:49374` | When the server runs somewhere other than the same machine, such as `http://192.168.0.90:49374`. |
| `AI_MEMORY_AUTH_TOKEN` | unset | When the server has bearer auth enabled. |

For a single-laptop loopback server, set neither variable. For a
remote or homelab server, put both in your shell rc or direnv file:

```bash
export AI_MEMORY_SERVER_URL="http://192.168.0.90:49374"
export AI_MEMORY_AUTH_TOKEN="<token>"
```

Explicit `--server-url` and `--auth-token` flags on `install-mcp`,
`install-hooks`, and `setup-agent` override the environment. That is
useful when you are generating config for a client that talks to a
different server than your default CLI target.

If you run `install-mcp --apply` first and later run `install-hooks --apply`
without env vars or flags, hooks reuse the existing ai-memory MCP entry for
that agent when possible. This keeps remote MCP config and lifecycle capture
pointed at the same server instead of falling back to loopback.

`init`, `serve`, and `generate-auth-token` do not need these env vars because
they either create local files or start the server itself.

---

## Arch Linux native packages (AUR)

Use the native packages when you want `/usr/bin/ai-memory` plus systemd units
instead of the Docker wrapper. The package installs the binary and hook sources
once; each user still stages their agent hook scripts into their own home dir
with `install-hooks --apply`.

### Package choice

```bash
yay -S ai-memory-bin    # prebuilt Linux x86_64/aarch64 binary, fastest install
yay -S ai-memory        # builds from source, works on x86_64 and aarch64
```

Both packages install the same runtime layout:

| Path | Purpose |
|---|---|
| `/usr/bin/ai-memory` | Native CLI/server binary. |
| `/usr/share/ai-memory/hooks/` | Packaged hook source bundle used by `install-hooks`. |
| `/usr/lib/systemd/system/ai-memory.service` | System-wide service unit. |
| `/usr/lib/systemd/user/ai-memory.service` | Per-user service unit. |
| `/usr/lib/sysusers.d/ai-memory.conf` | Creates the `ai-memory` system user. |
| `/usr/lib/tmpfiles.d/ai-memory.conf` | Creates `/var/lib/ai-memory` for the system service. |
| `/etc/ai-memory/config.toml` | System-service config file, tracked as a pacman backup file. |
| `/etc/ai-memory/env` | System-service environment/secrets file, tracked as a pacman backup file. |

The binary itself does not guess between system and user mode. The unit file
chooses explicitly:

| Mode | Data dir | Config | Env/secrets | Requires sudo? |
|---|---|---|---|---|
| User service | `~/.local/share/ai-memory` | `~/.config/ai-memory/config.toml` | `~/.config/ai-memory/env` | No |
| System service | `/var/lib/ai-memory` | `/etc/ai-memory/config.toml` | `/etc/ai-memory/env` | Yes |

Do not run both services on the same bind address. They can coexist on disk, but
only one can listen on `127.0.0.1:49374` unless you change `bind` in one config.

### User-level service

Use this on a single-user workstation. It needs no sudo after package install and
keeps all state in your home directory.

```bash
mkdir -p ~/.config/ai-memory ~/.local/share/ai-memory
ai-memory \
  --data-dir ~/.local/share/ai-memory \
  --config ~/.config/ai-memory/config.toml \
  init
```

Edit provider/auth settings if you want LLM consolidation or bearer auth:

```bash
$EDITOR ~/.config/ai-memory/config.toml
$EDITOR ~/.config/ai-memory/env
```

For a loopback-only local service, bearer auth is optional. If you want one:

```bash
TOKEN=$(ai-memory generate-auth-token)
printf 'AI_MEMORY_AUTH_TOKEN=%s\n' "$TOKEN" >> ~/.config/ai-memory/env
```

Start and inspect the service:

```bash
systemctl --user daemon-reload
systemctl --user enable --now ai-memory.service
systemctl --user status ai-memory.service
journalctl --user -u ai-memory.service -f
```

If the service should keep running after you log out:

```bash
loginctl enable-linger "$USER"
```

Verify the HTTP server:

```bash
curl http://127.0.0.1:49374/mcp
# Expect a JSON-RPC error, which means the server is reachable.
```

### System-level service

Use this for a shared workstation, LAN box, or homelab-style host where the
server should run independently of any logged-in user.

Make sure the package-created user and state directory exist, then initialize
the data layout as that service user:

```bash
sudo systemd-sysusers /usr/lib/sysusers.d/ai-memory.conf
sudo systemd-tmpfiles --create /usr/lib/tmpfiles.d/ai-memory.conf
sudo -u ai-memory ai-memory \
  --data-dir /var/lib/ai-memory \
  --config /etc/ai-memory/config.toml \
  init
```

Edit system config and secrets:

```bash
sudoedit /etc/ai-memory/config.toml
sudoedit /etc/ai-memory/env
```

The package installs `/etc/ai-memory/env` as root-readable only because it may
hold API keys. Keep that file out of backups or logs that other users can read.

For LAN exposure, set a non-loopback bind and allowed hosts in
`/etc/ai-memory/config.toml`, and set a bearer token in `/etc/ai-memory/env`:

```toml
bind = "0.0.0.0:49374"
allowed_hosts = ["homelab", "192.168.0.90", "localhost", "127.0.0.1"]
```

```bash
TOKEN=$(ai-memory generate-auth-token)
printf 'AI_MEMORY_AUTH_TOKEN=%s\n' "$TOKEN" | sudo tee -a /etc/ai-memory/env
```

Start and inspect the service:

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now ai-memory.service
sudo systemctl status ai-memory.service
journalctl -u ai-memory.service -f
```

Verify from the host:

```bash
curl -sI http://127.0.0.1:49374/handoff
# 401 Unauthorized when AI_MEMORY_AUTH_TOKEN is set.
```

### LLM provider login with native services

API-key providers go in the relevant env file:

```bash
# User service
printf 'AI_MEMORY_LLM_PROVIDER=anthropic\nANTHROPIC_API_KEY=sk-ant-...\n' >> ~/.config/ai-memory/env
systemctl --user restart ai-memory.service

# System service
sudoedit /etc/ai-memory/env
sudo systemctl restart ai-memory.service
```

OAuth-style providers write tokens into the selected data dir. Run the login
with the same `--data-dir` and `--config` pair as the service:

```bash
# User service
ai-memory \
  --data-dir ~/.local/share/ai-memory \
  --config ~/.config/ai-memory/config.toml \
  auth login openai-oauth

# System service
sudo -u ai-memory ai-memory \
  --data-dir /var/lib/ai-memory \
  --config /etc/ai-memory/config.toml \
  auth login openai-oauth
```

Use `auth login copilot` the same way for GitHub Copilot. Restart the service
after changing provider settings:

```bash
systemctl --user restart ai-memory.service      # user mode
sudo systemctl restart ai-memory.service        # system mode
```

### Wire agent CLIs after native install

For a local loopback server with no bearer token:

```bash
ai-memory install-mcp   --client claude-code --apply
ai-memory install-hooks --agent  claude-code --apply
```

For a bearer-protected local or LAN server, export the endpoint first. The MCP
URL includes `/mcp`; the hook URL is the bare origin.

```bash
export AI_MEMORY_SERVER_URL="http://127.0.0.1:49374"
export AI_MEMORY_AUTH_TOKEN="$TOKEN"

ai-memory install-mcp   --client claude-code --apply
ai-memory install-hooks --agent  claude-code --apply
```

`install-hooks` finds packaged hook sources under `/usr/share/ai-memory/hooks`,
then stages runnable copies under `~/.local/share/ai-memory/hooks/<agent>/` so
the agent can execute files owned by your user. Re-run `install-hooks --apply`
after package upgrades to refresh those staged copies.

### Native service operations

```bash
# User service
systemctl --user restart ai-memory.service
systemctl --user stop ai-memory.service
journalctl --user -u ai-memory.service -n 100

# System service
sudo systemctl restart ai-memory.service
sudo systemctl stop ai-memory.service
journalctl -u ai-memory.service -n 100
```

Backups still use the same CLI, just point it at the service data dir:

```bash
# User service
ai-memory --data-dir ~/.local/share/ai-memory backup --to ~/ai-memory-backup.tar.gz

# System service
sudo -u ai-memory ai-memory --data-dir /var/lib/ai-memory backup --to /var/lib/ai-memory/backup.tar.gz
```

Package removal does not delete data. Stop the service and remove state only
when you intentionally want to erase memory:

```bash
systemctl --user disable --now ai-memory.service
sudo systemctl disable --now ai-memory.service

# Optional destructive cleanup:
rm -rf ~/.local/share/ai-memory ~/.config/ai-memory
sudo rm -rf /var/lib/ai-memory /etc/ai-memory
```

### Maintainer integration test

The normal CI runs `scripts/check-native-packaging.sh`, a host-safe regression
check that uses a temporary alternate root for `systemd-analyze`,
`systemd-sysusers`, and `systemd-tmpfiles`. It verifies unit syntax, expected
paths, sysusers output, tmpfiles rules, env-file mode, and AUR shell syntax
without writing to host `/usr`, `/etc`, `/var`, or touching real services.

The repo also includes a manual Arch integration harness that is intentionally
kept out of routine CI because it creates a disposable distrobox, installs
packages, starts real systemd services, and can take several minutes:

```bash
scripts/test-native-arch-systemd-distrobox.sh
```

It verifies the AUR metadata shape, builds the current working tree, installs
the native layout into the disposable Arch container, starts the system service
with `systemctl`, starts the user-profile command under transient systemd
supervision, and checks that packaged hook sources under
`/usr/share/ai-memory/hooks` can be staged by `install-hooks`.

The destructive part of that script refuses to run unless it detects a
container/distrobox environment.

Useful knobs:

```bash
AI_MEMORY_NATIVE_TEST_BOX=ai-memory-native-test scripts/test-native-arch-systemd-distrobox.sh
AI_MEMORY_NATIVE_TEST_KEEP_BOX=1 scripts/test-native-arch-systemd-distrobox.sh
AI_MEMORY_NATIVE_TEST_IMAGE=quay.io/toolbx/arch-toolbox:latest scripts/test-native-arch-systemd-distrobox.sh
```

---

## Configuring other agent CLIs

> `install-mcp --server-url` takes the MCP endpoint **including** `/mcp`
> (e.g. `http://homelab:49374/mcp`) — the rendered client config expects the
> full MCP URL. `install-hooks --server-url` takes the bare server **origin**
> (e.g. `http://homelab:49374`) — hook scripts append `/hook`, `/handoff`,
> etc. themselves.

Each agent CLI needs two things:

1. **MCP registration** - so the agent can call `memory_query`,
   `memory_recent`, `memory_handoff_accept`.
2. **Lifecycle hooks** - so the server auto-captures session events.
   Without this, the agent can still query memory but capture
   becomes manual.

Claude Desktop is MCP-only today. Claude Code, Codex, OpenCode, OMP,
Cursor, Gemini CLI, Antigravity CLI, and OpenClaw have lifecycle capture paths through
`install-hooks`.

> **Two-step hook install pattern.** Claude Code, Codex, Cursor,
> Gemini CLI, and Antigravity CLI use shell/PowerShell hook scripts: (1) `docker cp` the
> bundled scripts to your home dir, (2) `docker run --rm install-hooks`
> to render the config snippet.
> OpenClaw, OpenCode, and OMP are different: they use generated
> TypeScript plugin/extension files, so no shell-script extraction is
> needed for those clients.

### OpenAI Codex

```bash
# MCP snippet (merge into ~/.codex/config.toml):
docker run --rm akitaonrails/ai-memory:latest \
    install-mcp --client codex \
    --server-url "http://homelab:49374/mcp" \
    --auth-token "$TOKEN"

# Hooks — extract scripts + render config:
docker cp ai-memory:/usr/local/share/ai-memory/hooks ~/.ai-memory/
docker run --rm akitaonrails/ai-memory:latest \
    install-hooks --agent codex \
        --hooks-dir ~/.ai-memory/hooks \
        --server-url "http://homelab:49374" \
        --auth-token "$TOKEN"
```

### OpenCode

```bash
docker run --rm akitaonrails/ai-memory:latest \
    install-mcp --client opencode \
    --server-url "http://homelab:49374/mcp" \
    --auth-token "$TOKEN"

# Plugin — write to ~/.config/opencode/plugins/ai-memory.ts.
# If you have the local wrapper installed, prefer `--apply`:
ai-memory install-hooks --agent opencode --apply \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"

# Docker-only preview path; redirect only if you want to write the file yourself:
docker run --rm akitaonrails/ai-memory:latest \
    install-hooks --agent opencode \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"
```

Restart OpenCode after installing or changing the plugin; plugins are
loaded at startup.

### Oh My Pi / OMP

```bash
docker run --rm akitaonrails/ai-memory:latest \
    install-mcp --client pi \
    --server-url "http://homelab:49374/mcp" \
    --auth-token "$TOKEN"

# Extension — write to ~/.omp/agent/extensions/ai-memory.ts.
# If you have the local wrapper installed, prefer `--apply`:
ai-memory install-hooks --agent omp --apply \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"
```

Restart OMP after installing or changing the extension; extensions are
loaded at startup. The ai-memory CLI accepts `--client pi` /
`--client omp` for MCP and `--agent omp` / `--agent pi` for hooks;
all four target the same current Oh My Pi integration surface.

### Bind mounts vs docker cp

The `setup-agent` subcommand does the extract + render in one shot
using a bind mount:

```bash
docker run --rm -v "$HOME/.ai-memory:/host" \
    akitaonrails/ai-memory:latest \
    setup-agent --agent claude-code --to /host/hooks \
        --host-prefix "$HOME/.ai-memory/hooks" \
        --server-url "http://homelab:49374" --auth-token "$TOKEN"
```

This works cleanly when the container user's UID matches the host
user's UID (e.g. the homelab where both are 1000). It **fails on
rootless Docker** and on hosts with `userns-remap` enabled - the
container can't write to a host directory that belongs to a UID
outside the user-namespace mapping.

The `docker cp` pattern recommended above sidesteps all of that
because `docker cp` is mediated by the docker daemon and outputs
files owned by the user running the command. Prefer it as the
default; reach for `setup-agent` only when your docker setup is
known not to remap UIDs.

### Cursor, Gemini CLI, Claude Desktop, OpenClaw, Antigravity CLI

See [**`docs/mcp-install.md`**](mcp-install.md) for the per-client MCP
config file path and snippet, or one-shot it via:

```bash
docker run --rm akitaonrails/ai-memory:latest \
    install-mcp --client cursor          --auth-token "$TOKEN" \
    --server-url "http://homelab:49374/mcp"

docker run --rm akitaonrails/ai-memory:latest \
    install-hooks --agent cursor         --auth-token "$TOKEN" \
    --server-url "http://homelab:49374"

docker run --rm akitaonrails/ai-memory:latest \
    install-mcp --client claude-desktop  --auth-token "$TOKEN" \
    --server-url "http://homelab:49374/mcp"

docker run --rm akitaonrails/ai-memory:latest \
    install-mcp --client gemini-cli      --auth-token "$TOKEN" \
    --server-url "http://homelab:49374/mcp"

docker run --rm akitaonrails/ai-memory:latest \
    install-hooks --agent gemini-cli     --auth-token "$TOKEN" \
    --server-url "http://homelab:49374"

docker run --rm akitaonrails/ai-memory:latest \
    install-mcp --client antigravity-cli --auth-token "$TOKEN" \
    --server-url "http://homelab:49374/mcp"

docker run --rm akitaonrails/ai-memory:latest \
    install-hooks --agent antigravity-cli --auth-token "$TOKEN" \
    --server-url "http://homelab:49374"

docker run --rm akitaonrails/ai-memory:latest \
    install-mcp --client openclaw        --auth-token "$TOKEN" \
    --server-url "http://homelab:49374/mcp"

docker run --rm akitaonrails/ai-memory:latest \
    install-hooks --agent openclaw       --auth-token "$TOKEN" \
    --server-url "http://homelab:49374"
```

Cursor, Gemini CLI, Antigravity CLI, and OpenClaw support both `install-mcp` and
`install-hooks`. Claude Desktop is MCP-only here, so you'll need to
nudge the model to call `memory_query` / `memory_handoff_accept` itself.
For clients with `install-hooks` support, the capture path handles
handoff injection at session start or the client's closest equivalent
(Antigravity CLI uses `PreInvocation`).

---

## Installing hooks without docker

If you only need to use ai-memory *from* a machine (i.e. that
machine doesn't run the server), the curl installer pulls shell hook
scripts straight from GitHub for shell-hook agents:

```bash
curl -sSL https://raw.githubusercontent.com/akitaonrails/ai-memory/main/scripts/install-hooks.sh \
    | bash -s -- --agent claude-code

# Then render the JSON config (still wants `ai-memory` somewhere —
# either via docker as a one-shot, or installed locally):
docker run --rm akitaonrails/ai-memory:latest \
    install-hooks --agent claude-code \
        --hooks-dir "$HOME/.ai-memory/hooks" \
        --server-url "http://homelab:49374" \
        --auth-token "$TOKEN"
```

The curl script installer supports
`--agent claude-code|codex|cursor|gemini-cli|antigravity-cli|opencode|openclaw|omp|pi`
and `--to <dir>`; `--help` prints the full flag list. OpenCode,
OpenClaw, and OMP do not need script extraction because `install-hooks`
generates TypeScript plugin/extension files for them instead.

This path is friction-free when:
- You have curl + bash but not docker
- You don't need to run a local ai-memory server (you're a client of
  a homelab/remote ai-memory)

---

## Running ai-memory without docker

Most users should stick to the docker wrapper from the Quick start. Build from
source only when hacking on ai-memory itself or running on a platform docker
doesn't support.

```bash
git clone https://github.com/akitaonrails/ai-memory ~/.ai-memory
cd ~/.ai-memory
cargo build --release --workspace
./target/release/ai-memory init                       # one-time
./target/release/ai-memory serve --transport http \
    --bind 127.0.0.1:49374                            # MCP + hook HTTP server
```

Data dir defaults to `~/.local/share/ai-memory` on Linux,
`~/Library/Application Support/ai-memory` on macOS, and the platform
local-data directory on Windows, typically
`%LOCALAPPDATA%\ai-memory`. Override with `AI_MEMORY_DATA_DIR=/path`.
To require bearer-token auth, set `AI_MEMORY_AUTH_TOKEN` in the
server's environment.

On Windows, see [`docs/windows.md`](windows.md). The short version: run
the install commands from the same environment that launches the agent.
WSL2-launched agents need WSL paths and POSIX `.sh` hooks; native Windows
agents need Windows paths and PowerShell `.ps1` hooks.

When run from source, `install-hooks` finds the bundled scripts in
the repo's `hooks/` automatically:

```bash
./target/release/ai-memory install-hooks --agent claude-code --auth-token "$TOKEN"
```

(No need for `setup-agent` in this case - the scripts already live
at the right host path.)

---

## LLM provider tiers

ai-memory works in three intensity tiers:

| Tier | What you get | Env vars | Cost |
|---|---|---|---|
| **Zero-LLM** (default) | FTS5 search, rule-based session summaries, auto-handoffs from prompt + tool-call history | (none) | $0 |
| **+ LLM consolidation** | LLM rewrites session pages as coherent narratives; PreCompact checkpoints; LLM-driven contradiction lint | `AI_MEMORY_LLM_PROVIDER=anthropic` + `ANTHROPIC_API_KEY` | ~$0.01–0.05 / session |
| **+ Anthropic via subscription** | Same LLM features using a Claude Pro/Max subscription instead of an API key | `AI_MEMORY_LLM_PROVIDER=anthropic-oauth` + `ANTHROPIC_OAUTH_TOKEN` | Uses your Claude subscription |
| **+ ChatGPT/Codex OAuth** | Same LLM features using a ChatGPT Pro/Plus login instead of an OpenAI Platform key | `AI_MEMORY_LLM_PROVIDER=openai-oauth` + `ai-memory auth login openai-oauth` | Uses your ChatGPT subscription |
| **+ GitHub Copilot** | Same LLM features using a GitHub Copilot subscription | `AI_MEMORY_LLM_PROVIDER=copilot` + `ai-memory auth login copilot` or `COPILOT_GITHUB_TOKEN` | Uses your Copilot subscription |
| **+ Hybrid retrieval** | RRF over FTS5 + vector cosine similarity. Better recall on paraphrased queries | `AI_MEMORY_EMBEDDING_PROVIDER=openai` + `OPENAI_API_KEY` | ~$0.0001 / page on backfill |

### Recommended models (chosen as defaults)

If you set only the provider, ai-memory picks a sensible default:

| Setting | Default | Why |
|---|---|---|
| `AI_MEMORY_LLM_PROVIDER=anthropic` | `claude-haiku-4-5` | **Recommended default.** Best balance of speed, restraint, and classification quality. Not a reasoning model. Consistently classifies durable project rules as `kind: rule`. |
| `AI_MEMORY_LLM_PROVIDER=anthropic-oauth` | `claude-sonnet-4-6` | Anthropic via Claude subscription. Run `claude setup-token` once; set `ANTHROPIC_OAUTH_TOKEN` (or `CLAUDE_CODE_OAUTH_TOKEN`). No `ANTHROPIC_API_KEY` needed. Same `/v1/messages` endpoint, Bearer token auth. |
| `AI_MEMORY_LLM_PROVIDER=openai` | `gpt-5.4-mini` | Cheaper + faster alternative. Same parse reliability; mild over-classification on thin sessions. |
| `AI_MEMORY_LLM_PROVIDER=openai-oauth` | `gpt-5.5` | ChatGPT/Codex backend. Run `ai-memory auth login openai-oauth` once; ai-memory stores the refresh token in `<data_dir>/auth.json` and refreshes access tokens automatically. |
| `AI_MEMORY_LLM_PROVIDER=copilot` | `gpt-5.5` | GitHub Copilot Chat backend. ai-memory stores a GitHub user token in `<data_dir>/auth.json`, exchanges it for a short-lived Copilot API token, and refreshes before expiry. |
| `AI_MEMORY_LLM_PROVIDER=gemini` | `gemini-2.5-flash` | Google's hosted option with a generous free tier. ai-memory disables Gemini 2.5 Flash's default dynamic thinking so hidden thought tokens do not truncate strict JSON. Set `GEMINI_API_KEY` (or `GOOGLE_API_KEY`). |
| `AI_MEMORY_EMBEDDING_PROVIDER=openai` | `text-embedding-3-small` (1536-dim) | 5× cheaper than `-3-large` with marginal recall loss. |
| `AI_MEMORY_EMBEDDING_PROVIDER=openai` + `AI_MEMORY_EMBEDDING_BASE_URL=https://openrouter.ai/api/v1` | `openai/text-embedding-3-small` via [OpenRouter](https://openrouter.ai) | Reuses `LLM_API_KEY` or `OPENAI_API_KEY` with the OpenAI-compatible embedding client. |
| `AI_MEMORY_EMBEDDING_PROVIDER=voyage` | `voyage-3` (1024-dim) | Voyage's current general-purpose recommendation. |
| `AI_MEMORY_EMBEDDING_PROVIDER=google` / `gemini` | `gemini-embedding-001` (768-dim) | Google-hosted embeddings via `embedContent`. Set `GEMINI_API_KEY` (or `GOOGLE_API_KEY`). |

> **What we don't recommend:** reasoning-mode models (Claude with extended
> thinking, GPT-o3, Gemini "thinking" variants) — they burn token budget on
> internal reasoning and hang or emit empty responses with the strict-JSON
> consolidation prompt. Turn reasoning off if you must use one.

### Anthropic via Claude subscription (OAuth)

> [!WARNING]
> **Unofficial and against Anthropic's usage policies — use at your own risk.**
> Anthropic provides no public OAuth API for the Claude Pro/Max subscription;
> this reuses the `claude setup-token` credential against `/v1/messages`, which
> is **not a supported or sanctioned integration**. Anthropic's terms reserve
> subscription (Claude Code) access for interactive use, and using it as an
> automated API backend may breach those terms and **could get your account
> rate-limited, flagged, or banned**. The header recipe is also undocumented
> and can change without notice. If you want a supported path, use the
> `anthropic` provider with a real Platform API key. We ship this purely as an
> opt-in convenience and make no guarantees about it.

`anthropic-oauth` is for Claude Pro/Max subscribers who want to use their
existing subscription instead of an Anthropic Platform API key. It hits the
**same** `/v1/messages` endpoint as the `anthropic` provider — only the auth
headers differ (Bearer token + `anthropic-beta: oauth-2025-04-20`).

```bash
# Obtain a token once using the Claude Code CLI:
claude setup-token

# Then export it (the CLI may also write CLAUDE_CODE_OAUTH_TOKEN automatically):
export ANTHROPIC_OAUTH_TOKEN=<paste token here>
export AI_MEMORY_LLM_PROVIDER=anthropic-oauth
ai-memory serve
```

For Docker, pass the token as an env var:

```bash
docker run -d --name ai-memory \
    -p 127.0.0.1:49374:49374 \
    -v ai-memory-data:/data \
    -e AI_MEMORY_LLM_PROVIDER=anthropic-oauth \
    -e ANTHROPIC_OAUTH_TOKEN=<token> \
    akitaonrails/ai-memory:latest
```

Both `ANTHROPIC_OAUTH_TOKEN` and `CLAUDE_CODE_OAUTH_TOKEN` are accepted;
ai-memory checks `ANTHROPIC_OAUTH_TOKEN` first.

> [!TIP]
> **Pick a small, fast model.** ai-memory's LLM work — session
> consolidation, lint, and explore — is summarisation/extraction, not hard
> reasoning, so a Haiku-class model is plenty: faster, cheaper, and far easier
> on subscription rate limits than Sonnet/Opus. Set e.g.
> `AI_MEMORY_LLM_MODEL=claude-haiku-4-5`. Save the high-effort thinking models
> for your actual coding agent.

### OpenAI OAuth / Codex

`openai-oauth` is for ChatGPT Pro/Plus/Codex accounts. It does **not** use
`OPENAI_API_KEY` and it does **not** call `api.openai.com`; requests go to the
ChatGPT/Codex Responses backend with a refreshable OAuth token.

For the Docker quick start wrapper, this writes into the same named volume the
server mounts at `/data`:

```bash
ai-memory auth login openai-oauth
docker run -d --name ai-memory \
    -p 127.0.0.1:49374:49374 \
    -v ai-memory-data:/data \
    -e AI_MEMORY_LLM_PROVIDER=openai-oauth \
    akitaonrails/ai-memory:latest
```

For a remote Docker host, run the login on that host against the same container
or data volume:

```bash
docker exec -it ai-memory ai-memory auth login openai-oauth
```

Use `ai-memory auth status` to check whether a token is present and
`ai-memory auth logout openai-oauth` to remove it.

> [!TIP]
> **Pick a small, fast model.** Consolidation / lint / explore are
> summarisation tasks, not hard reasoning — a mini-class model is plenty and
> is much easier on subscription rate limits. Set e.g.
> `AI_MEMORY_LLM_MODEL=gpt-5-mini` (the `gpt-5.5` default works but is
> overkill for this workload). Reserve the high-effort reasoning models for
> your coding agent.

### GitHub Copilot

`copilot` uses a GitHub user token, then exchanges it for a short-lived Copilot
API token through `https://api.github.com/copilot_internal/v2/token`. The raw
GitHub token is never sent to `api.githubcopilot.com`.

For the Docker quick start wrapper:

```bash
ai-memory auth login copilot
docker run -d --name ai-memory \
    -p 127.0.0.1:49374:49374 \
    -v ai-memory-data:/data \
    -e AI_MEMORY_LLM_PROVIDER=copilot \
    akitaonrails/ai-memory:latest
```

For a remote Docker host, run the login against the same data volume:

```bash
docker exec -it ai-memory ai-memory auth login copilot
```

Non-interactive deploys can set `COPILOT_GITHUB_TOKEN` instead. ai-memory also
accepts `GH_TOKEN` and `GITHUB_TOKEN` when running natively; prefer the explicit
`COPILOT_GITHUB_TOKEN` in Docker so you do not pass a broad token by accident.
Advanced users with a pre-minted Copilot API token can set
`GITHUB_COPILOT_API_TOKEN` and optionally `COPILOT_API_URL`.

`auth login copilot` defaults to GitHub Copilot's public device-flow client id.
Pass `--client-id` or set `AI_MEMORY_COPILOT_CLIENT_ID` if you operate your own
OAuth app.

### Self-hosted LLMs (Ollama / vLLM / LM Studio / OpenRouter)

```bash
docker run -d --name ai-memory \
    -p 49374:49374 \
    -v ai-memory-data:/data \
    -e AI_MEMORY_AUTH_TOKEN="$TOKEN" \
    -e AI_MEMORY_LLM_PROVIDER=openai-compat \
    -e AI_MEMORY_LLM_BASE_URL=http://host.docker.internal:11434/v1 \
    -e AI_MEMORY_LLM_MODEL=qwen2.5-coder:14b \
    akitaonrails/ai-memory:latest
```

There is no safe default model for `openai-compat`; the env var is
required. For OpenRouter (Kimi, DeepSeek, etc.):

```bash
-e AI_MEMORY_LLM_PROVIDER=openai-compat
-e AI_MEMORY_LLM_BASE_URL=https://openrouter.ai/api/v1
-e AI_MEMORY_LLM_MODEL=moonshotai/kimi-k2.6
-e LLM_API_KEY=sk-or-v1-...
```

---

## Subcommand reference

Two ways to invoke a subcommand against the docker deploy:

```bash
# A) Against the running container (stateful: status, search, backup,
#    forget-sweep, lint, embed).
docker exec ai-memory ai-memory status --json
docker exec ai-memory ai-memory search "karpathy"
docker exec ai-memory ai-memory backup --to /data/snapshot.tar.gz

# B) One-shot, no running container needed for pure-stdout helpers
#    (generate-auth-token, install-mcp, install-hooks, setup-agent, llm-test).
#    Auth login is stateful: use docker exec against the running container or
#    the wrapper so it writes into the same data volume as the server.
docker run --rm akitaonrails/ai-memory:latest generate-auth-token
docker run --rm akitaonrails/ai-memory:latest install-mcp --client cursor
docker run --rm akitaonrails/ai-memory:latest --help     # full subcommand tree
```

| Subcommand | Pattern | What it does |
|---|---|---|
| `serve` | `docker compose up -d` (already done) | Run the HTTP MCP server |
| `status` | `docker exec` | Counts, paths, and derived-index diagnostics |
| `search "<query>"` | `docker exec` | Wiki search with FTS5 + graph/vector RRF |
| `write-page` | `docker exec` | Manual page write (atomic + indexed) |
| `backup --to` / `restore --from` | `docker exec` | Snapshot or restore the data dir |
| `forget-sweep` / `lint` / `embed` | `docker exec` | Manual maintenance; sweep + lint also run on the server schedule by default |
| `commit -m "…"` | `docker exec` | Stage + commit the wiki tree |
| `reset --confirm` | `docker exec` | Wipe data (refuses while siblings alive) |
| `generate-auth-token` | `docker run --rm` | Print a random hex bearer token |
| `auth login openai-oauth` | same data volume as the server | Store a ChatGPT/Codex OAuth refresh token for the optional `openai-oauth` LLM provider |
| `auth login copilot` | same data volume as the server | Store a GitHub token for the optional `copilot` LLM provider |
| `install-mcp --client` | `docker run --rm` | MCP-config snippet per client |
| `install-hooks --agent` | `docker run --rm` | Hook-config snippet for an existing hooks dir |
| `setup-agent --agent --to --host-prefix` | `docker run --rm -v` | Extract bundled scripts + print config (one-shot) |
| `uninstall --apply` | same host environment used for install | Remove only ai-memory-owned hooks, MCP entries, and instruction blocks; generated plugin files are deleted only after content validation. Use `--mcp-url` for custom MCP endpoints and `--mcp-name` only to narrow removal. |
| `llm-test --provider …` | `docker run --rm -e …` | Smoke-test an LLM provider |

Data dir inside the container is `/data` (mounted via the compose
volume). Outside docker, override with `AI_MEMORY_DATA_DIR=/path`.

Scheduled maintenance is configured in `[maintenance]` in `config.toml`.
By default, rule-based lint and forget sweep run daily outside hook
latency. Embedding backfill is supported but defaults to off because it
can call a paid provider; enable it with
`embedding_backfill_interval_secs` after configuring an embedder.

---

## Bootstrap mid-project {#bootstrap-mid-project}

When you adopt ai-memory in a project that's already been around for
a while, the wiki starts empty. `ai-memory bootstrap` ingests the
project's existing history into seed pages so the first session has
warm context.

```bash
cd /path/to/project
ai-memory bootstrap
```

If you installed the Docker wrapper from the quick start and started the
server on `127.0.0.1:49374`, the wrapper automatically reaches that host
loopback server from its short-lived helper container. Set
`AI_MEMORY_SERVER_URL=http://<server>:49374` only when the server is
remote or uses a custom host/port.

**What gets ingested by default:**

| Source | Priority (dropped first when over budget) |
|---|---|
| `CLAUDE.md` / `AGENTS.md` (project rules) | never dropped |
| `README.md` at the repo root | very-late |
| `docs/**/*.md` | late |
| Substantive git commits (body >120 chars OR conventional-commit prefix) | mid |
| Module-level `//!` doc-comments in `**/*.rs` | first to drop |

**Flags:**

```
--repo-path <PATH>         (default: git rev-parse --show-toplevel)
--workspace <NAME>         (default: "default")
--project <NAME>           (default: "scratch")
--max-input-tokens N       (default: 150000; total source budget after prune)
--chunk-input-tokens N     (default: 24000; per LLM call; 0 = single call)
--since "30 days ago"      (git log filter; supports "N days/months/years ago" + YYYY-MM-DD)
--exclude-git              (skip commit history)
--exclude-readme           (skip README)
--exclude-docs             (skip docs/**/*.md)
--exclude-code             (skip Rust module headers)
--dry-run                  (collect + estimate but don't call LLM or write)
--force                    (re-bootstrap, overwrites the prior manifest)
```

**Cost.** With Kimi 2.6 via OpenRouter ($0.73/$3.49 per M):
- 50k input tokens cap → ~$0.04 worst case input
- 1-2k generated tokens → ~$0.007 output
- Total: well under $0.20 per run.

**Idempotency.** The first run produces a per-project `bootstrap.md`
manifest (at `<wiki>/<workspace>/<project>/bootstrap.md`) listing every
page generated + a one-paragraph rationale. Re-running without `--force`
errors out. Delete the manifest (and the generated pages) if you want a
clean re-bootstrap.

**Dry-run first.** Always worth doing before the real call to see
which sources would actually be sent + how many tokens that
represents. Output is JSON to stdout.

```bash
ai-memory bootstrap --dry-run
{
  "sources_collected": 117,
  "sources_sent": 22,
  "sources_dropped": 95,
  "estimated_input_tokens": 48760,
  "pages_written": [],
  "rationale": "(dry-run; LLM not invoked)",
  "dry_run": true,
  "llm_chunks": 1
}
```

Large repos (e.g. years of git history) are pruned client-side before
POST, then processed in sequential LLM chunks so provider context limits
are not exceeded. The CLI logs `llm_chunks` in dry-run and the final
outcome.

**Caveat: LLM-fabricated detail.** A bootstrap run can produce
plausible-but-wrong pages (the LLM doesn't know your project, it's
inferring from git history). The wiki is git-versioned precisely so
this is recoverable: review what landed, `docker exec ai-memory git
-C /data/wiki diff HEAD~1`, and revert if it's off.

## Operating without auth

For local-only / single-machine deploys you can skip the bearer
token:

```bash
docker run -d --name ai-memory \
    -p 127.0.0.1:49374:49374 \
    -v ai-memory-data:/data \
    akitaonrails/ai-memory:latest
```

Notice the bind: `127.0.0.1:49374`, not `0.0.0.0:49374`. This is the
critical pairing - **no bearer token AND loopback only** is the only
safe combination. The startup log will warn loudly if you bind to a
LAN address without setting `AI_MEMORY_AUTH_TOKEN`.

Then wire up the agent CLI. Both commands default to no auth and
`http://127.0.0.1:49374` - no extra flags needed for the local case:

```bash
ai-memory install-mcp   --client claude-code --apply
ai-memory install-hooks --agent  claude-code --apply
```

The installed Docker wrapper runs CLI commands inside a short-lived
helper container. For local loopback servers, it automatically bridges
that helper back to the host's `127.0.0.1:49374`, so `ai-memory status`,
`ai-memory search`, and `ai-memory bootstrap` work with the same default
URL as the generated agent config.

### Docker compose alternative

If you prefer compose, clone the repo and run:

```bash
docker compose -f docker/docker-compose.yml up -d
```

The bundled compose file already has `restart: unless-stopped`, a
healthcheck, and the named volume wired up. Agent setup is the same as
the regular Docker path.

---

## Keeping ai-memory up to date

The wrapper checks Docker Hub at most once every 24 hours and prints a
one-line warning when a newer image is available. Upgrade with:

```bash
ai-memory upgrade
```

The command self-upgrades the wrapper script, pulls the latest Docker
image, re-stages hook scripts under
`~/.local/share/ai-memory/hooks/<agent>/` for configured agents, and
prints how to restart the server container so the new binary is used.
Re-running `install-hooks --apply` remains idempotent: ai-memory
replaces only the hook entries it owns and leaves unrelated hooks alone.

Set `AI_MEMORY_NO_VERSION_CHECK=1` to silence the daily check, or
`AI_MEMORY_WRAPPER_URL=<url>` to pin wrapper self-upgrades to a fork or
tagged release.

When the upgraded server starts, it applies SQLite schema migrations and
pending wiki-structure migrations automatically. No manual database
reset or wiki rewrite is required for normal upgrades.

If the server runs on another host, `ai-memory upgrade` refreshes only
the local wrapper, local image, and local hook scripts. Redeploy the
remote server separately with `bin/deploy` or `docker compose pull &&
docker compose up -d` in that deploy directory.

Inside ai-jail or another bwrap sandbox, the wrapper is usable from the
sandbox, but run `install-*` commands outside the sandbox because they
write to `~/.local/share/ai-memory/hooks/`.

---

## See also

- [`docs/deploy.md`](deploy.md) - homelab deploy walkthrough
  (`bin/deploy`, cloudflared TLS, env-file management)
- [`docs/usage.md`](usage.md) - handoffs, proactive querying, web UI,
  routing snippet, and raw-wiki inspection
- [`docs/mcp-install.md`](mcp-install.md) - per-client MCP config
  reference for Cursor, Claude Desktop, Gemini CLI, Antigravity CLI, OpenClaw, OMP
- [`docs/ARCHITECTURE.md`](ARCHITECTURE.md) - what's actually
  running inside ai-memory
