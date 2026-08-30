# Installation cookbook

The [README quick-start](../README.md#quick-start) covers the happy
path (docker + Claude Code). This page covers everything else:

- [Server on a different machine](#server-on-a-different-machine)
  (homelab, LAN box, remote server)
- [Configuring the CLI URL and auth](#configuring-the-cli-url-and-auth)
- [Arch Linux native packages (AUR)](#arch-linux-native-packages-aur)
  (systemd system service or user service)
- [Configuring other agent CLIs](#configuring-other-agent-clis)
  (Codex, Command Code, Devin CLI, OpenCode, OMP, Pi, Cursor, Claude Desktop, Gemini CLI, Antigravity CLI, Grok Build CLI, Zero, ZCode, Kimi Code, Kiro CLI, Pool, OpenClaw, VS Code Copilot, Zed)
- [Installing hooks without docker](#installing-hooks-without-docker)
  (curl-based installer)
- [Running ai-memory without docker](#running-ai-memory-without-docker)
  (mise, building from source)
- [Managed cross-harness workstreams](managed-workstreams.md)
  (`ai-memory run`, transparent native resume, and argument forwarding)
- [LLM provider tiers + self-hosted Ollama](#llm-provider-tiers)
- [Common subcommands](#common-subcommands)
- [Managed routing snippets and Agent Skills](#managed-routing-snippets-and-agent-skills)
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

> **Podman.** The `bin/ai-memory` wrapper works with rootless podman, either
> through the `podman-docker` `docker` shim or by pointing it at podman
> directly with `AI_MEMORY_DOCKER=podman`. See
> [SELinux-enforcing hosts](#selinux-enforcing-hosts) for how it detects the
> engine's rootless and SELinux state.

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
`AI_MEMORY_AUTH_TOKEN` and `AI_MEMORY_ALLOWED_HOSTS` are both required for
normal non-loopback binds. Bearer auth does not encrypt traffic: use the ready
[Caddy](../docker/compose.tls.caddy.yml) or
[Cloudflare Tunnel](../docker/compose.tls.cloudflared.yml) templates from the
[HTTPS reverse-proxy guide](https-via-proxy.md) for LAN or remote access.
When the proxy serves `/web` over HTTPS, also set
`AI_MEMORY_AUTH__SECURE_COOKIE=true` in the server environment and close or
redirect direct HTTP access to that hostname. Do not set it for direct HTTP:
browsers then correctly withhold the session cookie.

### Client side (the laptop)

```bash
export AI_MEMORY_SERVER_URL="http://<server-ip>:49374"
export AI_MEMORY_AUTH_TOKEN="$TOKEN"

ai-memory install-mcp   --client claude-code --apply
ai-memory install-hooks --agent  claude-code --apply
```

`--session-aware` is an optional Claude Code MCP mode:

```bash
ai-memory install-mcp --client claude-code --session-aware --apply
```

It replaces the static HTTP MCP entry with a local ai-memory stdio bridge that
still connects to the configured remote server and bearer token, while
forwarding Claude's lifecycle session id. Pair it with
`[auto_scope] mode = "per_session"` when the same operator runs concurrent
Claude Code sessions in different projects. The default static HTTP
registration remains appropriate for one active project at a time.

If `CLAUDE_CONFIG_DIR` is set, the claude-code installers match Claude Code's
own config resolution: `install-mcp` writes the MCP registration to
`$CLAUDE_CONFIG_DIR/.claude.json` (instead of `~/.claude.json`),
`install-hooks` / `setup-agent` target `$CLAUDE_CONFIG_DIR/settings.json`
(instead of `~/.claude/settings.json`), and `install-skills --scope global`
uses `$CLAUDE_CONFIG_DIR/skills` (instead of `~/.claude/skills`). `uninstall`
sweeps the active relocated paths alongside the home defaults. It cannot
discover an older arbitrary `CLAUDE_CONFIG_DIR` that is no longer set. The
Docker wrapper forwards the variable for config roots under its existing
`$HOME` bind mount; use the native binary when the relocated root is outside
`$HOME`.

The CLI commands (`bootstrap`, `status`, `search`, `lint`, `auto-improve`,
`curator`, `pending-writes`, etc.) inherit the two env vars automatically. So do
`install-mcp`, `install-hooks`, and
`setup-agent`: with `AI_MEMORY_SERVER_URL` set, `install-mcp` derives the
`/mcp` endpoint and `install-hooks` uses the bare server origin.

After upgrading ai-memory, refresh the managed routing package in existing
projects so Claude Code/OpenCode/Codex/Gemini pick up new tool guidance and
proactive retrieval rules. From an agent, ask "refresh the ai-memory routing in
this project"; from the terminal, run `ai-memory install-instructions` (or pass
`--target AGENTS.md` for non-Claude prompt files). The update is idempotent:
legacy long snippets between `<!-- ai-memory:start -->` /
`<!-- ai-memory:end -->` are replaced in place with the slim snippet, and
managed Agent Skills are installed or updated alongside it.

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

All installer `--apply` modes preserve symlinked configuration files: the
atomic update is written to the symlink target, including a missing final
target, while the timestamped backup stays next to the user-facing config
path. This keeps stow, chezmoi, and similar dotfile-managed installs linked.

`init`, `serve`, and `generate-auth-token` do not need these env vars because
they either create local files or start the server itself.

### Default project resolution (`--project-strategy`)

By default each session files memory under `basename(cwd)`. Because an agent
shell keeps its working directory between tool calls, a single
`mkdir sub && cd sub` reparents the rest of the session into a phantom project
named `sub`. To make every session for an install resolve its project from the
git repo root instead — collapsing subdirectories and worktrees — bake the
strategy into the hooks:

```bash
ai-memory install-hooks --apply --agent claude-code --project-strategy repo-root
```

`--project-strategy` accepts `basename` (the new-install default; bakes nothing)
or `repo-root`. Omitting it during a later `--apply` preserves the strategy
already baked into that agent's ai-memory hooks, including during the wrapper's
automatic post-upgrade refresh. Pass `basename` explicitly to remove an
existing `repo-root` default. This works for every agent and delivery path. A
per-repo `.ai-memory.toml` marker's own `project_strategy` / `project` still
take precedence — see
[the marker-file reference](marker-file.md#install-wide-default-no-marker).

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

Use `auth login copilot` the same way for GitHub Copilot. For per-developer
native hook auth against an OIDC issuer, run `auth login oidc-device` in the
developer's selected data dir instead:

```bash
ai-memory auth login oidc-device \
  --issuer "https://issuer.example.com/realms/team" \
  --client-id "ai-memory-cli"
```

The stored OIDC access token is also used by thin-client HTTP commands
(`status`, `search`, `read-page`, `write-page`, `backup`, `embed`, and
similar) when no static `AI_MEMORY_AUTH_TOKEN` / `[auth].bearer_token` is
configured. Static bearer auth still has precedence. This is for external
OIDC-aware gateways/bridges; native ai-memory server auth still uses static root
bearer / DB-user tokens, and `/admin/*` remains root-only unless a gateway
translates accepted OIDC auth into upstream auth that ai-memory accepts.

OIDC/Keycloak `sid` claims describe the login provider's session, not the
coding-agent session ai-memory uses for `[auto_scope]` isolation. Gateways may
propagate the authenticated user/client/agent headers, but
`X-Memory-Actor-Session-Id` should only contain a real lifecycle-hook session id
from a session-aware bridge.

Restart the service after changing provider settings:

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

For concurrent Claude Code sessions, set `[auto_scope] mode = "per_session"` in
the server config and add `--session-aware` to the `install-mcp` command. This
works for local and LAN servers; the generated stdio bridge keeps using
`AI_MEMORY_SERVER_URL` / `--server-url` and `AI_MEMORY_AUTH_TOKEN`.

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

### Hook latency expectations

Hook-capable agents launch one short-lived hook process per lifecycle event. A
successful tool call normally fires both a pre-tool and a post-tool event, so
per-invocation overhead is paid twice. As an order-of-magnitude reference, one
independent v1.29.0 evaluation on native macOS aarch64 measured about 145 ms per
`posix-native` invocation (about 290 ms per completed tool call) and about 172
ms per legacy `.sh` invocation.

Those figures are one host's measurements, not a benchmark or performance
guarantee. Process startup, filesystem and security software, the selected
data directory, authentication, and host load can all change the result. The
native hook fast path skips full config loading and tracing, and normally
spools locally instead of waiting for the server, so keep the installer's
native default where supported. Latency-sensitive deployments should measure
their own agent host before opting into additional captured events or choosing
a script fallback.

### Capture-policy capability and refresh

`[capture] ignore_paths` is enforced only by native `ai-memory hook` commands
and generated OpenCode/OMP/Pi/OpenClaw integrations. Local installers select
native commands where supported; legacy `.sh`/`.ps1` hooks and remote-only or
Docker script bundles do not enforce it. Re-run `install-hooks --agent <agent>
--apply` or refresh/reinstall generated plugins after upgrading; installer
capability output reflects the selected integration. See the canonical
[capture exclusions reference](marker-file.md#capture-exclusions).

Lifecycle observation bodies are bounded separately from the 10 MiB HTTP
request limit. User prompts and post-compaction summaries retain up to 16 KiB;
notifications and tool excerpts retain up to 2 KB. Native `ai-memory hook`
commands truncate those fields UTF-8-safely before they enter the local spool
or wire. The server repeats the event-specific caps for every integration,
including script and generated clients, then applies a 16 KiB backstop after
sanitization before any observation reaches SQLite or FTS. Native hook commands
invoke the installed binary directly, so upgrading that binary is enough to
receive the client-side cap.

**Disable Claude Code prompt capture.** Regulated or mixed-trust machines can
leave the rest of Claude Code's lifecycle capture enabled while preventing
`UserPromptSubmit` text from entering the local spool or wire:

```bash
ai-memory install-hooks --agent claude-code --no-capture-prompts --apply
```

The installer removes only ai-memory's prompt hook and preserves third-party
hooks registered under the same event. A later bare `install-hooks --apply`
(including an upgrade refresh) inherits the disabled state. Re-enable prompt
capture explicitly with `--capture-prompts`. These options are Claude Code-only:
some other agents use their prompt hook to inject handoff context, so removing
it would break continuity. Disabling prompt capture reduces session summaries
and recall quality because user intent is no longer part of the observation
stream; tool and session-boundary capture continues unchanged.

**Capture only repositories that opt in.** The controls above narrow *what* is
captured; this one narrows *where*. By default a repository with no
`.ai-memory.toml` marker is still captured, so a machine that works across many
checkouts captures every new one automatically — forgetting a marker means
capturing more, not less. Allowlist mode inverts that:

```bash
ai-memory install-hooks --apply --capture-mode allowlist
```

A repository without a marker then emits **no lifecycle event at all** — not a
trimmed one. The event is dropped in the hook process before it can reach the
local spool or the wire, so nothing is written to disk for a repository that
never opted in. Opting a repository in is just placing a `.ai-memory.toml`
marker in it, which is the same file that already configures routing and
`ignore_paths`.

**It is enforced by native `ai-memory hook` commands only** — the same
boundary that already applies to `[capture] ignore_paths`, and for the same
reason: the gate runs inside the hook binary, immediately before it spools.

That is what a normal `install-hooks --apply` writes on Linux, macOS and
Windows, so the usual install is covered. It is the *script* installs that are
not: the bundled shell/PowerShell hooks POST to the server directly and never
execute the binary, so nothing reads the mode. In practice that means the
legacy `posix`/`windows` platform override (`AI_MEMORY_HOOK_PLATFORM`), the
Docker host wrapper, and `setup-agent` snippets, which emit script commands by
design. `install-hooks --apply` prints the mode and warns when the install it
is writing cannot enforce it.

Within that boundary the mode is not per-agent: it is stored once in the data
directory and every native hook command reads it, whichever agent invoked it.
That also means a later bare
`install-hooks --apply` — including the auto-refresh inside `ai-memory upgrade`
— leaves it alone by construction rather than by re-detecting it. Every
`--apply` prints the mode in force. Return to the default with
`--capture-mode denylist`.

Verify it on any repository without changing anything:

```bash
printf '{"cwd":"%s"}' "$PWD" | ai-memory hook --event user-prompt-submit \
    --agent claude-code --server-url "$AI_MEMORY_SERVER_URL" --check-capture
```

`--check-capture` inspects policy without spooling, draining, or contacting the
server, and needs a JSON payload naming the directory to test. In the output,
`"admits_capture": false` means that repository captures nothing;
`"marker_present"` shows whether a marker was found by the upward walk.

Note the trade: recall is lost for every repository you do not mark, and a
repository you *intended* to capture stays silent until you add its marker.

Some agent harnesses attach the assistant's final turn to their `Stop` event —
Claude Code sends it as a raw `last_assistant_message`. By default that text is
never persisted: the native hook binary strips the raw field before it can reach
the local spool or the wire, and the server strips it defensively on arrival.

**Opt-in capture (#196).** You can opt in to storing a sanitized, 2 KB-capped
excerpt of the assistant's final turn as the Stop body. It is a **double
opt-in** — enable the server first, then the client:

1. **Server:** set `capture_assistant = true` in the live
   `<data_dir>/config.toml` (or the service's configured TOML file), or set
   `AI_MEMORY_CAPTURE_ASSISTANT=true`, then restart `ai-memory serve`.
2. **Client:** re-install the Claude Code hooks with the flag:

   ```bash
   ai-memory install-hooks --agent claude-code --capture-assistant --apply
   ```

The client sanitizes (built-in patterns) and truncates the excerpt before it
touches the spool or wire; the server re-scrubs with its `[sanitize]` patterns
before storing. If either side is off — or the marker is malformed — the Stop
stays empty. Re-running `install-hooks` without `--capture-assistant` removes
the flag (idempotent). `--capture-assistant` is Claude Code + native-platform
only; on any other agent or the script fallback the installer refuses it rather
than enabling something that cannot take effect. Assistant text is
privacy-sensitive — read the `SECURITY.md` notes on what it can contain and where
it flows (consolidation/reviewer prompts, and out to a cloud LLM provider if one
is configured) before enabling it.

Upgrading the binary is sufficient for native Claude Code installs, and pending
spooled events drain with the raw field stripped as well. Installs that run the
`.sh`/`.ps1` script fallback (the Docker script bundle or an explicit
`AI_MEMORY_HOOK_PLATFORM=posix`) cannot sanitize the assistant text, so a `Stop`
payload still carrying the raw field is dropped whole by the script rather than
POSTed verbatim. The Docker wrapper deliberately keeps script commands because a
binary path inside its helper container is not valid on the host; running
`install-hooks` through that wrapper refreshes the scripts but does not convert
them. To capture assistant text safely, install a native ai-memory client on the
agent host, then use that native executable to run
`install-hooks --agent claude-code --apply`. Even if the script fallback is
retained, the server still strips any raw field on receipt before persistence.

Native `ai-memory hook --event ...` commands spool events locally. Session start
does a short bounded cleanup drain before fetching a handoff; cancellation-prone
boundary events (`stop`, `pre-compact`, and `session-end`) start a detached
`hook-drain` helper so delivery does not depend on one shutdown hook surviving.
Each spooled entry keeps one idempotency key across retries. A server that
processed an event but lost the batch response will not duplicate its
observation or completed session-end effects; if processing stopped after the
observation commit, the retry re-runs downstream work. SessionEnd atomically
commits its end watermark with its automatic handoff; a retry that finds that
transaction complete finishes any interrupted wiki commit, durable provider
enqueue, and ingest-key completion without adding a second handoff. Those
incomplete effects remain at-least-once until the server marks the event
complete.
On Unix, the helper uses a trusted `setsid` launcher when available and falls
back to a separate process group otherwise; Windows uses detached/breakaway
process flags. The spool is capped, so a permanently undrained backlog is
eventually pruned rather than unbounded, but old undelivered events can be lost.
The built-in timings stay short on agent-facing paths, but high-latency or
large-backlog instances can raise them with whole-minute runtime env vars in the
agent's environment; no `install-hooks` rerun is needed:

| Env var | Built-in default | Max override | What it caps |
|---|---:|---:|---|
| `AI_MEMORY_HOOK_DRAIN_TIMEOUT_MINUTES` | 3 seconds | 60 minutes | each event POST during a drain |
| `AI_MEMORY_HOOK_HANDOFF_TIMEOUT_MINUTES` | 3 seconds | 60 minutes | the synchronous `session-start` handoff GET |
| `AI_MEMORY_HOOK_START_BUDGET_MINUTES` | 3 seconds | 60 minutes | total time `session-start` may spend waiting for the drain lock and cleanup draining |
| `AI_MEMORY_HOOK_BACKGROUND_DRAIN_BUDGET_MINUTES` | 5 minutes | 60 minutes | total time the detached `hook-drain` helper may spend after a background-drain boundary |
| `AI_MEMORY_HOOK_INCREMENTAL_THRESHOLD` | 32 events | positive integer | spool backlog size that triggers a 250 ms `post-tool-use` catch-up drain |

Timing values must be positive whole minutes. Missing, empty, non-numeric, or
zero values fall back to the built-in defaults; values above 60 are clamped. The
incremental threshold is a positive event count; invalid values fall back to 32.

Server-side hook ingest also has an optional per-source limiter for shared or
remote installs that need protection from one runaway agent session. Set
`AI_MEMORY_HOOK_RATE_PER_SEC` on the server to the token refill rate per
actor/session source; `0` or unset disables the limiter. Set
`AI_MEMORY_HOOK_RATE_BURST` to override the burst size (defaults to the refill
rate, minimum one token when enabled). The limiter is bounded in both key count
and key bytes, and `/hook/batch` drains can skip over-budget sources while still
accepting later unrelated sources.

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

> `install-mcp --server-url` accepts either the bare server origin or the full
> MCP endpoint and appends a missing `/mcp` exactly once.
> `install-hooks --server-url` takes the bare server **origin**
> (e.g. `http://homelab:49374`) — hook scripts append `/hook`, `/handoff`,
> etc. themselves.

Each agent CLI needs two things:

1. **MCP registration** - so the agent can call `memory_query`,
   `memory_recent`, `memory_handoff_accept`.
2. **Lifecycle hooks** - so the server auto-captures session events.
   Without this, the agent can still query memory but capture
   becomes manual.

Claude Desktop, VS Code Copilot, and Zed are MCP-only today. The
hook-capable clients in the [README Support Matrix](../README.md#support-matrix),
including Pi and Zero, have lifecycle capture paths through `install-hooks`.

> **Hook install pattern.** Local supported profiles default to host-native
> commands. Claude Code may use its supported Windows exec form (`command` =
> real `ai-memory.exe`, `args` = argv tokens for `hook --event ...`); other
> agents use native single command strings according to their hook schema.
> PowerShell/Git Bash script bundles are compatibility fallbacks and do not
> enforce capture-policy v1. Remote-only/Docker script installs still use the
> two-step path: (1) `docker cp` bundled scripts to your home dir, (2)
> `docker run --rm install-hooks` renders the config snippet.
> OpenClaw, OpenCode, OMP, and Pi are different: they use generated
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

Codex still does not expose a reliable true session-end hook. Its `Stop` hook is
captured as a turn/stop observation only; ai-memory does **not** treat it as
SessionEnd. When you need the final session summary, handoff, and
auto-improvement eligibility for the current project, run:

```bash
ai-memory finalize-session
# add --all to close every matching open Codex session in this workspace/project
# or target one exact concurrent session (mutually exclusive with --all)
ai-memory finalize-session --session-id <uuid>
```

Antigravity CLI also lacks a true session-end event. Its `Stop` hook marks the
end of one execution loop, so ai-memory intentionally records it without
closing the conversation. Its `PreInvocation` hook likewise runs before every
model call; ai-memory treats only the documented `invocationNum = 0` call as
SessionStart. Later invocations return an empty hook result without capturing
another start or fetching the single-use handoff, so a handoff created while
the current conversation winds down remains available to the next session.
After the final turn, finalize the latest matching Antigravity session
explicitly:

```bash
ai-memory finalize-session --agent antigravity-cli
# add --all only to close every matching open Antigravity session in this scope
# or add --session-id <uuid> to close one exact concurrent session
```

### Devin CLI

Devin uses `~/.devin/config.json` for MCP servers and `~/.devin/hooks.v1.json`
for lifecycle hooks by default. If you prefer one combined Devin config file,
pass `--config-file ~/.devin/config.json` to `install-hooks`; ai-memory then
merges the hook entries under that file's `hooks` key.

```bash
ai-memory install-mcp --client devin --apply \
    --server-url "http://homelab:49374/mcp" \
    --auth-token "$TOKEN"

ai-memory install-hooks --agent devin --apply \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"

ai-memory install-skills --agent devin
```

Devin's hook vocabulary is close to Claude Code's, with two important
differences:

- Devin emits `PostCompaction` after compaction and includes a `summary` field;
  ai-memory records it as `post-compaction`.
- Devin does not expose subagent start/stop hooks, so ai-memory cannot capture
  nested subagent boundaries for Devin.

The `SessionStart` hook injects pending handoffs through Devin's
`hookSpecificOutput.additionalContext`. Real Devin `SessionStart` and
`PostToolUse` payloads may omit `session_id` and `cwd`; ai-memory now infers cwd
from `DEVIN_PROJECT_DIR` or the hook process working directory when the payload
omits it, and mints/reuses a per-host session id from hook state when necessary,
so those events are still captured. A payload-provided value always wins.

### Kimi Code

Kimi Code keeps MCP servers in `~/.kimi-code/mcp.json` and lifecycle hooks in
`~/.kimi-code/config.toml`; both move together when `$KIMI_CODE_HOME` is set.
The CLI also accepts `--agent kimi` as an alias. `install-mcp` writes the
server URL with a `?flavor=moonshot` query because the Moonshot API rejects
root-level `anyOf`/`oneOf`/`allOf` in tool parameter schemas ("moonshot
flavored json schema") — the ai-memory server answers flavored requests with
flat schemas, and all other clients keep the upstream shape.

```bash
ai-memory install-mcp --client kimi-code --apply \
    --server-url "http://homelab:49374/mcp" \
    --auth-token "$TOKEN"

ai-memory install-hooks --agent kimi-code --apply \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"
```

`install-hooks` merges `[[hooks]]` entries into `config.toml`, preserving the
provider/model settings the same file holds. Entries cover 10 events —
`SessionStart`, `SessionEnd`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`,
`PostToolUseFailure` (Kimi Code reports tool failures separately from
successful calls; it reuses the post-tool-use handler), `Stop`,
`SubagentStart`, `SubagentStop`, and `PreCompact` — and default to
native `ai-memory hook --event … --agent kimi-code` commands on local installs
(local spool plus batched delivery, capture-policy v1 enforced); the staged
script bundle under `~/.local/share/ai-memory/hooks/kimi-code/` is the
compatibility fallback (fire-and-forget POSTs to `/hook`). A pending handoff
is injected at `UserPromptSubmit` through the hook's stdout, which Kimi Code
appends to the model context as a user message before the turn; Kimi Code
fires `SessionStart` but discards that hook's stdout, so hooks installed by
an older release consumed handoffs without delivering them. Existing native
hook commands invoke the current `ai-memory` binary and pick up the corrected
delivery behavior on upgrade. Re-run
`ai-memory install-hooks --agent kimi-code --apply` only for a
script-fallback installation so its staged scripts are refreshed.

Kimi Code hook entries accept only `event`, `matcher`, `command`, and
`timeout`; extra fields make the whole `config.toml` fail to load, so prefer
`install-hooks --apply` over hand edits.

### Command Code

Command Code keeps user-scope MCP and hook configuration in separate JSON
files under `~/.commandcode/`. Install both integrations with:

```bash
ai-memory install-mcp --client command-code --apply \
    --server-url "http://homelab:49374/mcp" \
    --auth-token "$TOKEN"

ai-memory install-hooks --agent command-code --apply \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"
```

The aliases `commandcode`, `cmdc`, and `cmd` are accepted. `install-mcp`
merges a native HTTP entry into `~/.commandcode/mcp.json`; `install-hooks`
merges only Command Code's four stable events (`SessionStart`, `PreToolUse`,
`PostToolUse`, and `Stop`) into `~/.commandcode/settings.json`, preserving
other settings and hook handlers. The hook definitions deliberately omit
`matcher`: Command Code documents omission as "all tools", while any matcher
on `SessionStart` or `Stop` prevents that lifecycle hook from firing.

Local installs use the native `ai-memory hook` command, so Command Code's
native `session_id` and `cwd` are attributed directly;
recognized `shell_command`, `read_file`, `write_file`, and `edit_file`
payloads pass through the same bounded capture-exclusion policy as other
native integrations. A pending handoff is injected through
`hookSpecificOutput.additionalContext` at `SessionStart`.

Command Code's stable `Stop` event ends a turn, not a session. Finalize the
open session after the last turn when you need immediate consolidation and a
handoff:

```bash
ai-memory finalize-session --agent command-code
ai-memory finalize-session --agent command-code --session-id <uuid>
```

ai-memory does not install Command Code Mods. Mods run arbitrary unsandboxed
code and are not needed for the stable hook or managed-session paths.

Managed sessions are opt-in:

```bash
ai-memory run command-code
ai-memory run command-code --yolo --model <model-id>
```

The aliases `commandcode`, `cmdc`, and `cmd` select the same adapter. The
default executable is `command-code` on Unix and `cmdc` on native Windows.
Fresh sessions keep Command Code's native UUID; returning sessions use exact
`--session <uuid>`. The read-only adapter accepts only the observed v3 header,
requires its UUID filename and canonical `cwd` to match the checkout, excludes
checkpoint/prompt sidecars, hidden reasoning, images, and provider metadata,
and preserves `parentId` on visible events for branch provenance. An unknown
future transcript version fails closed until its schema is audited. Direct
`cmd`, `cmdc`, or `command-code` launches remain unchanged.

### Kiro CLI

Kiro CLI has one MCP surface and two incompatible lifecycle-hook formats.
ai-memory supports both through explicit installer targets: `kiro-cli` remains
the v2 target, while `kiro-cli-v3` selects the standalone v3 registration. The
global MCP file is `$KIRO_HOME/settings/mcp.json`, defaulting to
`~/.kiro/settings/mcp.json`; pass `--config-file .kiro/settings/mcp.json` for a
project-scoped entry.

```bash
ai-memory install-mcp --client kiro-cli --apply \
    --server-url "https://memory.example/mcp" \
    --auth-token "$TOKEN"
```

The `kiro` alias is equivalent. The installed URL includes
`?flavor=bedrock` so Kiro's Bedrock backend receives schemas without
root-level `anyOf`, `oneOf`, or `allOf`; nested schemas and runtime validation
remain intact. Kiro requires HTTPS for non-loopback remote servers, so the CLI
rejects a plain-HTTP homelab URL before changing the file. Configure a reverse
proxy as described in [HTTPS via reverse proxy](https-via-proxy.md).

Install v2 hooks with the `kiro-cli` agent value. When `--server-url` is omitted,
`install-hooks` can infer the hook origin and bearer token from the managed MCP
entry above.

```bash
# Default v2 engine: merge hooks into every existing global agent config.
ai-memory install-hooks --agent kiro-cli --apply

# A project-local v2 agent overrides a same-named global agent. Update the
# selected local config explicitly instead of assuming the global copy runs.
ai-memory install-hooks --agent kiro-cli --apply \
    --config-file .kiro/agents/<agent-name>.json
```

The v2 engine stores camelCase hooks inside agent JSON files. ai-memory updates
existing `$KIRO_HOME/agents/*.json` files only; it will not fabricate an agent
that Kiro never selects. Create and select an agent first when that directory
is empty. Kiro gives [project-local agents precedence over global agents](https://kiro.dev/docs/cli/custom-agents/configuration-reference/),
so use `--config-file` when the active definition lives under
`.kiro/agents/`. All target files are parsed before any one is changed, and
unrelated agent fields, third-party hooks, and each agent's existing
`--project-strategy` remain intact.

The install registers spawn, user-prompt, pre-tool, post-tool, and stop capture,
remains fail-open when ai-memory is unavailable, and delivers a pending handoff
through successful `agentSpawn` stdout. Verified v2 tool payloads enforce
`[capture] ignore_paths`; an unrecognized payload shape is stored as bounded
metadata rather than exposing file content.

Install v3 hooks with the explicit `kiro-cli-v3` target. This distinction is
intentional: `kiro` and `kiro-cli` continue to mean v2 so an upgrade cannot
silently rewrite an existing installation into an incompatible format. The
standalone registration was acceptance-tested with an interactive Kiro CLI
2.16.2 `--v3` session.

```bash
# Global v3 registration under $KIRO_HOME/hooks (default ~/.kiro/hooks).
ai-memory install-hooks --agent kiro-cli-v3 --apply

# Project-local v3 registration.
ai-memory install-hooks --agent kiro-cli-v3 --apply \
    --config-file .kiro/hooks/ai-memory.json
```

The v3 installer writes the documented standalone `version: "v1"` schema with
PascalCase triggers. It preserves third-party entries in a shared file,
refuses an unsupported schema version or a third-party collision with an
ai-memory-reserved hook name, and bounds capture-only commands to one second.
SessionStart gets five seconds so ai-memory's bounded handoff fetch can finish.
Both engines use the same sanitized hook-ingress boundary: documented and live
`tool_name`/`tool_input` file operations honor `[capture] ignore_paths`, while
unknown file-tool payload shapes degrade to metadata-only capture.

Kiro v2's `stop` event ends a turn, not the session. After the final turn, close
the matching session explicitly; use the exact id when several Kiro sessions
are open in the same project:

```bash
ai-memory finalize-session --agent kiro-cli
ai-memory finalize-session --agent kiro-cli --session-id <uuid>
```

`ai-memory uninstall --only hooks --apply --yes` removes only exact ai-memory
entries from global v2 agents, the current project's `.kiro/agents` directory,
and ai-memory's global/current-project v3 registration. A purely generated v3
file is deleted; third-party entries in a shared file remain. `ai-memory run
kiro` (alias `kiro-cli`) manages the default v2 engine and honors `$KIRO_HOME`;
add `--v3`, `--mode`, or `--agent-engine v3` for version-safe v3 resume. Once
linked, a later plain Kiro launch recovers the stored engine transparently, and
bare `ai-memory run` considers checkout-local sessions from both incompatible
stores. See
[managed workstreams](managed-workstreams.md#native-adapter-behavior).

### Pool (Poolside Agent CLI)

Pool reads lifecycle hooks from a project-scoped `.poolside/settings.yaml` at
the root of each repository it runs in — there is no user-global hook file for
ai-memory to merge. `install-hooks --agent pool` (alias `poolside`) therefore
stages the hook scripts to the stable user-global location and prints a
ready-to-paste `hooks:` snippet; ai-memory deliberately does not write files
inside your repositories.

```bash
# Stage the scripts and print the snippet to paste into
# <repo>/.poolside/settings.yaml:
ai-memory install-hooks --agent pool --apply \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"
```

The snippet wires Pool's five documented events — `SessionStart`,
`UserPromptSubmit`, `PreToolUse`, `PostToolUse`, and `Stop` (Claude-shaped
names, snake_case JSON payload on stdin, verified against Poolside CLI
v1.0.16). Local installs use the native `ai-memory hook` command, so Pool's
documented `tool_name`/`tool_input` file operations honor `[capture]
ignore_paths` and unknown file-tool payload shapes degrade to metadata-only
capture.

Pool has no true session-end event; `Stop` is a turn boundary. After the final
turn, close the session explicitly; use the exact id when several Pool
sessions are open in the same project:

```bash
ai-memory finalize-session --agent pool
ai-memory finalize-session --agent pool --session-id <uuid>
```

Pool tolerates hook stdout, but model-visible context injection from
`SessionStart` stdout is not demonstrated, so the session-start hook captures
only and never fetches the (single-use) handoff — recover a prior session's
handoff via the MCP `memory_handoff_accept` tool. No first-party
`install-mcp` client and no managed workstream (`ai-memory run pool`) are
claimed: Pool's native session-store contract is not demonstrated, per
[managed-harness contributions](managed-harness-contributions.md).

### ZCode (z.ai)

ZCode wires lifecycle hooks in the root `hooks` block of
`~/.zcode/cli/config.json` — the same file that holds its other CLI
registration. `install-hooks --agent zcode` (alias `zai`) merges ai-memory's
entries into that block around any third-party hooks you already have, and is
idempotent: re-running strips only ai-memory's own entries (marked by
`statusMessage: "ai-memory capture"`) and rewrites them in place.

```bash
ai-memory install-hooks --agent zcode --apply \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"

# Preview the exact hooks block without writing anything:
ai-memory install-hooks --agent zcode \
    --server-url "http://homelab:49374"
```

Entries are exec-form `{"type": "process", "command": <ai-memory>, "args":
[…]}` — ZCode spawns them with the event JSON on stdin and no shell, which the
native `ai-memory hook` command reads directly, so the local spool, bearer
auth, and `[capture] ignore_paths` exclusions all apply. Every emitted key is
from ZCode's documented hook schema (`type`, `command`, `args`, `enabled`,
`timeoutMs`, `statusMessage`); ZCode drops entries carrying undocumented keys,
so none are emitted. Six documented triggers are wired: `SessionStart`,
`UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, and
`Stop` (verified live against the embedded engine v0.16.5).
`PostToolUseFailure` fires *instead of* `PostToolUse` when a tool throws and
is forwarded onto the same capture channel with the error preserved as the
observation outcome. `PermissionRequest` is deliberately not installed: its
hook chain races the interactive permission client, and a fast client decision
aborts the losing hook, so passive capture of that event is unreliable by
design.

ZCode injects `SessionStart` stdout as model context
(`hookSpecificOutput.additionalContext`), so unlike Pool, Zero, and Grok the
prior session's handoff is delivered automatically at session start. There is
no true session-end event — `Stop` fires at the end of every turn — so close
finished sessions explicitly; use the exact id when several ZCode sessions are
open in the same project:

```bash
ai-memory finalize-session --agent zcode
ai-memory finalize-session --agent zcode --session-id <uuid>
```

No first-party `install-mcp` client and no managed workstream
(`ai-memory run zcode`) are claimed yet.

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

**On a Gemini/Vertex model, serve Gemini-safe schemas.** OpenCode forwards MCP
tool schemas to the configured provider verbatim, and Google's `Schema`
(Vertex/Gemini `functionDeclaration.parameters`) accepts only a single `type` per
field. `schemars` renders every optional tool argument as a nullable union
(`"type": ["integer", "null"]`), so the provider rewrites it into `any_of` with
`description` still beside it and Vertex fails the whole session at
`tools/list`:

```
Unable to submit request because `ai-memory_memory_auto_improve` functionDeclaration
`parameters.max_proposals` schema specified other fields alongside any_of.
When using any_of, it must be the only field set.
```

Either set `gemini_safe_schemas = true` in `config.toml`
(`AI_MEMORY_GEMINI_SAFE_SCHEMAS=true`) on the server, which collapses those
unions to `"type": "integer"` plus `nullable: true` for every client, or append
`?flavor=gemini` to just this client's MCP URL. Runtime validation is unchanged
either way. Gemini CLI and Antigravity CLI normalize schemas client-side and
need neither.

### Oh My Pi / OMP

```bash
docker run --rm akitaonrails/ai-memory:latest \
    install-mcp --client omp \
    --server-url "http://homelab:49374/mcp" \
    --auth-token "$TOKEN"

# Extension — write to ~/.omp/agent/extensions/ai-memory-omp.ts.
# If you have the local wrapper installed, prefer `--apply`:
ai-memory install-hooks --agent omp --apply \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"
```

Restart OMP after installing or changing the extension; extensions are
loaded at startup. The ai-memory CLI accepts `--client omp` (or
`--client oh-my-pi`) for MCP and `--agent omp` (or `--agent oh-my-pi`)
for hooks; both target OMP's native `.omp` integration surface.

### Pi

Pi does not read a native `mcp.json`. ai-memory supports Pi through one
generated TypeScript extension at `~/.pi/agent/extensions/ai-memory-pi.ts`; the
same file captures lifecycle events and bridges ai-memory's HTTP MCP tools into
Pi with `pi.registerTool`. When `PI_CODING_AGENT_DIR` is set (it relocates
Pi's whole `~/.pi/agent` home), the extension is written to
`$PI_CODING_AGENT_DIR/extensions/ai-memory-pi.ts` instead.

The Pi and OMP extensions use distinct filenames (`ai-memory-pi.ts` and
`ai-memory-omp.ts`) so installing one never overwrites the other. They are
not interchangeable — only Pi's bridges MCP tools.

#### OMP profiles

`omp --profile <name>` relocates OMP's agent home to
`~/.omp/profiles/<name>/agent`. Point the installer at the same profile so
the extension lands where that profile loads it:

```bash
ai-memory install-hooks --agent omp --profile work --apply
# or set it once for the shell:
OMP_PROFILE=work ai-memory install-hooks --agent omp --apply
```

`--profile` takes precedence over `OMP_PROFILE`, and `uninstall --profile
<name>` removes the same file. `PI_CODING_AGENT_DIR` overrides **both** —
when it is set it names the agent directory outright, so no profile
subdirectory is derived from it.

```bash
ai-memory install-hooks --agent pi --apply \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"

# `install-mcp --client pi` prints this guidance instead of writing mcp.json:
ai-memory install-mcp --client pi --server-url "http://homelab:49374/mcp"
```

Restart Pi after installing or changing the extension. OMP / Oh My Pi remains
separate and continues to use `.omp` paths.

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

### Other MCP clients

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
    install-mcp --client grok            --auth-token "$TOKEN" \
    --server-url "http://homelab:49374/mcp"

docker run --rm akitaonrails/ai-memory:latest \
    install-hooks --agent grok            --auth-token "$TOKEN" \
    --server-url "http://homelab:49374"

docker run --rm akitaonrails/ai-memory:latest \
    install-mcp --client openclaw        --auth-token "$TOKEN" \
    --server-url "http://homelab:49374/mcp"

docker run --rm akitaonrails/ai-memory:latest \
    install-hooks --agent openclaw       --auth-token "$TOKEN" \
    --server-url "http://homelab:49374"

docker run --rm akitaonrails/ai-memory:latest \
    install-mcp --client kiro-cli        --auth-token "$TOKEN" \
    --server-url "https://memory.example/mcp"

docker run --rm akitaonrails/ai-memory:latest \
    install-hooks --agent kiro-cli       --auth-token "$TOKEN" \
    --server-url "https://memory.example"

docker run --rm akitaonrails/ai-memory:latest \
    install-mcp --client command-code    --auth-token "$TOKEN" \
    --server-url "http://homelab:49374/mcp"

docker run --rm akitaonrails/ai-memory:latest \
    install-hooks --agent command-code   --auth-token "$TOKEN" \
    --server-url "http://homelab:49374"

docker run --rm akitaonrails/ai-memory:latest \
    install-mcp --client vscode-copilot  --auth-token "$TOKEN" \
    --server-url "http://homelab:49374/mcp"

docker run --rm akitaonrails/ai-memory:latest \
    install-mcp --client zed             --auth-token "$TOKEN" \
    --server-url "http://homelab:49374/mcp"

docker run --rm akitaonrails/ai-memory:latest \
    install-mcp --client zcode           --auth-token "$TOKEN" \
    --server-url "http://homelab:49374/mcp"
```

Cursor, Gemini CLI, Antigravity CLI, Grok Build CLI, Kiro CLI, Command Code, and OpenClaw support both
`install-mcp` and `install-hooks`. Grok's `install-mcp --client grok` writes
`$GROK_HOME/config.toml` (default `~/.grok/config.toml`); its hooks live under
`$GROK_HOME/hooks` (default `~/.grok/hooks`). `install-hooks --agent grok`
captures lifecycle events.
Grok ignores `SessionStart` stdout, so handoffs must be accepted through MCP with
`memory_handoff_accept` when resuming. Claude Desktop, VS Code Copilot, Zed,
and ZCode
are MCP-only here, so you'll need to nudge the model to call
`memory_query` / `memory_handoff_accept` itself.
For clients with `install-hooks` support, the capture path handles
handoff injection at session start or the client's closest equivalent, except
for Grok's (and Zero's) no-stdout SessionStart behavior (Antigravity CLI uses `PreInvocation`).

---

## Installing hooks without docker

If you only need to use ai-memory *from* a machine (i.e. that machine doesn't
run the server), download and verify the release installer. The installer then
downloads and verifies the release's hook archive before writing any scripts:

```bash
installer_base=https://github.com/akitaonrails/ai-memory/releases/latest/download/ai-memory-install-hooks
installer_tmp="$(mktemp -d)"
trap 'rm -rf "$installer_tmp"' EXIT
curl -fsSL "$installer_base" -o "$installer_tmp/ai-memory-install-hooks"
curl -fsSL "$installer_base.sha256" -o "$installer_tmp/ai-memory-install-hooks.sha256"
expected="$(awk 'NR == 1 { print $1 }' "$installer_tmp/ai-memory-install-hooks.sha256")"
if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$installer_tmp/ai-memory-install-hooks" | awk '{ print $1 }')"
else
    actual="$(shasum -a 256 "$installer_tmp/ai-memory-install-hooks" | awk '{ print $1 }')"
fi
[ -n "$expected" ] && [ "$actual" = "$expected" ] || { echo "installer checksum mismatch" >&2; exit 1; }
chmod +x "$installer_tmp/ai-memory-install-hooks"
"$installer_tmp/ai-memory-install-hooks" --agent claude-code
rm -rf "$installer_tmp"
trap - EXIT

# Then render the JSON config (still wants `ai-memory` somewhere —
# either via docker as a one-shot, or installed locally):
docker run --rm akitaonrails/ai-memory:latest \
    install-hooks --agent claude-code \
        --hooks-dir "$HOME/.ai-memory/hooks" \
        --server-url "http://homelab:49374" \
        --auth-token "$TOKEN"
```

The curl script installer supports
`--agent claude-code|codex|cursor|gemini-cli|antigravity-cli|grok|opencode|openclaw|omp|oh-my-pi|pi`
and `--to <dir>`; `--help` prints the full flag list. OpenCode,
OpenClaw, OMP / Oh My Pi, and Pi do not need script extraction because
`install-hooks` generates TypeScript plugin/extension files for them
instead. For Pi, the generated extension also provides the MCP bridge.

This path is friction-free when:
- You have curl + bash but not docker
- You don't need to run a local ai-memory server (you're a client of
  a homelab/remote ai-memory)

### Hook command paths across a container boundary

`install-hooks --apply` stages the hook scripts into the data dir and
writes their absolute paths into the agent's config. When the CLI runs
inside a container but the agent runs on the host, those staged paths
would be container paths the host can't see. Set
`AI_MEMORY_HOOKS_HOST_ROOT` to the *host* directory that the staged
`hooks/` tree is mounted from and the rendered config uses
`<host-root>/<agent>/…` command paths instead. The bundled docker
wrappers (`bin/ai-memory`, `bin/ai-memory.ps1`) forward this variable
automatically; you only set it by hand for custom container setups.

---

## Running ai-memory without docker

Most users should stick to the docker wrapper from the Quick start. Arch
Linux users have the [AUR packages](#arch-linux-native-packages-aur). For any
other host, `mise` installs a tagged release binary directly from GitHub —
no Rust toolchain needed:

```bash
mise use -g github:akitaonrails/ai-memory
```

This uses [mise's GitHub backend](https://mise.jdx.dev/dev-tools/backends/github.html),
which downloads the release archive matching your OS/arch
(`ai-memory-linux-x86_64.tar.gz`, `ai-memory-macos-aarch64.tar.gz`, etc.),
verifies its checksum against the published `.sha256` sidecar, then extracts
it and puts `ai-memory` on `PATH`. (mise can also check GitHub artifact
attestation and SLSA provenance where a project publishes them; ai-memory's
release workflow does not emit either today, so only the checksum applies.)
No dedicated mise plugin or
registry entry is required — the backend works against any repo whose
release assets follow this naming convention. By default mise also holds back
the very newest release for a short safety window
([`minimum_release_age`](https://mise.jdx.dev/dev-tools/github-backend.html)),
so a fresh tag may resolve to the previous version for a day or so; pin an
exact tag with `mise use -g github:akitaonrails/ai-memory@1.30.0` to bypass
that.

`cargo install ai-memory` is not available: the crate name is already taken
by an unrelated project on crates.io, as is `ai-memory-core` (the workspace's
foundational internal crate). Publishing would require renaming at least
that crate for the registry — a naming decision the project hasn't made yet.

Build from source only when hacking on ai-memory itself or running on a
platform none of the above covers. On macOS, tagged releases also publish
native `ai-memory-macos-aarch64.tar.gz` and `ai-memory-macos-x86_64.tar.gz`
archives when you only need the client CLI.

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

#### Optional serve flags

The `serve` subcommand also accepts:

| Flag | Env var | What it does |
|---|---|---|
| `--enable-web` | `AI_MEMORY_ENABLE_WEB=true` | Mount the read-only web browser + `/api/v1` JSON API. |
| `--base-path /wiki` | `AI_MEMORY_BASE_PATH` | Host the entire HTTP surface (`/mcp`, `/hook`, `/admin/*`, `/api/v1`, `/web`) under a configurable subpath — useful behind a reverse proxy sharing a hostname. `.` and `..` segments are rejected; unsafe chars cause a fallback to root with a warning. See [`docs/https-via-proxy.md`](https-via-proxy.md#hosting-under-a-subpath). |
| `--web-slug /web` | `AI_MEMORY_WEB_SLUG` | Where the web UI mounts within the base-path. Default `/web`; set to `/` to mount the UI at the base-path root. |
| `--web-ui-dir <path>` | `AI_MEMORY_WEB_UI_DIR` | Serve a custom SPA from `<path>` instead of the built-in browser. ai-memory injects `<base href>` and `<meta name="ai-memory-base-path">` so the SPA can build relative URLs and API calls under the configured prefix. |
| `--cors-allow-origin <origin>` | `AI_MEMORY_CORS_ALLOW_ORIGINS` (CSV) | Allow listed origins to call `/api/v1`. Layer is scoped only to that route — `/mcp`, `/hook`, `/admin`, and `/web` remain origin-locked. |
| _(config only)_ | `AI_MEMORY_HOOK_RATE_PER_SEC`, `AI_MEMORY_HOOK_RATE_BURST` | Optional per-actor/session hook ingest token bucket. Unset/`0` rate disables it; burst defaults to the rate (minimum one token when enabled). |

On macOS, see [`docs/macos.md`](macos.md); use the archive matching your
architecture: `aarch64` for Apple Silicon, `x86_64` for Intel. On Windows, see
[`docs/windows.md`](windows.md).
The short version: run the install commands from the same environment that
launches the agent. WSL2-launched agents need WSL paths and POSIX `.sh` hooks.
Native Windows agents can use the tagged `ai-memory-windows-x86_64.zip`, the
Docker Desktop wrapper, or a source build. Native Claude Code uses Claude exec
form with a real `ai-memory.exe` by default; the Windows Docker wrapper renders
other native Windows script-hook agents through encoded PowerShell `.ps1`
fallback commands.

When run from source, `install-hooks` finds the bundled scripts in
the repo's `hooks/` automatically. Extracted release archives also
auto-discover the sibling `hooks/` bundle beside the `ai-memory` binary:

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
| **Zero-LLM** (default) | FTS5 + manually declared entity + graph search, rule-based session summaries, auto-handoffs from prompt + tool-call history | (none) | $0 |
| **+ LLM consolidation** | LLM rewrites session pages as coherent narratives; PreCompact checkpoints; LLM-driven contradiction lint | `AI_MEMORY_LLM_PROVIDER=anthropic` + `ANTHROPIC_API_KEY` | ~$0.01–0.05 / session |
| **+ Anthropic via subscription** | Same LLM features using a Claude Pro/Max subscription instead of an API key | `AI_MEMORY_LLM_PROVIDER=anthropic-oauth` + `ANTHROPIC_OAUTH_TOKEN` | Uses your Claude subscription |
| **+ ChatGPT/Codex OAuth** | Same LLM features using a ChatGPT Pro/Plus login instead of an OpenAI Platform key | `AI_MEMORY_LLM_PROVIDER=openai-oauth` + `ai-memory auth login openai-oauth` | Uses your ChatGPT subscription |
| **+ GitHub Copilot** | Same LLM features using a GitHub Copilot subscription | `AI_MEMORY_LLM_PROVIDER=copilot` + `ai-memory auth login copilot` or `COPILOT_GITHUB_TOKEN` | Uses your Copilot subscription |
| **+ LLM reranking** | At most one relevance pass over up to 30 bounded project/scopes search candidates; normal order is preserved on invalid, failed, timed-out, or concurrency-saturated responses | `AI_MEMORY_RERANKER=llm` + any configured LLM provider | One LLM call per eligible query, at most four concurrently |
| **+ Hybrid retrieval** | Adds vector cosine similarity to FTS5 + entity + graph RRF. Better recall on paraphrased queries | `AI_MEMORY_EMBEDDING_PROVIDER=openai` + `OPENAI_API_KEY` (or `EMBEDDING_API_KEY`) | ~$0.0001 / page on backfill |

### Recommended models (chosen as defaults)

If you set only the provider, ai-memory picks a sensible default:

| Setting | Default | Why |
|---|---|---|
| `AI_MEMORY_LLM_PROVIDER=anthropic` | `claude-haiku-4-5` | **Recommended default.** Best balance of speed, restraint, and classification quality. Not a reasoning model. Consistently classifies durable project rules as `kind: rule`. |
| `AI_MEMORY_LLM_PROVIDER=anthropic-oauth` | `claude-sonnet-4-6` | Anthropic via Claude subscription. Run `claude setup-token` once; set `ANTHROPIC_OAUTH_TOKEN` (or `CLAUDE_CODE_OAUTH_TOKEN`). No `ANTHROPIC_API_KEY` needed. Same `/v1/messages` endpoint, Bearer token auth. |
| `AI_MEMORY_LLM_PROVIDER=openai` | `gpt-5.4-mini` | Cheaper + faster alternative. Same parse reliability; mild over-classification on thin sessions. |
| `AI_MEMORY_LLM_PROVIDER=openai-oauth` | `gpt-5.5` | ChatGPT/Codex backend. Run `ai-memory auth login openai-oauth` once; ai-memory stores the refresh token in `<data_dir>/auth.json` and refreshes access tokens automatically. |
| `AI_MEMORY_LLM_PROVIDER=copilot` | `gpt-5.5` | GitHub Copilot Chat backend. ai-memory stores a GitHub user token in `<data_dir>/auth.json`, exchanges it for a short-lived Copilot API token, and refreshes before expiry. |
| `AI_MEMORY_LLM_PROVIDER=gemini` | `gemini-3.5-flash` | Google's hosted option with a generous free tier. ai-memory disables Gemini 3.5 Flash's default dynamic thinking so hidden thought tokens do not truncate strict JSON. Set `GEMINI_API_KEY` (or `GOOGLE_API_KEY`). |
| `AI_MEMORY_LLM_PROVIDER=opencode` | `claude-sonnet-4-6` | [OpenCode Zen/Go](https://opencode.ai) cloud API — OpenAI-compatible endpoint at `opencode.ai/zen/go/v1`. Set `OPENCODE_API_KEY` (key from `opencode.ai/auth`). Alias: `opencode-zen`. |
| `AI_MEMORY_EMBEDDING_PROVIDER=openai` | `text-embedding-3-small` (1536-dim) | 5× cheaper than `-3-large` with marginal recall loss. |
| `AI_MEMORY_EMBEDDING_PROVIDER=openai` + `AI_MEMORY_EMBEDDING_BASE_URL=https://openrouter.ai/api/v1` | `openai/text-embedding-3-small` via [OpenRouter](https://openrouter.ai) | Uses `EMBEDDING_API_KEY`, else reuses `LLM_API_KEY` or `OPENAI_API_KEY`, with the OpenAI-compatible embedding client. |
| `AI_MEMORY_EMBEDDING_PROVIDER=openai` + `AI_MEMORY_EMBEDDING_BASE_URL=https://api.orcarouter.ai/v1` | `openai/text-embedding-3-small` via [OrcaRouter](https://www.orcarouter.ai) | Uses `EMBEDDING_API_KEY`, else reuses `LLM_API_KEY`, with the OpenAI-compatible embedding client. |
| `AI_MEMORY_EMBEDDING_PROVIDER=voyage` | `voyage-3` (1024-dim) | Voyage's current general-purpose recommendation. |
| `AI_MEMORY_EMBEDDING_PROVIDER=google` / `gemini` | `gemini-embedding-001` (768-dim) | Google-hosted embeddings via `embedContent`. Set `GEMINI_API_KEY` (or `GOOGLE_API_KEY`). |
| `AI_MEMORY_EMBEDDING_PROVIDER=openai-compat` | no default — set model, dim, and base URL explicitly | Self-hosted engines (Ollama, LM Studio, vLLM). Keyless by default; `EMBEDDING_API_KEY`, else `LLM_API_KEY`, is sent as a bearer token when present (gateways). Example: `AI_MEMORY_EMBEDDING_BASE_URL=http://localhost:11434/v1`, `AI_MEMORY_EMBEDDING_MODEL=nomic-embed-text`, `AI_MEMORY_EMBEDDING_DIM=768`. Switching an existing `openai`+base-URL setup to `openai-compat` changes the stored `{provider, model, dim}` triple — run `ai-memory embed --force` to re-embed. |

> **What we don't recommend:** reasoning-mode models (Claude with extended
> thinking, GPT-o3, Gemini "thinking" variants) — they burn token budget on
> internal reasoning and hang or emit empty responses with the strict-JSON
> consolidation prompt. Turn reasoning off if you must use one.

#### Embedding credentials (`EMBEDDING_API_KEY`)

`EMBEDDING_API_KEY` is optional and credentials the embedding role alone. Set
it when the embedder should authenticate against a different provider than the
LLM — the `openai` and `openai-compat` embedders otherwise borrow the chat
model's `OPENAI_API_KEY` or `LLM_API_KEY`, which is then sent to whatever
`AI_MEMORY_EMBEDDING_BASE_URL` points at and rejected with a 401.

Precedence for `AI_MEMORY_EMBEDDING_PROVIDER=openai`:

1. `EMBEDDING_API_KEY`
2. `OPENAI_API_KEY`
3. `LLM_API_KEY`, only when `AI_MEMORY_EMBEDDING_BASE_URL` is set

`openai-compat` checks `EMBEDDING_API_KEY`, then `LLM_API_KEY`, and stays
keyless when neither is set. `voyage` and `google`/`gemini` are unaffected:
they read their own `VOYAGE_API_KEY` and `GEMINI_API_KEY`/`GOOGLE_API_KEY` and
never borrow another role's key. With `EMBEDDING_API_KEY` unset, key
resolution is exactly what it was before the variable existed.

```bash
# Chat on api.openai.com — `openai` is the provider that sends
# `max_completion_tokens`, which gpt-5 / o-series models require and
# `openai-compat` never emits — with embeddings on another endpoint.
export AI_MEMORY_LLM_PROVIDER=openai
export OPENAI_API_KEY=sk-...
export AI_MEMORY_EMBEDDING_PROVIDER=openai
export AI_MEMORY_EMBEDDING_BASE_URL=https://openrouter.ai/api/v1
export EMBEDDING_API_KEY=sk-or-v1-...
```

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
ai-memory checks `ANTHROPIC_OAUTH_TOKEN` first. When either variable is exported
on the host, the POSIX and PowerShell Docker wrappers forward its name to
short-lived helper commands such as `llm-test`; the token value is inherited by
Docker rather than placed in the wrapper's command line. The long-lived server
container still needs the provider and token variables in its own environment,
as in the example above.

For both Anthropic providers, ai-memory omits `temperature` for Claude
4.7 and later models and Claude Mythos Preview because those models reject
non-default sampling parameters. `llm-test` deliberately starts with the same
representative 0.2 value as bootstrap and consolidation, then exercises the
provider's compatibility normalization before sending the request.

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

### OpenAI-compatible providers (Ollama / vLLM / LM Studio / hosted APIs)

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

[Atlas Cloud](https://www.atlascloud.ai/models/qwen/qwen3.5-flash) uses the
same provider; no Atlas-specific ai-memory provider is needed. Pass its API key
through the generic compatibility credential:

```bash
-e AI_MEMORY_LLM_PROVIDER=openai-compat
-e AI_MEMORY_LLM_BASE_URL=https://api.atlascloud.ai/v1
-e AI_MEMORY_LLM_MODEL=qwen/qwen3.5-flash
-e LLM_API_KEY="$ATLASCLOUD_API_KEY"
```

Replace the model with another current Atlas model id when needed. ai-memory
does not select a default for hosted compatibility endpoints.

[OrcaRouter](https://www.orcarouter.ai) uses the same provider; no
OrcaRouter-specific ai-memory provider is needed. Pass its API key through the
generic compatibility credential:

```bash
-e AI_MEMORY_LLM_PROVIDER=openai-compat
-e AI_MEMORY_LLM_BASE_URL=https://api.orcarouter.ai/v1
-e AI_MEMORY_LLM_MODEL=openai/gpt-4o
-e LLM_API_KEY=sk-orca-...
```

Replace the model with another current OrcaRouter model id (same
`provider/model` format as OpenRouter, e.g. `anthropic/claude-sonnet-4.6` or
`deepseek/deepseek-v4-flash`) when needed.

OpenAI-compatible structured calls use the operation's JSON Schema by default:

```bash
-e AI_MEMORY_LLM_COMPAT_STRICT=true
```

Modern Ollama, vLLM, LM Studio, llama.cpp, and gateway endpoints honour this
OpenAI-style `response_format=json_schema` request. ai-memory retries with its
tolerant parser when an endpoint explicitly rejects the structured-output field
or returns a malformed response shape. For an incompatible endpoint, opt out:

```bash
-e AI_MEMORY_LLM_COMPAT_STRICT=false
```

Hosted gateways that stream long completions past the default 300-second
per-request ceiling fail with `http: error sending request`; raise the
ceiling to match the gateway's worst-case generation time:

```bash
-e AI_MEMORY_LLM_TIMEOUT_SECS=900
```

#### Match the consolidation budget to a local model's context window

Consolidation defaults to an approximate 100k-token input target plus a 32k
output limit, sized for a 200k-context provider. A local model with a smaller
window can reject the whole request (`exceed_context_size_error` from
llama.cpp, HTTP 400 from most gateways). Lower both limits so their sum fits
the real context window, with additional headroom for tokenizer variance:

```bash
# e.g. a model loaded with an 8k context window
-e AI_MEMORY_CONSOLIDATION__MAX_INPUT_TOKENS=6500
-e AI_MEMORY_CONSOLIDATION__MAX_OUTPUT_TOKENS=1000
```

The double underscore separates the `[consolidation]` section from each key.
The input target accounts for the rendered observations, current page body,
system prompt, page conventions, bounded slot snapshots, structured-output
schema, and provider-envelope reserve. Tokenizers differ, so this is a
conservative estimate rather than an exact provider token count. An automatic
checkpoint provider failure degrades to a rule-based page rather than losing
the checkpoint, but right-sized limits are what allow LLM consolidation to
succeed. Startup rejects input targets below 6,000 and output limits below
1,000 because the batch schema and a useful response cannot fit reliably below
those floors.

---

## Common subcommands

This is the operational shortlist. Run `ai-memory --help` for the authoritative
full command tree.

Two ways to invoke a subcommand against the docker deploy:

```bash
# A) Against the running container (stateful: status, search, backup,
#    checkpoints, restore-page, audit-contamination, forget-sweep, lint, embed).
docker exec ai-memory ai-memory status --json
docker exec ai-memory ai-memory search "karpathy"
docker exec ai-memory ai-memory backup --to /data/snapshot.tar.gz

# B) One-shot, no running container needed for pure-stdout helpers
#    (generate-auth-token, completions, install-mcp, install-hooks, setup-agent,
#    llm-test).
#    Auth login is stateful: use docker exec against the running container or
#    the wrapper so it writes into the same data volume as the server.
docker run --rm akitaonrails/ai-memory:latest generate-auth-token
docker run --rm akitaonrails/ai-memory:latest completions zsh
docker run --rm akitaonrails/ai-memory:latest install-mcp --client cursor
docker run --rm akitaonrails/ai-memory:latest --help     # full subcommand tree
```

| Subcommand | Pattern | What it does |
|---|---|---|
| `serve` | `docker compose up -d` (already done) | Run the HTTP MCP server |
| `run [harness] [args...]` | host wrapper or native binary | Opt into one managed cross-harness workstream; omit the harness to resume the newest usable local session, or name Claude Code, Codex, OpenCode, Pi, Crush, Kimi Code, Command Code, Kiro CLI v2/v3, OMP, Grok Build CLI, or Antigravity CLI explicitly; exact `--yolo` and `--fresh` flags are wrapper-owned and other native arguments pass through |
| `show [--json]` | host wrapper or native binary | Choose a client-local checkout and installed managed harness, or return structured discovery data without launching; remote servers never provide checkout paths |
| `continue [--workspace NAME]` | host wrapper or native binary | From any directory, revalidate and resume the newest client-local managed checkout; accepts `--yolo` and `--fresh` but no harness-native arguments |
| `workstreams [--workspace NAME] [--project NAME] [--limit N] [--json]` | host wrapper or native binary | List recent workstreams selectable from the current checkout, including current selection and linked harnesses, without exposing paths or native session ids |
| `workstream-search [query]` | managed child or thin HTTP client | Search the complete visible managed-workstream ledger; the managed child receives its workstream id automatically |
| `status` | `docker exec` | Counts, paths, derived-index diagnostics, and passive LLM/embedding provider health |
| `search "<query>"` | `docker exec` | Wiki FTS5 search + bounded source authority; use MCP `memory_query` for entity/graph/vector RRF |
| `write-page` | `docker exec` | Manual page write (atomic + indexed) |
| `backup --to` / `restore --from` | `docker exec` | Snapshot or restore the data dir |
| `checkpoints` / `restore-page` | `docker exec` | List wiki git checkpoints or restore one markdown page and reindex it |
| `audit-contamination` | `docker exec` | Read-only structural audit for likely cross-project contamination |
| `forget-sweep` / `lint` / `embed` | `docker exec` | Manual maintenance; sweep + lint also run on the server schedule by default |
| `commit -m "…"` | `docker exec` | Stage + commit the wiki tree |
| `reset --confirm` | `docker exec` | Wipe data (refuses while siblings alive) |
| `generate-auth-token` | `docker run --rm` | Print a random hex bearer token |
| `auth login openai-oauth` | same data volume as the server | Store a ChatGPT/Codex OAuth refresh token for the optional `openai-oauth` LLM provider |
| `auth login copilot` | same data volume as the server | Store a GitHub token for the optional `copilot` LLM provider |
| `auth login oidc-device` | same developer data dir as native hooks and thin-client CLI commands | Store a per-developer OIDC device token for native hook authentication and HTTP CLI fallback auth |
| `install-mcp --client` | `docker run --rm` | MCP-config snippet per client |
| `install-hooks --agent` | `docker run --rm` | Hook-config snippet for an existing hooks dir |
| `setup-agent --agent --to --host-prefix` | `docker run --rm -v` | Extract bundled scripts + print config (one-shot) |
| `install-instructions [--target] [--print] [--no-skills]` | same host environment used for the agent prompt files | Install or update the slim CLAUDE.md / AGENTS.md routing block and, by default, the managed ai-memory Agent Skills |
| `install-skills [--scope] [--agent]` | same host environment used for the agent skill dirs | Install or update only the managed ai-memory Agent Skills |
| `uninstall --apply` | same host environment used for install | Remove only ai-memory-owned hooks, MCP entries, instruction blocks, managed skill files, and generated plugin files after content/marker validation. Use `--mcp-url` for custom MCP endpoints and `--mcp-name` only to narrow removal. |
| `llm-test --provider …` | `docker run --rm -e …` | Smoke-test an LLM provider |
| `completions <shell>` | `docker run --rm` or native binary | Print a bash/zsh/fish/PowerShell/elvish completion script; see [`shell-completions.md`](shell-completions.md) |

### Managed routing snippets and Agent Skills

ai-memory's routing install is agent-facing prompt packaging. It does not add a
runtime skill router, and `SKILL.md` files are not durable memory pages. The
wiki remains the durable source of truth.

`ai-memory install-instructions` now writes two managed prompt artifacts by
default:

1. A slim instruction block in `CLAUDE.md`, `AGENTS.md`, or the file passed with
   `--target`. The block is bounded by `<!-- ai-memory:start -->` and
   `<!-- ai-memory:end -->` delimiters that appear alone on their own lines.
2. Managed ai-memory Agent Skills containing the detailed tool-routing guidance.

Re-running the command is safe. If a project still has the old long ai-memory
block between line-anchored markers, the refresh replaces that block in place
with the slim snippet, leaves unrelated instructions before and after it alone,
and writes a timestamped `.bak-*` backup before changing an existing file.
Managed skill files contain an ai-memory ownership marker; same-name user skills
without that marker are preserved unless you explicitly force replacement.
Their embedded payloads use LF line endings on every release platform, so CLI
installs and `memory_install_self_routing` return the same bytes on Windows,
Linux, and macOS. This does not rewrite line endings in user-authored files.
`install-instructions --print` previews only the instruction snippet; run
`install-skills --print` when you want to preview the managed skill payloads.

`install-instructions` flags for skills:

| Flag | Meaning |
|---|---|
| `--no-skills` | Refresh only the markered instruction block. |
| `--skills-scope <scope>` | Choose project-local or user-global skill roots. Values: `project`, `global`. Defaults to `project`. |
| `--skills-agent <agent>` | Choose `.claude/skills`, `.agents/skills`, `.devin/skills`, `.grok/skills`, or both Claude/Agents roots. Values: `claude-code`, `agents`, `devin`, `grok`, `both`. By default, `CLAUDE.md` targets imply `claude-code`, `AGENTS.md` targets imply `agents`, and both instruction files imply `both`. |
| `--skills-target-dir <dir>` | Write managed skill directories below an explicit root instead of inferring from scope and agent. |
| `--skills-force` | Replace unmanaged same-name skills during `install-instructions`; without it, they are left untouched and the command exits with an actionable error. |

Use `install-skills` when the instruction block is already right and only the
Agent Skill files need a refresh:

```bash
ai-memory install-skills
ai-memory install-skills --scope global --agent agents
ai-memory install-skills --scope global --agent devin
ai-memory install-skills --scope global --agent grok
ai-memory install-skills --agent both --print
ai-memory install-skills --target-dir .custom/skills --force
```

`install-skills` flags:

| Flag | Meaning |
|---|---|
| `--scope <scope>` | Install into this project or the current user's global skill roots. Values: `project`, `global`. Defaults to `project`. |
| `--agent <agent>` | Install into Claude Code's skill root, the cross-agent skill root, Devin's skill root, Grok's skill root, or both Claude/Agents roots. Values: `claude-code`, `agents`, `devin`, `grok`, `both`. Defaults to `claude-code`. |
| `--target-dir <dir>` | Write managed skill directories below an explicit root; `--scope` and `--agent` are ignored. |
| `--print` | Print target paths and `SKILL.md` contents without writing files. |
| `--force` | Replace unmanaged same-name skills; without it, user-authored same-name skills are preserved. |

Default skill target roots:

| Scope | `--agent claude-code` | `--agent agents` | `--agent devin` | `--agent grok` |
|---|---|---|---|---|
| `project` | `.claude/skills` | `.agents/skills` | `.devin/skills` | `.grok/skills` |
| `global` | `~/.claude/skills` | `~/.agents/skills` | Windows: `%APPDATA%\devin\skills`; non-Windows: `~/.devin/skills` | `$GROK_HOME/skills` (default `~/.grok/skills`) |

Each managed skill is written as `<root>/<skill-name>/SKILL.md`.

`ai-memory uninstall --only skills --apply` removes managed skill files only
from the default project/global roots shown above, after validating the
ai-memory ownership marker. If you installed with `--target-dir` or
`--skills-target-dir`, clean up that custom root manually.

Data dir inside the container is `/data` (mounted via the compose
volume). Outside docker, override with `AI_MEMORY_DATA_DIR=/path`.

Scheduled maintenance is configured in `[maintenance]` in `config.toml`.
By default, rule-based lint and forget sweep run daily outside hook
latency across every existing workspace/project. Embedding backfill is
supported but defaults to off because it can call a paid provider; if you
enable `embedding_backfill_interval_secs` after configuring an embedder,
each scheduled tick backfills every existing workspace/project and may
increase provider usage accordingly.

Forget sweep and rule-based lint persist their last successful completion. On
restart, a job that is not due waits only its remaining interval; a never-run
or overdue job runs once after a bounded startup delay. Failed runs are not
recorded as successful and retry after that bounded delay. Embedding backfill
remains opt-in and keeps its interval-only behavior (no startup catch-up).

---

## Bootstrap mid-project

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
--workspace <NAME>         (default: the nearest `.ai-memory.toml` marker's
                            `workspace`, else "default")
--project <NAME>           (default: the marker's `project` when pinned,
                            else derived from cwd — main repo root's
                            basename via `git rev-parse --show-toplevel`,
                            or basename(cwd) when no repo is found.
                            "scratch" only as a defensive fallback for
                            hook events with no usable cwd.)
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

## Logs and read-only sandboxes

The CLI and server write daily-rolling logs to `<data_dir>/logs/`
(`~/.local/share/ai-memory/logs/` by default). When that location is not
writable — sandboxes like [ai-jail](https://github.com/akitaonrails/ai-jail)
mount `$HOME` read-only or as throwaway tmpfs — ai-memory degrades instead
of failing: it falls back to the OS temp dir, then to stderr-only logging,
printing the exact path that failed at each step. Commands keep working
either way. To keep durable file logs (and durable hook spooling) inside a
sandbox, map the data dir read-write, e.g. `ai-jail --rw-map
~/.local/share/ai-memory …`.

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
safe combination. The server refuses an unauthenticated LAN bind before it
accepts requests. `--allow-insecure-no-auth` can override that refusal only
for an intentional dangerous plain-HTTP deployment; prefer
`AI_MEMORY_AUTH_TOKEN` or loopback instead.

In a *container* that refusal becomes a warning, because the container must
bind `0.0.0.0` internally for `-p` to work at all and cannot see which host
address you published to. The `-p` above is therefore doing the real work: it
is what keeps this container loopback-only. If you change it to publish on a
LAN address, set `AI_MEMORY_AUTH_TOKEN` as well.

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

#### SELinux-enforcing hosts

On SELinux-enforcing Linux systems such as Fedora, RHEL, and openSUSE, normal
home-directory labels can prevent the helper container from reaching agent
config even when its UID and GID match the host user. The wrapper checks both
the host enforcement mode and the engine's advertised security options. For the
short-lived helper commands that touch host files (`install-*`, `setup-agent`,
`uninstall`, `backup`, `restore`, and `bootstrap`), it adds `--security-opt
label=disable`; thin-client commands remain confined when they use the named
data volume and implicit configuration. An explicit `--config` path or a valid
host-backed `AI_MEMORY_DATA_DIR` also activates the host-file treatment. This
relaxes SELinux label confinement only for that trusted helper invocation. It
does not modify the long-lived ai-memory server, which uses an engine-managed
named volume.

`bootstrap` is in that list even though it only *reads* host files: an
unmapped UID and a confined label block reads just as hard, and the failure is
misleading — it degrades silently to `no .git found at /work; bootstrapping
from README/docs/rules only` before dying with `Permission denied (os error
13)`.

The two engines report these facts under different keys. Docker answers
`docker info --format '{{.SecurityOptions}}'`; podman has no such field and
fails that template, so the wrapper falls back to podman's
`{{.Host.Security.Rootless}}` and `{{.Host.Security.SELinuxEnabled}}` when the
Docker probe comes back empty. Rootless engines additionally need `-u 0:0`,
because only container UID 0 maps back to the invoking host user — on rootless
podman with SELinux enforcing, both adjustments are required and neither alone
lets the write land.

Do not add `:z` or `:Z` to the wrapper's whole `$HOME` bind. Docker's
[bind-mount documentation](https://docs.docker.com/engine/storage/bind-mounts/#configure-the-selinux-label)
warns that relabeling system directories such as `/home` can make the host
inoperable. Docker documents `label=disable` in the
[`docker run` security options](https://docs.docker.com/reference/cli/docker/container/run/#security-opt).

`ai-memory run`, `ai-memory show`, `ai-memory continue`, and
`ai-memory workstreams` are the exceptions: the current wrapper intercepts them
and starts a cached checksum-verified native client on the host, where local
checkouts, harness executables, and session stores exist. It preserves an
explicit remote `AI_MEMORY_SERVER_URL`. If one of these commands logs
`data_dir=/data`, cannot find a checkout, or cannot find `codex`, `claude`, or
another host executable, refresh the stale wrapper with `ai-memory upgrade` on
that client machine.

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

The command downloads the wrapper and its SHA-256 checksum from the latest
GitHub Release, refuses an unverified update, pulls the latest Docker
image, re-stages hook scripts under
`~/.local/share/ai-memory/hooks/<agent>/` for configured agents, and
prints how to restart the server container so the new binary is used.
Re-running `install-hooks --apply` remains idempotent: ai-memory
replaces only the hook entries it owns and leaves unrelated hooks alone.
When a Compose file is found, the wrapper first verifies that its project owns
the running `ai-memory` container. A standalone container is never handed to an
unrelated Compose project just because its file occupies a conventional path;
the wrapper instead writes the inspected standalone recreation script for
review, preserving the existing `/data` mount and other runtime options.

Set `AI_MEMORY_NO_VERSION_CHECK=1` to silence the daily check. To pin wrapper
self-upgrades to a fork or tagged release, set `AI_MEMORY_WRAPPER_URL=<url>`;
the wrapper requires `<url>.sha256` unless
`AI_MEMORY_WRAPPER_SHA256_URL=<checksum-url>` is also set.

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
- [`docs/usage.md`](usage.md) - handoffs, proactive querying, web UI, slim
  routing snippet + managed Agent Skills, migration from other memory tools, and raw-wiki inspection
- [`docs/mcp-install.md`](mcp-install.md) - per-client MCP config reference for
  every client in the [README Support Matrix](../README.md#support-matrix)
- [`docs/ARCHITECTURE.md`](ARCHITECTURE.md) - what's actually
  running inside ai-memory
