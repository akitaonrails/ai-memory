# Installation cookbook

The [README quick-start](../README.md#quick-start) covers the happy
paths (the two-command native install, Docker + Claude Code, Arch AUR, Fedora
RPM, macOS menu bar app). This page covers everything else:

- [One-step native install](#one-step-native-install-installsh--ai-memory-setup)
  (`install.sh` + `ai-memory setup`, Linux and macOS, no root)
- [Marketplace plugins](#marketplace-plugins)
  (Claude Code and Codex: hooks, MCP and skills as one plugin)
- [Server on a different machine](#server-on-a-different-machine)
  (homelab, LAN box, remote server)
- [Configuring the CLI URL and auth](#configuring-the-cli-url-and-auth)
- [Fedora native package (RPM)](#fedora-rpm)
  (systemd system service or user service)
- [Arch Linux native packages (AUR)](#arch-linux-native-packages-aur)
  (systemd system service or user service)
- [Nix / NixOS](#nix--nixos)
  (flake package, NixOS module, maintainer test ladder)
- [macOS menu bar app](#macos-menu-bar-app)
  (self-contained `.app` + LaunchAgent)
- [Configuring other agent CLIs](#configuring-other-agent-clis)
  (Codex, Command Code, IBM Bob, Devin CLI, OpenCode, OpenCode 2, OMP, Pi, Cursor, Claude Desktop, Gemini CLI, Qwen Code, Antigravity CLI, Grok Build CLI, GitHub Copilot CLI, Factory Droid, Meta Muse Code, CodeBuddy Code, Hermes Agent, Zero, ZCode, Kimi Code, Kiro CLI, Cline CLI, Pool, OpenClaw, FreeBuff, VS Code Copilot, Zed, and MCP-only recipes for Warp, Kilo Code, Goose and OpenHuman)
- [Installing hooks without docker](#installing-hooks-without-docker)
  (curl-based installer)
- [Running ai-memory without docker](#running-ai-memory-without-docker)
  (mise, building from source)
- [Managed cross-harness workstreams](managed-workstreams.md)
  (`ai-memory run`, transparent native resume, and argument forwarding)
- [LLM provider tiers + self-hosted Ollama](#llm-provider-tiers)
- [Common subcommands](#common-subcommands)
- [Managed routing snippets and Agent Skills](#managed-routing-snippets-and-agent-skills)
- [Human password bootstrap and recovery](#human-password-bootstrap-and-recovery)
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

> **Podman.** The `bin/ai-memory` wrapper automatically uses rootless Podman
> when Docker is not installed. It also works through the `podman-docker`
> `docker` shim; set `AI_MEMORY_DOCKER=podman` to force Podman when both engines
> are installed. The default image name is fully qualified for non-interactive
> Podman short-name resolution. See
> [SELinux-enforcing hosts](#selinux-enforcing-hosts) for how it detects the
> engine's rootless and SELinux state.

---

## One-step native install (`install.sh` + `ai-memory setup`)

On Linux (x86_64, aarch64) and macOS (Apple Silicon, Intel), two commands go
from nothing to a running server with your agents wired. Neither needs root.

```bash
curl -fsSL https://github.com/akitaonrails/ai-memory/releases/latest/download/install.sh | sh
ai-memory setup
```

`install.sh` is published with every release, next to its own
`install.sh.sha256`. It picks `ai-memory-<os>-<arch>.tar.gz` for the machine,
downloads it with its `.sha256` file, and stops before unpacking anything when
the checksum file is missing, names another asset, or does not match. It also
refuses an archive holding an absolute or `..` path, or any member that is not
a plain file or directory (a symlink or hard link), and unpacks with
`--no-same-owner`. Without curl it falls back to wget, and for the default
GitHub URL only to a wget that supports `--https-only` (so a redirect cannot
drop to plain http); BusyBox wget is refused there. The release goes to
`~/.local/lib/ai-memory`, and `~/.local/bin/ai-memory` becomes a link to the
binary, so the bundled `hooks/` directory sits beside it and
`ai-memory upgrade` can later replace both. An `ai-memory` at the link path that
is not such a link (the Docker wrapper, a hand-built binary) is left alone and
the script exits. The whole script runs from one function called on its last
line, so a download cut short runs nothing. To read it before running it:

```bash
curl -fsSLO https://github.com/akitaonrails/ai-memory/releases/latest/download/install.sh
curl -fsSLO https://github.com/akitaonrails/ai-memory/releases/latest/download/install.sh.sha256
sha256sum -c install.sh.sha256   # macOS: shasum -a 256 -c install.sh.sha256
sh install.sh
```

| Variable | Default | Effect |
|---|---|---|
| `AI_MEMORY_VERSION` | latest | Install this release, e.g. `3.0.0` |
| `AI_MEMORY_BIN_DIR` | `~/.local/bin` | Where the `ai-memory` link goes |
| `AI_MEMORY_LIB_DIR` | `~/.local/lib/ai-memory` | Where the release is unpacked |
| `AI_MEMORY_RELEASE_BASE_URL` | GitHub Releases | A mirror serving `latest/download/<asset>` and `download/v<version>/<asset>`; the same variable `ai-memory upgrade` reads. The checksum comes from the same origin as the archive, so use only a mirror you control. |
| `AI_MEMORY_INSTALL_TARGET` | detected | Force `linux-x86_64`, `linux-aarch64`, `macos-x86_64` or `macos-aarch64` |

Native Windows has its own script, `install.ps1`, published next to
`install.sh` with its own `install.ps1.sha256`. In PowerShell (no
administrator rights needed):

```powershell
irm https://github.com/akitaonrails/ai-memory/releases/latest/download/install.ps1 | iex
ai-memory setup
```

It downloads `ai-memory-windows-x86_64.zip` and its `.sha256`, stops before
unpacking when the checksum file is missing, names another asset, or does not
match, and refuses an archive entry that is absolute or holds `..`. The
release goes to `%LOCALAPPDATA%\Programs\ai-memory` (`AI_MEMORY_INSTALL_DIR`
overrides it), which is added to your user `PATH` unless
`AI_MEMORY_NO_MODIFY_PATH=1`. An existing install there is left alone; update it
with `ai-memory upgrade`. `AI_MEMORY_VERSION` and `AI_MEMORY_RELEASE_BASE_URL`
work as for `install.sh`. On Windows on Arm it installs the x86_64 build, which
Windows 11 runs under emulation. Errors are thrown rather than exiting, so a
failure under `iex` leaves your PowerShell window open. Details are in
[`docs/windows.md`](windows.md).

`ai-memory setup [--yes] [--agent <name>]... [--no-service] [--plan [--json]] [--web [--no-open]]`
runs, in order, the commands a native install otherwise needs one by one:

1. `init`. On a fresh data dir this mints the root bearer token into
   `<data_dir>/auth-tokens/default` (owner-only) and records
   `[auth] bearer_token_file`, as described in
   [`[auth]` root bearer](ARCHITECTURE.md#auth-root-bearer). An existing install
   keeps its config and its auth posture; `ai-memory auth enable` is still the
   opt-in for a token there.
2. A per-user service running `serve --transport http --enable-web` on that
   data dir and config: `~/.config/systemd/user/ai-memory.service` (or under
   `$XDG_CONFIG_HOME`) on Linux, rendered from the packaged user unit, or
   `~/Library/LaunchAgents/com.github.akitaonrails.ai-memory.plist` on macOS,
   rendered from the packaged LaunchAgent. The unit runs the binary `setup` ran
   from. When that binary sits in a versioned package location (Homebrew's
   `Cellar/ai-memory/<version>` or a `/nix/store` path, both gone after an
   upgrade and `brew cleanup` or garbage collection), the unit names a stable
   link that resolves to the same file instead: the path as invoked, an
   `ai-memory` on `PATH` such as `~/.nix-profile/bin/ai-memory`, or Homebrew's
   `opt/ai-memory/bin/ai-memory` or `bin/ai-memory`. With no such link it names
   the versioned path, and you re-run `setup` after upgrading.
   On Windows there is no per-user service manager, so `setup` writes an
   `ai-memory` value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
   (no administrator rights needed) that runs `ai-memory service-host` at
   logon, and starts that host right away. The host runs `serve --transport
   http --enable-web` on the same data dir and config with no console window
   and restarts it after a failure, giving up after five failures within ten
   seconds of starting; see
   [`docs/windows.md`](windows.md#scenario-e-keep-the-server-running-native-windows-service).
   The step is skipped when an ai-memory server already answers at the configured
   URL and no unit or logon entry of ours exists yet (Docker, a packaged system
   unit, a foreground `serve`), when the configured server is not on this
   machine, and with `--no-service`. Setup requires a successful
   `application/json` response with `{"status":"ok"}` from `/healthz` before
   treating a listener as healthy. An unrecognized response leaves any local
   service unchanged and fails the self-check with a diagnostic. Check which
   process owns the address, then free it or change `bind` in the named
   `config.toml` and re-run setup; update an explicit `server_url` too.
   OpenCode's background service also defaults to port 49374 and can be moved
   with `opencode service set port <free-port>`.
3. For each agent: `install-mcp --apply`, `install-hooks --apply` and
   `install-skills --scope global`. With no `--agent`, `setup` wires every
   harness whose session store or MCP config exists under your home, using the
   same store locations as `doctor` (including `CLAUDE_CONFIG_DIR`,
   `CODEX_HOME` and similar relocations). The plan numbers each agent and
   says what it was found by (its session store, its MCP config, or for Muse
   its directory), then lists the other agents `--agent` accepts. Cursor,
   Gemini CLI, Droid and the other agents with no store to look for are
   wired by naming them. OpenCode 1 and 2 share one session store, so a
   detected or named `opencode` runs `opencode --version` once and wires the
   plugin and MCP entry for the major it reports, as `install-mcp --client
   opencode` and `install-hooks --agent opencode` do by hand. When that probe
   fails (no `opencode` on `PATH`, a timeout, or unreadable output), `setup`
   writes no OpenCode plugin or MCP entry and reports the problem; `--agent
   opencode2` forces V2 without probing. The Kiro CLI v3 engine shares a
   store with Kiro CLI, so it is wired only when named.
4. A self-check that writes nothing: `GET /healthz`, a `memory_status` call
   over MCP with the install's credential, and `doctor`. When Codex was wired,
   `setup` also asks Codex whether it will run the ai-memory hooks. If Codex
   has not trusted them yet, the closing line says so and tells you to open
   Codex, run `/hooks`, and trust them, instead of reporting the install as
   ready. That still exits 0, because only you can grant the trust in Codex.

Without `--yes`, `setup` prints this plan and asks on a terminal. `y` applies
it. With more than one agent in the plan you can instead type the numbers of
the agents to wire (`1 3` or `1,3`); every other step still runs, and an answer
naming a number outside the plan changes nothing. With no terminal (a script,
an agent's tool call) it changes nothing and exits non-zero, asking for
`--yes`. `--plan` prints the plan and exits without changing anything;
`--plan --json` prints it as a JSON document for a script or front end, with a
`format` field that is bumped only when a field changes meaning or is removed.

`ai-memory setup --web` shows the same plan as a page in your browser instead
of the terminal. It prints a one-time address on `127.0.0.1` (and opens it,
unless `--no-open` or there is no desktop), where you tick the agents to wire
and press Apply. Apply runs `ai-memory setup --yes --agent <each one you
picked>` and streams its output to the page, so the result is the same as the
terminal command; `--no-service` is passed through. The page answers only
requests that name its own address, carry the secret part of the URL, and,
for a change, come from the page itself, so another site open in the same
browser cannot drive it. It holds no token. It stops when you press Finish,
after 15 minutes without a request, or on Ctrl-C. On a remote machine,
forward the port it prints (`ssh -L <port>:127.0.0.1:<port> <host>`) and open
the same address locally. Each step is idempotent: a second run rewrites no
file and leaves a running service alone. A step that fails does not stop the
others; `setup` lists each `problem:` at the end and exits non-zero. When every step
passed it ends by naming the agents to restart and the question to ask one of
them, "What do you remember about this project?": answering it takes a call
to ai-memory's tools, so the reply shows the wiring works. The token
is never printed or passed on a command line, and no harness config holds it:
hooks read the credential files `init` writes, and MCP clients reach the
server through `ai-memory mcp-bridge`.

The service serves the web UI at `http://127.0.0.1:49374/web`, and `setup`
prints that address at the end. A fresh install has a bearer token and no user
accounts, so the browser asks for a username and password: type any username,
and the contents of `<data_dir>/auth-tokens/default` as the password. To sign
in with a password of your own instead, create the first root account as in
[first-time root](#first-time-root-greenfield).

`uninstall --apply` removes what `setup` wired, the service included. Every
unit `setup` renders carries an identity line holding the SHA-256 of the rest of
the file, and `uninstall` removes only a unit whose line is there and still
matches: on Linux it runs `systemctl --user disable --now ai-memory.service`,
deletes the file and runs `systemctl --user daemon-reload`; on macOS it boots
the LaunchAgent out and deletes the plist. A unit `setup` did not write, or one
edited since, is left in place and `uninstall` says so. If you need to remove
such a unit, or the service manager refused, do it by hand:

```bash
# Linux
systemctl --user disable --now ai-memory.service
rm ~/.config/systemd/user/ai-memory.service
systemctl --user daemon-reload
# macOS
launchctl bootout "gui/$(id -u)/com.github.akitaonrails.ai-memory"
rm ~/Library/LaunchAgents/com.github.akitaonrails.ai-memory.plist
```

An AI agent installing ai-memory for its user can follow
[`INSTALL_FOR_AGENTS.md`](../INSTALL_FOR_AGENTS.md), which comes down to
`ai-memory setup --yes --agent <self>`.

`scripts/onboarding-acceptance.sh` measures this path in a clean temporary
home against a locally built release: two commands, and about one second from
the first command to a `memory_query` that returns a captured hook event (a
debug build on a Linux workstation, local embeddings off so no model download
is counted). The target is 60 seconds.

## Marketplace plugins

From 3.0 this repository is also a plugin marketplace. A plugin wires one
agent in a single step: lifecycle hooks, the ai-memory MCP tools, the routing
skills, and a `status` skill that runs `ai-memory doctor` and summarizes it.
It is the alternative to `install-hooks` + `install-mcp` + `install-skills`
for that agent; `ai-memory setup` does the rest (data dir, service, other
agents) either way.

The plugin does not contain ai-memory. Install the binary first (above, or
Homebrew, AUR, RPM, the Docker wrapper, the macOS app), then add the plugin.
The plugins need ai-memory 3.0 or later: an older binary requires
`--server-url` on every hook, so the launcher retries it against
`http://127.0.0.1:49374` with a one-line upgrade hint on stderr, and the MCP
entry does not connect until the binary is upgraded.
Every hook runs `scripts/ai-memory-hook`, a short POSIX script that finds
the binary and hands it the event with `ai-memory hook`, the same native
command `install-hooks` wires. Capture policy (`ignore_paths`, allowlist
mode), marker scope and server profiles therefore apply exactly as they do
for settings-file hooks. Without the binary the plugin captures nothing,
prints one line at session start, and never blocks the agent. The plugins
have no telemetry and hold no credentials: hooks and the MCP bridge use the
credentials the CLI already stores (`ai-memory setup`, `auth login`).

The catalogs pin each plugin to the release tag, so a marketplace add only
installs released plugin content. Generated files live under `plugins/`
(see [`plugins/README.md`](../plugins/README.md)).

### Claude Code

```sh
claude plugin marketplace add akitaonrails/ai-memory
claude plugin install ai-memory@ai-memory
```

(or `/plugin marketplace add akitaonrails/ai-memory` and `/plugin install
ai-memory@ai-memory` inside Claude Code). The plugin works in the CLI and in
the desktop app's Code tab. Options, all optional, are set with `/plugin
configure ai-memory@ai-memory`:

| Option | Default | Use |
| --- | --- | --- |
| `server_url` | empty | Empty uses the `server_url` from your ai-memory config, so the plugin and the CLI never disagree (the launcher omits `--server-url`). Set it only to point this plugin elsewhere. |
| `ai_memory_bin` | `ai-memory` | Absolute path to the binary when it is not on the `PATH` the app sees; a relative path is ignored, since it would resolve inside the checkout. The launcher also tries `~/.local/bin`, `~/.cargo/bin`, `/opt/homebrew/bin`, `/usr/local/bin` and `/usr/bin`. |
| `data_dir` | empty | Only for an install with a non-default data directory. |

- Claude Code does not auto-update a third-party marketplace
  unless you turn that on. `ai-memory upgrade` runs `claude plugin
  marketplace update ai-memory` and `claude plugin update ai-memory@ai-memory`
  after replacing the binary; with a package-managed binary (Homebrew, AUR,
  RPM) run those two commands yourself after the package upgrade.
- If `install-hooks` already put ai-memory hooks in
  `~/.claude/settings.json` or a checkout's `.claude/settings.local.json`,
  those win: the plugin skips every event they deliver, so nothing is
  captured twice. Only a hook with an empty or `*` matcher counts; one you
  narrowed to some tools leaves the plugin capturing the others. `ai-memory doctor` reports the double install and which
  copy to remove. While the plugin is enabled, `ai-memory setup` and
  `ai-memory run` stop writing Claude's settings hooks, MCP entry and
  skills; run `ai-memory install-hooks --agent claude-code --apply` (or
  `install-mcp`, `install-skills`) to add them anyway.
- Folderless sessions (a desktop scratch workspace with no project folder)
  are not captured through the plugin.
- Factory Droid can install this plugin through its Claude marketplace
  fallback. Its events are recorded as `droid`, and when `install-hooks
  --agent droid` hooks deliver the same event (`~/.factory/hooks.json`, or
  the `hooks` key of `~/.factory/settings.json` while `hooks.json` does not
  exist) the plugin skips it, as it does for Claude's settings hooks.
- Meta Muse Code reads `.claude-plugin/plugin.json` bundles, but Muse Code
  1.4.7 refuses to install this one (`plugin declares a retained unsupported
  MCP boundary`) because its `.mcp.json` uses `${user_config.*}`
  placeholders; use `install-hooks --agent muse` and `install-mcp --client
  muse` instead. The launcher is ready for a Muse that loads it: it tells Muse
  apart by the `MUSE_PLUGIN_ID` and `MUSE_SESSION_ID` Muse sets for plugin
  hooks and records the events as `muse`, skips an event that
  `install-hooks --agent muse` hooks already deliver (a catch-all matcher
  under the `hooks` key of `${XDG_CONFIG_HOME:-~/.config}/muse/settings.json`),
  and drops the plugin's `SubagentStart` and `SubagentStop` events, which
  `install-hooks --agent muse` never wires.
- On Windows, Claude Code runs plugin hooks through Git Bash, which it
  already requires, so the launcher needs nothing else.
- With the Docker wrapper, plugin hooks need a `bin/ai-memory` wrapper that runs
  `ai-memory hook` through its native host client, not in a container
  (otherwise each event pays a `docker run` and `127.0.0.1` points inside
  the container). Run `ai-memory upgrade` to get the wrapper with that
  routing.
- Each release attaches
  `ai-memory-claude-plugin.zip` for **Customize > Plugins > Upload plugin**.
  In Cowork, capture runs on your computer and needs ai-memory installed
  there; the memory tools still work.
- Remove it with `claude plugin uninstall ai-memory@ai-memory`;
  `ai-memory uninstall` does it too.

### Codex

```sh
codex plugin marketplace add akitaonrails/ai-memory
codex plugin add ai-memory@ai-memory
```

Then open Codex, run `/hooks`, and trust the ai-memory hooks. Codex runs a
plugin's hooks only after you trust them, and skips an untrusted one without
a message. Trust is keyed on the hook command before `${PLUGIN_ROOT}` is
expanded, and the commands stay identical across releases, so a plugin
upgrade keeps it; a release that has to change a command says so in the
changelog. `ai-memory doctor` and the `status` skill report hooks Codex
skips. The Codex plugin has no options: the server URL and credentials come
from your ai-memory config, and the MCP entry runs `ai-memory mcp-bridge
--no-session-id` from `PATH`. On Windows the hooks call `ai-memory hook`
directly (`commandWindows`), so `ai-memory` must be on `PATH`. That direct
call is a known limitation: it skips the launcher, so a Windows event is not
marked as plugin-launched (folderless sessions are captured, as with a
settings hook), and without the binary Codex reports a failed hook instead
of the launcher's silent no-op. Changing it would also change the command
Codex keys hook trust on.

Codex runs plugin hooks and `~/.codex/hooks.json` hooks side by side, and an
untrusted settings hook is skipped silently, so the plugin cannot safely drop
its copy the way the Claude Code plugin does. Keep one: `ai-memory setup`
does not add Codex settings hooks while the plugin is enabled, and `doctor`
flags a double install. To update, run `codex plugin marketplace upgrade
ai-memory` and `codex plugin add ai-memory@ai-memory` (`ai-memory upgrade`
prints them). Remove with `codex plugin remove ai-memory@ai-memory` or
`ai-memory uninstall`.

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

That wires the hooks user-wide (`~/.claude/settings.json`), so every Claude
Code session is captured. To opt in per repository instead, run from inside
the checkout:

```bash
ai-memory install-hooks --agent claude-code --scope project --apply
```

This writes the repository's gitignored `.claude/settings.local.json` at the
git root (the main checkout's root from a worktree), which is where Claude Code
reads that file even when launched from a subdirectory (see
[where Claude Code looks for each file](https://code.claude.com/docs/en/settings#where-claude-code-looks-for-each-file)).
On Windows, and when the repository root is your home directory, Claude Code
reads the launch directory instead, so the installer writes to the current
directory there: run it, and launch Claude Code, from the same directory.
The user-level file is left alone. Claude Code merges the project file's
hooks with any user-level ones, so pick one scope per machine; the installer
notes when the other scope already carries ai-memory hooks.
The installer warns when the file is not ignored by git (Claude Code adds
`**/.claude/settings.local.json` to your global excludes only when it creates
the file itself) and refuses to embed a bearer token in it when the token
cannot be persisted under the data dir. When it updates an existing file, the
backup goes to `<data_dir>/backups/claude-settings-local/` (owner-only), not
next to the file, so nothing new appears in `git status`.
`ai-memory uninstall --only hooks --apply` run from inside the checkout
removes the entries again; run elsewhere it does not reach into the
repository, so run it in every project-scoped checkout before removing
ai-memory. Otherwise those files keep calling the removed binary and data
dir. `upgrade` and the `ai-memory run` auto-wire leave the user-level file
alone while a project-scoped install is the only one present. `upgrade`
replaces the binary the hook commands call but does not rewrite project
files; re-run `install-hooks --scope project --apply` in each checkout to pick
up hook-command changes. `setup-agent` and `backup-agents` only know the
user-level file. Claude Code only; other agents keep their user-level hook
files.

`--session-aware` is an optional Claude Code MCP mode:

```bash
ai-memory install-mcp --client claude-code --session-aware --apply
```

It replaces the static HTTP MCP entry with a local ai-memory stdio bridge that
still connects to the configured remote server and bearer token, while
forwarding Claude's lifecycle session id. On a Docker-wrapper install the
wrapper runs the bridge through its checksum-verified native host client, so
the configured loopback server URL resolves on the host. Pair it with
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
discover an older arbitrary `CLAUDE_CONFIG_DIR` that is no longer set.
`install-hooks --scope project` ignores the variable: Claude Code resolves
project files from the checkout, not from the config dir. The
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

If you install into `AGENTS.md` and the project is also used from Claude Code,
make `CLAUDE.md` import it with a bare `@AGENTS.md` first line. Claude Code
2.1.277 and later reads `AGENTS.md` only when no `CLAUDE.md`,
`.claude/CLAUDE.md`, or `CLAUDE.local.md` exists in the working directory or
any directory above it. Without that import, sessions that read only
`CLAUDE.md` (older Claude Code, the AGENTS.md plugin disabled, the `claude-md`
Project instructions setting, or a developer's own `CLAUDE.local.md`) miss the
installed block at session start. See
[Claude Code memory](https://code.claude.com/docs/en/memory#agents-md).

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

By default an undeclared checkout with a valid `upstream` or `origin` remote
uses the canonical repository-path project name; current clients send that
choice explicitly for upgrade safety. Repositories without a valid remote use
`basename(cwd)`, including from a git subdirectory. Because an agent shell keeps its working directory between tool calls, a single
`mkdir sub && cd sub` reparents the rest of the session into a phantom project
named `sub`. To make every session for an install resolve its project from the
git repo root instead, collapsing subdirectories and worktrees, bake the
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
take precedence; see
[the marker-file reference](marker-file.md#install-wide-default-no-marker).

---

## Fedora (RPM)

Download the `x86_64` or `aarch64` RPM from the
[latest GitHub release](https://github.com/akitaonrails/ai-memory/releases/latest)
and install it with `sudo dnf install ./ai-memory-*.rpm`. The package includes
the binary, hook sources, system and user systemd units, and system service
configuration. On a single-user machine, run `ai-memory setup`, as in the
[one-step native install](#one-step-native-install-installsh--ai-memory-setup).
For the manual route, follow the [user-level service](#user-level-service) or
[system service](#system-service) steps below.

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

The short route is `ai-memory setup`, which does everything below and wires
your agents too (see the
[one-step native install](#one-step-native-install-installsh--ai-memory-setup)).
It writes its own unit to `~/.config/systemd/user/ai-memory.service`, which
takes precedence over the packaged one of the same name, with the config at
`~/.local/share/ai-memory/config.toml`. The manual steps below use the packaged
unit and its paths instead.

```bash
mkdir -p ~/.config/ai-memory ~/.local/share/ai-memory
ai-memory \
  --data-dir ~/.local/share/ai-memory \
  --config ~/.config/ai-memory/config.toml \
  init
```

`init` on a fresh data dir already turned bearer auth on (the token is in
`auth-tokens/default` under the data dir). Edit the provider settings if you
want LLM consolidation:

```bash
$EDITOR ~/.config/ai-memory/config.toml
$EDITOR ~/.config/ai-memory/env
```

`init` on a fresh data dir also writes a random root bearer token to
`~/.local/share/ai-memory/auth-tokens/default` (owner-only) and records
`bearer_token_file = "auth-tokens/default"` in the config, so the service
requires it on every route except `/healthz`. You never copy it by hand:
`install-hooks --apply` and `install-mcp --apply` find it in the data dir,
the hooks read it from there, and Claude Code is registered as the
`ai-memory mcp-bridge` stdio bridge, which reads the file at launch. Codex and
Prime Agent reference `$AI_MEMORY_AUTH_TOKEN` instead; export it where you
start them:

```bash
export AI_MEMORY_AUTH_TOKEN="$(cat ~/.local/share/ai-memory/auth-tokens/default)"
```

Every other client `install-mcp` supports gets a local stdio entry that
runs `ai-memory mcp-bridge --no-session-id`, which also reads the token file
at launch, so `ai-memory` must be on the `PATH` the client sees. ZCode only
takes HTTP entries and is refused; add its entry by hand with the header if
you accept the token in its config.

An install created before this default keeps running without auth.
`ai-memory status` then shows `auth: none (loopback)`; run
`ai-memory auth enable` and restart the service to turn the token on.

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
curl -s http://127.0.0.1:49374/healthz
# Expect {"status":"ok"}. /healthz needs no token; `curl -sI http://127.0.0.1:49374/mcp`
# answers 401 without one.
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

`init` also wrote the root bearer token to
`/var/lib/ai-memory/auth-tokens/default`, readable only by the `ai-memory`
user. To wire your own login's agents to this server, copy it into your
user's data dir once; `install-mcp` and `install-hooks` then find it there:

```bash
install -d -m 0700 ~/.local/share/ai-memory/auth-tokens
sudo cat /var/lib/ai-memory/auth-tokens/default \
  | install -m 0600 /dev/stdin ~/.local/share/ai-memory/auth-tokens/default
```

For LAN exposure, set a non-loopback bind and allowed hosts in
`/etc/ai-memory/config.toml`. The bearer `init` wrote already protects every
route; set `AI_MEMORY_AUTH_TOKEN` in `/etc/ai-memory/env` only to replace it:

```toml
bind = "0.0.0.0:49374"
allowed_hosts = ["homelab", "192.168.0.90", "localhost", "127.0.0.1"]
```

`server_url` does not need to be set here. When neither `config.toml` nor
`AI_MEMORY_SERVER_URL` names one, the thin-client commands derive it from
`bind`, so a local `ai-memory status` reaches this server rather than the
default port. An unspecified bind is mapped to loopback (`0.0.0.0` becomes
`127.0.0.1`, `[::]` becomes `[::1]`). Set `server_url` only when clients
reach the server at an address it does not bind, such as behind a reverse
proxy or a tunnel:

```toml
server_url = "https://memory.example"
```

```bash
# optional: replace the token init wrote
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
# 401 Unauthorized without the bearer token.
```

### LLM provider login with native services

> **You do not need a paid platform API key.** ai-memory's LLM features
> (consolidation, lint, auto-improve) are opt-in, and when you enable them you
> can authenticate with a **subscription you already pay for** instead of a
> metered API key: a ChatGPT Plus/Pro/Codex plan via `openai-oauth`
> (`ai-memory auth login openai-oauth`) or a GitHub Copilot plan via `copilot`
> (`ai-memory auth login copilot`). (Claude Pro/Max via `anthropic-oauth` is
> currently refused by Anthropic; see
> [below](#anthropic-via-claude-subscription-oauth).) See
> [`docs/llm-providers.md`](llm-providers.md) for the full table. And you can
> skip an LLM entirely: the default zero-LLM path still captures, searches
> (FTS), and writes rule-based summaries with no provider at all, and
> [`docs/local-embeddings.md`](local-embeddings.md) makes vector search
> keyless too.

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

The issuer, and the device and token endpoints its discovery document lists,
must use `https`. Plain `http` is accepted only on `localhost`, `127.0.0.1`
and `::1`, for a local test issuer. Authentication requests do not follow
redirects, and refresh revalidates the token endpoint stored by older releases
before sending a refresh token.

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

For a local loopback server (the CLI finds the token file `init` wrote, so no
token goes on the command line):

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
native commands where supported. The Linux/macOS Docker wrapper also routes
ordinary `install-hooks` through its checksum-verified native host client, so
the quick-start path is covered. Legacy `.sh`/`.ps1` hooks, an explicit
`AI_MEMORY_HOOK_PLATFORM=posix|windows`, `setup-agent`, and remote-only/manual
Docker script bundles do not enforce it. Re-run `install-hooks --agent
<agent> --apply` or refresh/reinstall generated plugins after upgrading;
installer capability output reflects the selected integration. See the canonical
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
checkouts captures every new one automatically. Forgetting a marker means
capturing more, not less. Allowlist mode inverts that:

```bash
ai-memory install-hooks --apply --capture-mode allowlist
```

A repository without a marker then emits **no lifecycle event at all**, not even a
trimmed one. The event is dropped in the hook process before it can reach the
local spool or the wire, so nothing is written to disk for a repository that
never opted in. To opt a repository in, place a `.ai-memory.toml`
marker in it, which is the same file that already configures routing and
`ignore_paths`.

**It is enforced by native `ai-memory hook` commands only**, the same
boundary that already applies to `[capture] ignore_paths`, and for the same
reason: the gate runs inside the hook binary, immediately before it spools.

That is what a normal `install-hooks --apply` writes on Linux, macOS and
Windows, so the usual install is covered. It is the *script* installs that are
not: the bundled shell/PowerShell hooks POST to the server directly and never
execute the binary, so nothing reads the mode. In practice that means the
legacy `posix`/`windows` platform override (`AI_MEMORY_HOOK_PLATFORM`), the
remote `setup-agent` flow, and manual Docker script-bundle installs. The
Linux/macOS Docker wrapper's ordinary `install-hooks` path uses its native host
client and is covered. `install-hooks --apply` prints the mode and warns when
the install it is writing cannot enforce it.

Within that boundary the mode is not per-agent: it is stored once in the data
directory and every native hook command reads it, whichever agent invoked it.
That also means a later bare
`install-hooks --apply`, including the auto-refresh inside `ai-memory upgrade`,
leaves it alone by construction rather than by re-detecting it. Every
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

**Send different repositories to different servers.** One install can deliver
capture to several ai-memory servers, for example one per organisation. Register
each server once with `ai-memory server add <name> --url <url> --root <dir>
--auth-token-stdin`, then set `server = "<name>"` in each repository's
`.ai-memory.toml`. Repositories without the key keep using the server
`install-hooks` configured. A profile that does not resolve drops the event
instead of falling back. Native hook commands route profiles; the generated
TypeScript integrations and the script hooks drop routed repositories. See
[`marker-file.md`](marker-file.md#routing-capture-to-another-server-server).

Some agent harnesses attach the assistant's final turn to their `Stop` event;
Claude Code sends it as a raw `last_assistant_message`. By default that text is
never persisted: the native hook binary strips the raw field before it can reach
the local spool or the wire, and the server strips it defensively on arrival.

**Opt-in capture (#196).** You can opt in to storing a sanitized, 2 KB-capped
excerpt of the assistant's final turn as the Stop body. It is a **double
opt-in**: enable the server first, then the client:

1. **Server:** set `capture_assistant = true` in the live
   `<data_dir>/config.toml` (or the service's configured TOML file), or set
   `AI_MEMORY_CAPTURE_ASSISTANT=true`, then restart `ai-memory serve`.
2. **Client:** re-install the Claude Code (or Codex) hooks with the flag:

   ```bash
   ai-memory install-hooks --agent claude-code --capture-assistant --apply
   # Codex is supported too — its Stop payload carries last_assistant_message:
   ai-memory install-hooks --agent codex --capture-assistant --apply
   ```

The client sanitizes (built-in patterns) and truncates the excerpt before it
touches the spool or wire; the server re-scrubs with its `[sanitize]` patterns
before storing. If either side is off, or the marker is malformed, the Stop
stays empty. Re-running `install-hooks` without `--capture-assistant` preserves
an existing opt-in. To remove it, run `ai-memory uninstall --only hooks
--apply --yes`, then reinstall without the flag. `--capture-assistant` is Claude Code, Codex and
OpenCode 2 (`--agent opencode2`) on a native hook platform only; on any other agent or the script fallback the
installer refuses it rather than enabling something that cannot take effect. Assistant text is
privacy-sensitive. Read the `SECURITY.md` notes on what it can contain and where
it flows (consolidation/reviewer prompts, and out to a cloud LLM provider if one
is configured) before enabling it.

Upgrading the binary is sufficient for native Claude Code installs, and pending
spooled events drain with the raw field stripped as well. The Linux/macOS Docker
wrapper installs the same native hook command through its checksum-verified
host client. Installs that explicitly select the `.sh`/`.ps1` compatibility
fallback (`AI_MEMORY_HOOK_PLATFORM=posix|windows`), or use a remote/manual
Docker script bundle, cannot sanitize the assistant text, so a `Stop` payload
still carrying the raw field is dropped whole by the script rather than POSTed
verbatim. Even when that fallback is retained, the server strips any raw field
on receipt before persistence.

Native `ai-memory hook --event ...` commands spool events locally. The POSIX
shell bundle and the PowerShell (`.ps1`) bundle spool too, but only on
failure: they POST first and write the event to the same
`<data_dir>/hook-spool/` contract when the server is unreachable, answers
5xx, or answers 408, 425 or 429 (a saturated server asks for a retry), then
flush the backlog behind the next delivery that succeeds. Any other 4xx
is a permanent rejection and is not retried. Session start
does a short bounded cleanup drain before fetching a handoff; cancellation-prone
boundary events (`stop`, `pre-compact`, and `session-end`) start a detached
`hook-drain` helper so delivery does not depend on one shutdown hook surviving.
The script bundles assign one idempotency key before their initial POST and
keep that key if the event enters the spool. A server that processed an event
but lost the response will not duplicate its observation or completed session-end
effects; if processing stopped after the observation commit, the retry re-runs
downstream work. SessionEnd atomically
commits its end watermark with its automatic handoff; a retry that finds that
transaction complete finishes any interrupted wiki commit, durable provider
enqueue, and ingest-key completion without adding a second handoff. Those
incomplete effects remain at-least-once until the server marks the event
complete.
On Unix, the helper uses a trusted `setsid` launcher when available and falls
back to a separate process group otherwise; Windows uses detached/breakaway
process flags. The spool is capped, so a permanently undrained backlog is
eventually pruned rather than unbounded, but old undelivered events can be lost.

Native hook drains drop an event after 8 failed passes by default, including
passes when the server is unreachable. Set `max_attempts = 0` under
`[hook_spool]` in `<data_dir>/config.toml` (or set
`AI_MEMORY_HOOK_SPOOL_MAX_ATTEMPTS=0`) to keep
retrying until the 7-day age limit or 10,000-file spool cap applies. A positive
value sets the number of failed passes before a drop. The same bounds apply to
`ai-memory run`'s degraded offline launches, which rely on this spool (see
[Degraded offline launches](managed-workstreams.md#degraded-offline-launches)).

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
| `AI_MEMORY_HOOK_SPOOL_MAX_ATTEMPTS` | 8 failed passes | non-negative integer | failed drain passes before dropping an event; `0` disables only attempt-count drops |

The retry setting can also be written as `max_attempts` under `[hook_spool]` in
`<data_dir>/config.toml`; the runtime environment wins when both are set. A set
but invalid `AI_MEMORY_HOOK_SPOOL_MAX_ATTEMPTS` value (non-numeric) falls back to
the built-in default of 8, overriding any `config.toml` value, the same convention
as the timing vars. A zero
value still leaves the 7-day age limit and 10,000-file spool cap active.

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

## Nix / NixOS

User-facing NixOS module setup lives in the [README NixOS
section](../README.md#nixos) (`nixosModules.default`,
`services.ai-memory.enable`). This section is the maintainer test ladder
for the flake and module: what CI runs, and what each tier proves.

The package does not bundle `git`: the module adds `pkgs.git` to the
service's `PATH`, and a package-only install needs `git` installed beside it.
Do not wrap the binary to supply it (see the README section for why).

### Maintainer test ladder

1. **Package smoke**: `nix build .#packages.<system>.default` then
   `scripts/check-nix-packaging.sh ./result` (binary `--version`, hooks
   tree, config template, `nix run`). Linux runs on every path-filtered
   `nix.yml` PR; Darwin is schedule / `workflow_dispatch` / `nix` or
   `full-ci` label only.
2. **Eval contracts**: `nix build .#checks.x86_64-linux.nixos-module-eval`
   and `nixos-sandbox-parity`. Cheap Linux-only asserts for enable/bind/
   `--config`/secrets wiring, API-only vs web mounts, refusal messages (age+sops mutex, secrets in
   `settings.auth`, `settings.bind`), escaped `ExecStart` (`--data-dir`,
   `serve`, `--transport http`), default `StateDirectory` vs custom
   `dataDir` tmpfiles/`ReadWritePaths`, and sandbox key parity with
   `nix/systemd-sandbox.nix`. Runs on path-filtered PRs with the Linux
   package job.
3. **Toplevel → OCI → container smoke**: one closure path. Building
   `packages.x86_64-linux.nixos-ai-memory-docker` builds
   `system.build.toplevel` once (via the nixpkgs docker-image tarball);
   there is no separate bare-toplevel CI job. Then:

```bash
scripts/test-nixos-systemd-container.sh
```

   That script imports the rootfs, runs a privileged systemd container,
   checks `systemctl is-active ai-memory` and in-container `curl /healthz`
   (the module's default loopback bind is unreachable via Docker `-p`),
   writes a
   marker under `/var/lib/ai-memory`, restarts the unit, and (by default)
   remounts a named volume once. Gated in `nix.yml` to schedule /
   `workflow_dispatch` / `nix` or `full-ci` label (not every Cargo.lock
   bump).

**Non-goals** (do not treat these as covered by the ladder above):

- A→B flake/package upgrade or SQLite-wiki migration matrices
- Exhaustive typed settings coverage (unknown TOML keys use `freeformType`)
- Soft/fake systemd without a real unit start
- Claiming the published app Docker image covers the NixOS module path
- Darwin / multi-arch NixOS OCI (Linux x86_64 only by design)

Useful knobs for the container smoke:

```bash
AI_MEMORY_NIXOS_TEST_KEEP=1 scripts/test-nixos-systemd-container.sh
AI_MEMORY_NIXOS_TEST_VOLUME=0 scripts/test-nixos-systemd-container.sh
AI_MEMORY_NIXOS_TEST_IMAGE=ai-memory-nixos-test scripts/test-nixos-systemd-container.sh
AI_MEMORY_DOCKER=podman scripts/test-nixos-systemd-container.sh
```

---

## macOS menu bar app

On a Mac, the self-contained menu bar app is the GUI install: it bundles the
native `ai-memory` binary and `hooks/` tree, governs the existing LaunchAgent
(`com.github.akitaonrails.ai-memory`), and opens `/web`, `ai-memory status`,
`config.toml`, the data directory, and logs. It does not replace those tools
with a second dashboard.

Needs a Rust toolchain and Xcode / Swift 6 (the same as a source build):

```bash
git clone https://github.com/akitaonrails/ai-memory
cd ai-memory
./companions/ai-memory-macos/build.sh
open "companions/ai-memory-macos/dist/AI Memory.app"
```

Drag **AI Memory.app** to `/Applications`, then **Install & Start Server**
from the menu extra (no Dock icon). When the status item is green, wire an
agent with the bundled binary so `install-hooks` finds the sibling `hooks/`
tree:

```bash
BIN="/Applications/AI Memory.app/Contents/Resources/runtime/ai-memory"
"$BIN" install-mcp --client claude-code --apply
"$BIN" install-hooks --agent claude-code --apply
```

Durable memory stays in `~/Library/Application Support/ai-memory`. Replacing
the `.app` is an update and does not rewrite that tree. Prebuilt tarball,
source-build, Docker-wrapper, and hand-installed launchd paths remain in
[`docs/macos.md`](macos.md). Companion source:
[`companions/ai-memory-macos`](../companions/ai-memory-macos).

---

## Configuring other agent CLIs

> `install-mcp --server-url` accepts either the bare server origin or the full
> MCP endpoint and appends a missing `/mcp` exactly once.
> `install-hooks --server-url` takes the bare server **origin**
> (e.g. `http://homelab:49374`); hook scripts append `/hook`, `/handoff`,
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
> enforce capture-policy v1. The Linux/macOS Docker wrapper downloads a
> checksum-verified native host client for ordinary `install-hooks` calls.
> Remote-only/manual Docker script installs still use the
> two-step path: (1) `docker cp` bundled scripts to your home dir, (2)
> `docker run --rm install-hooks` renders the config snippet.
> OpenClaw, OpenCode, OMP, and Pi are different: they use generated
> TypeScript plugin/extension files, so no shell-script extraction is
> needed for those clients.

### OpenAI Codex

```bash
# MCP snippet (merge into $CODEX_HOME/config.toml, default ~/.codex/config.toml):
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

The Codex entry `install-mcp` writes sets `default_tools_approval_mode =
"approve"`, so ai-memory's read tools run without a prompt, and adds
`[mcp_servers.<name>.tools.<tool>]` with `approval_mode = "prompt"` for every
tool annotated destructive (`memory_delete_page`, `memory_forget_sweep`,
`memory_restore_page`, the handoff accept and cancel tools, the message pop and
cancel tools, and `memory_feedback`), so Codex asks before running them. A
re-install keeps any per-tool `approval_mode` you set yourself. Both keys are in
Codex's [configuration reference](https://developers.openai.com/codex/config-reference).

Native Codex tool hooks use top-level `tool_name`, `tool_input`, `tool_response`,
and `tool_use_id` fields (verified against CLI 0.154.0). ai-memory records the
tool family and call ID on `PreToolUse` and `PostToolUse`; recognized tools such
as `Bash` and `apply_patch` also retain a sanitized response excerpt on
`PostToolUse`, capped at 2 KB including metadata. Structured JSON responses are
flattened using the same bounded excerpt path. Inputs are not copied into
observations, and unknown tools (including unrecognized MCP names) retain only
metadata. `PostToolUse` alone does not prove success, so Codex outcomes remain
`unknown`.

Capture exclusions still run before native spooling. Codex's `apply_patch`
passes patch text in `tool_input.command`, which does not provide direct file
paths to the capture policy. With active `ignore_paths`, those events retain
only metadata; ai-memory does not parse patch or shell text to infer paths.
Tool events are delivered at the normal 32-event catch-up threshold or a
lifecycle drain boundary, so a small active turn may still have queued events.

Codex CLI 0.145.0 and later expose a native `SessionEnd` hook. `Stop` is captured
as a turn boundary and leaves the session open; a native `SessionEnd` triggers
the final summary, handoff, and auto-improvement eligibility. See the
[Codex hook lifecycle](https://learn.chatgpt.com/docs/hooks#sessionend) for when
Codex ends a session. For older clients or a missed session-end delivery, run:

```bash
ai-memory finalize-session
# add --all to close every matching open Codex session in this workspace/project
# or target one exact concurrent session (mutually exclusive with --all)
ai-memory finalize-session --session-id <uuid>
```

### Antigravity lifecycle hooks

Antigravity (the `agy` CLI, the desktop App and the IDE, which read the same
global `~/.gemini/config/hooks.json`) has no conversation-end event, and none
of its hook payloads carries the user's prompt. `install-hooks --agent
antigravity-cli` wires five of its
[documented hooks](https://antigravity.google/docs/hooks/):

- `SessionStart` records the session start and injects a pending handoff as
  an `injectSteps` system message, which stays in the conversation for later
  turns.
- `PreInvocation` runs before every model call. Only `invocationNum = 0`, the
  start of a user turn, does anything: the native hook reads the turn's prompt
  from the conversation's transcript (`transcriptPath`) and records it as a
  user prompt. On a build that fires no `SessionStart`, the first turn's
  `PreInvocation` records the start and claims the handoff instead. A
  per-conversation marker keeps either start to once, so a handoff created
  while the conversation winds down stays open for the next session.
- `PreToolUse` and `PostToolUse` capture tool calls.
- `Stop` ends one execution loop. When the model finished the turn
  (`terminationReason` `NO_TOOL_CALL`, spelled `model_stop` on the web page)
  and nothing is left running (`fullyIdle: true`), the server writes a turn
  checkpoint: it refreshes the session's summary page and its one open
  handoff and leaves the conversation open, the path OpenCode 2 uses. Prompts
  the transcript gained since the turn started are recorded ahead of the
  `Stop`, so the checkpoint includes them. A cancelled, failed or
  budget-limited loop is not a checkpoint.

The transcript is read only when `transcriptPath` names the conversation's own
`~/.gemini/<app>/brain/<conversationId>/.system_generated/logs/transcript.jsonl`
(or the `transcript_full.jsonl` beside it, which the CLI reports), with
`<app>` one of `antigravity`, `antigravity-cli` or `antigravity-ide`. Another
file, another conversation's transcript and a symlinked directory that leads
out of the conversation are refused. One hook reads at most 8 MiB under a
100 ms filesystem deadline; a timeout leaves the cursor untouched so the next
hook retries. The bounded cursor under
`<data_dir>/hook-state/antigravity-transcripts/` resumes where the previous
read stopped. Steps a hook injected are skipped, the text inside
the CLI's `<USER_REQUEST>` wrapper is kept, and each prompt is capped and
scrubbed like any other before the server sanitizes it again. Its ingest key
comes from the conversation and step, so a repeated read stores it once. The
shell and PowerShell fallback hooks do not parse the transcript, and a
checkout the capture allowlist does not admit is not read at all.

Nothing tells a closed Antigravity window from a conversation still in use,
so a checkpoint's handoff goes to a new session in the same project only once
the conversation has been quiet for 10 minutes. To hand it over at once, or
to queue opt-in SessionEnd consolidation, close the conversation explicitly:

```bash
ai-memory finalize-session --agent antigravity-cli
# add --all only to close every matching open Antigravity session in this scope
# or add --session-id <uuid> to close one exact concurrent session
# if the conversation continued after a first finalize, re-close it to cover
# the new observations (a re-run with nothing new is a harmless no-op)
ai-memory finalize-session --agent antigravity-cli --reopen --session-id <uuid>
```

For a session whose prompts the live hooks did not capture (the shell
fallback hooks, or an install from before transcript capture),
`finalize-session` replays its typed prompts from `agy`'s
`~/.gemini/antigravity-cli/history.jsonl` before the session-end (only lines
whose conversation and workspace match the session), and the summary page and
handoff are built with them. Each prompt is keyed as `backfill` keys it, so a
re-close stores it once and a prompt typed after the first finalize arrives
with the next. A checkout the capture allowlist does not admit sends none,
`--all-owners` replays none (other users' prompts are not in this machine's
history). When history is missing or yields no prompt, export falls back to
the selected conversation's bounded, containment-checked brain transcript and
keeps only explicit user-input steps. `agy -p` writes no `history.jsonl` line,
so this fallback supplies its prompt.

Rerun `ai-memory install-hooks --agent antigravity-cli --apply` after
upgrading. An install from an earlier release runs its `session-start` handler
on `PreInvocation` and has no `SessionStart` entry; the native hook reads that
as the first-turn fallback, with transcript prompts and turn checkpoints, until
the reinstall adds `SessionStart`.

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
flavored json schema"). The ai-memory server answers flavored requests with
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
provider/model settings the same file holds. Entries cover 10 events
(`SessionStart`, `SessionEnd`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`,
`PostToolUseFailure` (Kimi Code reports tool failures separately from
successful calls; it reuses the post-tool-use handler), `Stop`,
`SubagentStart`, `SubagentStop`, and `PreCompact`) and default to
native `ai-memory hook --event … --agent kimi-code` commands on local installs
(local spool plus batched delivery, capture-policy v1 enforced); the staged
script bundle under `~/.local/share/ai-memory/hooks/kimi-code/` is the
compatibility fallback (POSTs to `/hook`, spooling a failed delivery for a
later drain, without capture-policy v1 enforcement). A pending handoff
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

### IBM Bob

IBM Bob IDE and Bob Shell share their global settings below `~/.bob/settings`.
Install MCP and lifecycle capture together:

```bash
ai-memory install-mcp --client ibm-bob --apply \
    --server-url "https://memory.example/mcp" \
    --auth-token "$TOKEN"
ai-memory install-hooks --agent ibm-bob --apply \
    --server-url "https://memory.example" \
    --auth-token "$TOKEN"
```

The `bob` alias is accepted. MCP is merged into `mcp.json` as Bob's native
`streamable-http` entry with `protocolNegotiation: "legacy"`; this skips Bob's
non-standard discovery probe and uses the standard MCP handshake. Hooks are
merged into the root `hooks` object of `settings.json`, preserving other
settings and handlers. The installer uses only the five events common to the
IDE and Shell, delivers the pending handoff as plain `SessionStart` context,
and enforces capture exclusions for tool events.

Bob has no true session-end event. Use `ai-memory finalize-session --agent
ibm-bob` after the final turn when you need the summary and handoff immediately.
Managed `ai-memory run ibm-bob` resume is not supported because Bob does not
document a stable native transcript store. See [the IBM Bob MCP and hook
details](mcp-install.md#ibm-bob).

### Qwen Code

Qwen Code (`qwen`) keeps MCP servers and lifecycle hooks in the same user
file, `~/.qwen/settings.json`. Install both together:

```bash
ai-memory install-mcp --client qwen-code --apply \
    --server-url "https://memory.example/mcp" \
    --auth-token "$TOKEN"
ai-memory install-hooks --agent qwen-code --apply \
    --server-url "https://memory.example" \
    --auth-token "$TOKEN"
```

The `qwen` and `qwencode` aliases are accepted. MCP is merged under
`mcpServers` as Qwen's `httpUrl` streamable-HTTP entry (`timeout` in
milliseconds); the plain `url` key is the legacy SSE transport. A token-file
install writes a stdio `ai-memory mcp-bridge --no-session-id` entry instead of
putting the bearer in `headers`. Hooks are merged into the root `hooks`
object, preserving other settings and handlers. Each entry runs the native
`ai-memory hook --event … --agent qwen-code` command with an empty matcher;
there is no script bundle. Ten events are wired: `SessionStart`,
`UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PostToolUseFailure` (Qwen
fires it instead of `PostToolUse` when a tool throws), `PreCompact`, `Stop`,
`SessionEnd`, `SubagentStart` and `SubagentStop`. Capture exclusions apply to
Qwen's Gemini-like tools (`write_file`, `read_file`, `run_shell_command`,
`grep_search`, `glob`). User-prompt capture uses only Qwen's
provenance-bearing `submitted_prompt`; automated continuation or retry content
that appears only in `prompt` is not stored or interpreted as a reload request.

The `SessionStart` hook delivers the prior session's handoff through
`hookSpecificOutput.additionalContext`. With nothing to deliver, `SessionStart`
and `UserPromptSubmit` print nothing, because Qwen injects that stdout into
the session. An interactive `SessionStart` may arrive without `cwd`; the hook
then uses `QWEN_PROJECT_DIR`, which Qwen exports on every command hook. When
neither names a directory, the hook still records the event but fetches no
handoff; the baton stays open for `memory_handoff_accept`. Qwen fires a real
`SessionEnd`, so `finalize-session` is not needed.

`ai-memory setup` wires Qwen Code when `~/.qwen` exists.
`ai-memory uninstall` removes the entries. Qwen reads `AGENTS.md` and
`QWEN.md`; a Qwen-only `backup-agents` run preserves both files,
`~/.qwen/settings.json`, `~/.qwen/skills`, and shared `~/.agents/skills`.
Managed `ai-memory run qwen` resume is not supported because Qwen
does not document a stable native transcript store. Start a new Qwen session
after installing so it reloads `settings.json`. See [the Qwen Code MCP and
hook details](mcp-install.md#qwen-code).

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
the root of each repository it runs in; there is no user-global hook file for
ai-memory to merge. `install-hooks --agent pool` (alias `poolside`) therefore
stages the hook scripts to the stable user-global location and prints a
ready-to-paste `hooks:` snippet; ai-memory deliberately does not write
committed files inside your repositories (the gitignored
`.claude/settings.local.json` that `--scope project` writes for Claude Code is
the one exception).

```bash
# Stage the scripts and print the snippet to paste into
# <repo>/.poolside/settings.yaml:
ai-memory install-hooks --agent pool --apply \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"
```

The snippet wires Pool's five documented events: `SessionStart`,
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
only and never fetches the (single-use) handoff. Recover a prior session's
handoff via the MCP `memory_handoff_accept` tool. No first-party
`install-mcp` client and no managed workstream (`ai-memory run pool`) are
claimed: Pool's native session-store contract is not demonstrated, per
[managed-harness contributions](managed-harness-contributions.md).

### ZCode (z.ai)

ZCode wires lifecycle hooks in the root `hooks` block of
`~/.zcode/cli/config.json`, the same file that holds its other CLI
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
[…]}`. ZCode spawns them with the event JSON on stdin and no shell, which the
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
no true session-end event (`Stop` fires at the end of every turn), so close
finished sessions explicitly; use the exact id when several ZCode sessions are
open in the same project:

```bash
ai-memory finalize-session --agent zcode
ai-memory finalize-session --agent zcode --session-id <uuid>
```

No first-party `install-mcp` client and no managed workstream
(`ai-memory run zcode`) are claimed yet.

### GitHub Copilot CLI

Copilot CLI keeps its config in `~/.copilot` (or `$COPILOT_HOME`).
`install-mcp --client copilot-cli` merges the remote server entry into
`mcp-config.json` there, and `install-hooks --agent copilot-cli` writes
`hooks/ai-memory.json` in Copilot's standalone `{"version": 1, "hooks": {…}}`
format, merging around any third-party entries already in each file:

```bash
ai-memory install-mcp --client copilot-cli --apply \
    --server-url "http://homelab:49374/mcp" \
    --auth-token "$TOKEN"
ai-memory install-hooks --agent copilot-cli --apply \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"
```

See [the MCP guide](mcp-install.md#github-copilot-cli) for the exact entry and
the project-level `.mcp.json` / `.github/mcp.json` alternatives.

The events are configured with PascalCase names (`SessionStart`,
`PreToolUse`, …), which makes Copilot send the VS Code/Claude-compatible
snake_case payload (`session_id`, `cwd`, `tool_name`, `tool_input`,
`tool_result`), so ai-memory's existing extraction applies and native commands
enforce `[capture] ignore_paths`. Ten events are wired: Claude Code's nine plus
`PostToolUseFailure`, which Copilot fires instead of `PostToolUse` when a tool
errors. Entries sit directly in each event array with no `matcher`: Copilot
rejects `matcher` on `SessionStart`, `Stop`, `SessionEnd`, and the subagent
events, and omitting it elsewhere means "every invocation". Tool output is
read from `tool_result.text_result_for_llm`.

The `SessionStart` hook delivers the prior session's handoff: Copilot reads a
top-level `additionalContext` from `SessionStart` stdout (not Claude Code's
`hookSpecificOutput` envelope), and the hook prints exactly that, or `{}` when
nothing is pending.

`--scope project` is intentionally unsupported for Copilot CLI. Copilot's
repository hook files (`.github/hooks/*.json`) are versioned, shared with the
team and loaded by the Copilot cloud agent, while ai-memory's hook entries
carry this machine's absolute executable and data-dir paths; committing them
would point every teammate at one person's install and server.
`ai-memory run copilot` (alias `copilot-cli`) launches Copilot CLI as a
managed workstream harness (see [managed workstreams](managed-workstreams.md));
`install-mcp --client copilot` remains the VS Code Copilot client.

### Factory Droid

Droid (`droid`) keeps its user config in `~/.factory`. `install-mcp --client
droid` (alias `factory-droid`) merges the server entry into `mcp.json` there,
and `install-hooks --agent droid` merges lifecycle hooks into `hooks.json`,
keeping every entry you already have in either file (#1240):

```bash
ai-memory install-mcp --client droid --apply \
    --server-url "http://homelab:49374/mcp"
ai-memory install-hooks --agent droid --apply \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"
```

The MCP entry is `{"type": "http", "url": …}` under `mcpServers`. Droid
expands `${NAME}` in `headers` but never in `url`, so the URL is written
literally. When the server has a bearer token, the entry gets
`"headers": {"Authorization": "Bearer ${AI_MEMORY_AUTH_TOKEN}"}` instead of
the token, so export `AI_MEMORY_AUTH_TOKEN` in the shell that starts droid:
Droid fails that connection, naming the variable, when it is unset. A
project's `.factory/mcp.json` also works (pass it with `--config-file`), but
Droid's docs advise keeping auth headers out of that committed file. Without a
token, `droid mcp add ai-memory http://127.0.0.1:49374/mcp --type http` writes
the same entry.

`~/.factory/hooks.json` holds the event map itself, with no `hooks` wrapper.
Droid sends Claude Code-shaped JSON on stdin (`session_id`, `cwd`,
`tool_name`, `tool_input`, `tool_response`), so each entry runs the native
`ai-memory hook --event … --agent droid` command; there is no script bundle,
and the local spool, bearer auth and `[capture] ignore_paths` apply. Droid's
`Read`, `Create`, `Edit`, `ApplyPatch` (a Codex-style patch envelope),
`LS`, `Grep`, `Glob` and `Execute` tools are mapped to the file, search and
shell families those exclusions use, verified against the Droid 0.240.0 tool
schemas. Droid payloads carry no tool-call id. Seven events are wired:
`SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PreCompact`,
`Stop` and `SessionEnd`. `SubagentStop` is left out because Droid has no
`SubagentStart` to pair it with.

Droid reads the `hooks` key of `~/.factory/settings.json` only while
`hooks.json` does not exist. If your hooks live there, `--apply` refuses to
create `hooks.json`, which would switch them off, and says so. Either move
that `hooks` object into `~/.factory/hooks.json` and run the command again, or
merge the entries `ai-memory install-hooks --agent droid` prints (without
`--apply`) under the `hooks` key of `settings.json` yourself.

The `SessionStart` hook delivers the prior session's handoff through
`hookSpecificOutput.additionalContext`, which Droid adds to the session, and
Droid's own `SessionEnd` closes the session, so `finalize-session` is not
needed. Start a new droid session after installing so it loads the hooks.
`ai-memory setup --agent droid` runs both installers; `setup` does not detect
Droid on its own, and no managed workstream (`ai-memory run droid`) is claimed.

### Meta Muse Code

Muse Code (`muse`, also installed as `muse-code`) keeps its user settings in
`${XDG_CONFIG_HOME:-~/.config}/muse/settings.json`, one file for both its
hooks and its MCP servers. `install-hooks --agent muse` (aliases `muse-code`,
`musecode`) merges lifecycle hooks under its `hooks` key and
`install-mcp --client muse` merges the MCP entry, keeping
`"schema_version": 1`, every other key and every entry you already have:

```bash
ai-memory install-mcp --client muse --apply
ai-memory install-hooks --agent muse --apply
```

Muse fails every command at startup when `settings.json` has no
`schema_version`, so both installers add `"schema_version": 1` when it is
missing and never rewrite an existing value. Each `--apply` writes a backup
next to the file when it changes it, and running it again changes nothing.

Muse sends Claude Code-shaped JSON on stdin (`session_id`, `cwd`,
`tool_name`, `tool_input`, `tool_use_id`, `tool_response`), so each entry
runs the native `ai-memory hook --event … --agent muse` command with an empty
matcher and a 30-second `timeout`. There is no script bundle. Muse runs hooks
with a cleared environment (`HOME`, `PATH`, `PWD` and its own `MUSE_*`
variables), so the server URL and the data dir are written into each
command, and `AI_MEMORY_*` variables exported in your shell do not reach the
hooks; run the installer again after changing either. The local spool,
bearer auth and `[capture] ignore_paths` apply. Muse's `read_file`,
`write_file`, `edit_file`, `apply_patch`, `search`, `bash`, `monitor`,
`web_fetch` and memory tools map to the file, search and shell families
those exclusions use, and a relative `path` is resolved against the session's
`cwd`. A `project`-scope memory path lands under `.agents/memory/`; the
personal memory scopes live outside the repository and are stored as
metadata only while a `[capture]` marker is active.

Eight events are wired: `SessionStart`, `UserPromptSubmit`, `PreToolUse`,
`PostToolUse`, `PostToolUseFailure` (Muse fires it instead of `PostToolUse`
when a tool fails), `PreCompact`, `Stop` and `SessionEnd`. `SubagentStart` and
`SubagentStop` are left out: Muse fires them on every turn for its built-in
reminder observers, each under its own session id, so wiring them would
record several empty sessions per prompt. The `SessionStart` hook delivers the
prior session's handoff through `hookSpecificOutput.additionalContext`,
which Muse adds to the session as developer context, and Muse's own
`SessionEnd` closes the session, so `finalize-session` is not needed. With no
handoff to deliver, the `SessionStart` and `UserPromptSubmit` hooks print
nothing, since Muse adds that stdout to the model's context too.

The MCP entry is a stdio `ai-memory mcp-bridge --agent muse` with
`"mode": "optional"` (Muse's default `required` aborts the run when a server
cannot start). Muse gives its stdio servers `MUSE_SESSION_ID`, and the bridge
forwards it, so Muse is a session-aware client: unscoped memory calls follow
the session's own project. A streamable-HTTP entry would carry no session
id. The entry goes under `mcpServers`, or under the legacy `mcp_servers` map
when the file already has one, and never under both: Muse ignores every MCP
server when both maps are present, so `--apply` refuses a file that already
has both and `ai-memory doctor` reports it. The entry names the install's
data dir (`--data-dir`) because the bridge starts in Muse's cleared
environment, and the bridge reads the token from there; a token passed
explicitly with `--auth-token` goes in the entry's `env` instead, so treat the
file as private then. `ai-memory` must be on the `PATH` Muse starts with.

The ai-memory routing skills need no extra step: Muse reads
`~/.agents/skills`, which `install-skills` writes, and it reads `AGENTS.md`
first among the instruction files. `ai-memory setup` detects Muse by its
config or data directory and runs all three installers;
`ai-memory setup --agent muse` names it explicitly. Muse also reads a
project's `.muse/hooks.json` in a trusted workspace; ai-memory does not write
it, because its commands carry this machine's paths. No managed workstream
(`ai-memory run muse`) and no transcript backfill from
`${XDG_DATA_HOME:-~/.local/share}/muse/sessions` are claimed yet.

### CodeBuddy Code

CodeBuddy Code (`codebuddy`), Tencent's coding agent, reads lifecycle hooks
from the `hooks` key of `~/.codebuddy/settings.json`.
`install-hooks --agent codebuddy` (alias `code-buddy`) merges ai-memory's
entries there, keeping your other settings and any hooks you already have
(#1299):

```bash
ai-memory install-hooks --agent codebuddy --apply
```

Without `--apply` the command prints the block to merge by hand, and
`ai-memory setup-agent --agent codebuddy` prints the same block for a host
that runs CodeBuddy against a remote server. Running `--apply` again changes
nothing.

CodeBuddy sends Claude Code-shaped JSON on stdin (`session_id`, `cwd`,
`tool_name`, `tool_input`, `tool_use_id`, `tool_response`) with Claude Code's
tool names, so each entry runs the native
`ai-memory hook --event … --agent codebuddy` command with an empty matcher.
There is no script bundle; the local spool, bearer auth and
`[capture] ignore_paths` apply. Seven events are wired: `SessionStart`,
`UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PreCompact`, `Stop` and
`SessionEnd`.

The `SessionStart` hook delivers the prior session's handoff through
`hookSpecificOutput.additionalContext`, which CodeBuddy appends to the
session. With nothing to deliver, the `SessionStart` and `UserPromptSubmit`
hooks print nothing, since CodeBuddy appends their output too. An interactive
session can start without a `cwd` in the payload; the hook then uses
`CODEBUDDY_PROJECT_DIR`, which CodeBuddy sets for its hooks. When neither
names a directory, the hook still records the event but fetches no handoff,
because the server cannot tell which project's handoff to claim; the
handoff stays open for `memory_handoff_list` and `memory_handoff_accept`.

`ai-memory setup` wires CodeBuddy when `~/.codebuddy` exists, and
`ai-memory uninstall` removes the entries. There is no `install-mcp` client
for CodeBuddy yet, no transcript backfill, no session close for headless
`codebuddy -p` runs and no managed workstream. This integration follows
CodeBuddy's hook documentation and the payloads reported on v2.164 to
v2.167; the maintainers have not yet run it end to end. Start a new
CodeBuddy session after installing so it loads the hooks.

### Hermes Agent (Nous Research)

Hermes declares lifecycle hooks in the `hooks:` block of
`~/.hermes/config.yaml` (`$HERMES_HOME/config.yaml`). `install-hooks --agent
hermes` (alias `hermes-agent`) prints the block to paste there:

```bash
ai-memory install-hooks --agent hermes --server-url "http://homelab:49374"
```

Hermes splits each configured `command` with `shlex.split` and runs it with
**no shell**, passing the event JSON on stdin, so the generated entry invokes
the native `ai-memory hook` command directly (the same exec-form shape Zero and
ZCode use) and there is no script bundle to stage. ai-memory never writes the
file, `--apply` included: `~/.hermes/config.yaml` is YAML you also edit, and
Hermes gates user hooks behind its own acceptance prompt (`hooks_auto_accept`).
Paste the block, let Hermes accept it, and restart Hermes and its gateway.

Six events are wired, each with a 20-second timeout:

| Hermes event | ai-memory event | What it does |
| --- | --- | --- |
| `pre_tool_call`, `post_tool_call` | `pre-tool-use`, `post-tool-use` | Tool observations, tool-family titles and `[capture] ignore_paths` enforcement. |
| `on_session_start` | `session-start` | Records the start. Hermes ignores this hook's output, so no handoff is fetched here. |
| `pre_llm_call` | `user-prompt-submit` | Captures the prompt from `extra.user_message` (text parts only, never the conversation history). On every primary-session turn it checks for a pending handoff and prints it as `{"context": "..."}`, which Hermes adds to that turn. This also works after a native Hermes session resumes. |
| `on_session_end` | `stop` | A turn boundary: Hermes fires it after every turn, not at session end. |
| `on_session_finalize` | `session-end` | Ends the session on CLI exit, `/new` or gateway shutdown. After a gateway `/new` only the ending session is closed. |

A `delegate_task` child runs as a Hermes session of its own (`platform`
`subagent`). Its lifecycle events are dropped: its goal is not a user prompt,
its first turn must not claim the handoff, and nothing would ever finalize it.
Its tool events are still captured.

A gateway session that expires idle is never finalized by Hermes. Close it
from the session's project directory, or name it with `--session-id`:

```bash
ai-memory finalize-session --agent hermes
ai-memory finalize-session --agent hermes --session-id <id>
```

Hermes reports its own process directory as the payload `cwd`; for a gateway
that is wherever the gateway started. The hook routes by `TERMINAL_CWD`
instead, the variable Hermes exports `terminal.cwd` through, when it names an
existing absolute directory. A multiplexing gateway serves every profile from
the default profile's process and exports only the default profile's
`terminal.cwd`, so inside a gateway the variable is used only for events the
`default` profile fired; another profile's events keep the process directory.
A tool call can still move to another checkout
when its absolute location proves one: `terminal`'s `workdir`, a file tool's
`path`, or `search_files`' `path`. With neither, an event stays with the
process directory and, under `capture_mode = allowlist`, is captured only if
that directory carries a marker. See [marker-file.md](marker-file.md) for the
routing rules.

The community [`ai-memory-hermes-plugin`](https://github.com/MrLuciano/ai-memory-hermes-plugin)
memory provider also posts prompts and session ends and claims the handoff.
When it is installed in `$HERMES_HOME/plugins/ai-memory`, `install-hooks` and
`setup-agent` print only the two tool events, so prompts are not captured twice
and the handoff is not claimed twice. If you install the plugin later, delete
the four lifecycle entries. The check only asks whether that directory exists
where the command runs: a plugin disabled with `hermes plugins disable` but
left in place still selects the tool events alone, and a plugin installed
under a profile's `HERMES_HOME` is seen only when the command runs with that
`HERMES_HOME`. Adjust the pasted block by hand in either case.

For MCP, `ai-memory install-mcp --client hermes` prints an `mcp_servers:` entry
for the same file (see [mcp-install.md](mcp-install.md#hermes-agent)). Hermes
does not read MCP server instructions, so also install the skills:
`ai-memory install-skills --scope global --agent hermes` writes them to
`~/.hermes/skills`. No managed workstream (`ai-memory run hermes`) is claimed.

### OpenCode

```bash
# Host-side commands probe the installed executable and select V1/V2.
ai-memory install-mcp --client opencode --apply \
    --server-url "http://homelab:49374/mcp" \
    --auth-token "$TOKEN"
ai-memory install-hooks --agent opencode --apply \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"

# Docker cannot inspect the host executable. Generate V1 artifacts explicitly:
docker run --rm akitaonrails/ai-memory:latest \
    setup-agent --agent opencode --opencode-dialect v1 --to /tmp/unused \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"
```

Restart OpenCode after installing or changing the plugin; plugins are
loaded at startup. The generic `opencode` client/agent spelling runs the installed
`opencode --version` directly, accepts only major 1 or 2, and selects the matching
plugin and MCP schema. Under `ai-memory run`, executable lookup, the version probe,
and launch share the same captured environment plus `--env` / `--env-file`
overrides. Malformed output, a timeout, or an unsupported future major fails with
an actionable error instead of guessing.

### OpenCode 2 (beta)

The generated plugin targets the OpenCode 2.0.10+ event API (checked against
2.0.14). OpenCode 2 runs one long-lived service behind every CLI, so closing a
terminal is not a session end: each completed root turn instead writes a
deterministic checkpoint (no LLM call) of `sessions/<id>.md` and refreshes the
session's automatic handoff, keeping one open baton per live session. The next
session claims the latest checkpoint of a session that has captured nothing for
ten minutes. Nothing tells a closed terminal from a parallel session still at
work in the same directory, so this is a heuristic: a session in use keeps its
baton, one session's turn never retires another live session's baton, and a
claim retires only older batons of quiet sessions. A tool or model call that
runs longer than ten minutes without a captured event looks quiet. Startup context is claimed once per root
session and retained on every later model request; child sessions never claim
it or publish a baton.

Assistant text stays opt-in, as for Claude Code and Codex: set
`capture_assistant = true` on the server and install with
`ai-memory install-hooks --agent opencode --capture-assistant --apply`; the
generic spelling verifies that the installed executable is V2 first. The
plugin hands the last completed text to the native hook, which sanitizes and
caps it before it reaches the spool or the wire; the excerpt then rides in the
next session's automatic handoff. A bare re-apply preserves the opt-in.

For a commented `opencode.jsonc`, preview `install-mcp --client opencode2` and
merge the entry into the existing `mcp.servers` object by hand: the apply path
writes strict JSON.

The `opencode2`, `opencode-v2`, and `open-code2` spellings remain compatibility
aliases that force the V2 contracts without probing. They share V1's config dir
and session store. Prefer the generic `opencode` spelling for normal installs:

```bash
# Host-side generic commands auto-detect V2; compatibility aliases force it.
ai-memory install-hooks --agent opencode2 --apply \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"

# Docker cannot inspect the host executable. Generate V2 artifacts explicitly:
docker run --rm akitaonrails/ai-memory:latest \
    setup-agent --agent opencode --opencode-dialect v2 --to /tmp/unused \
    --server-url "http://homelab:49374" \
    --auth-token "$TOKEN"
```

V2 nests servers under `mcp.servers` and uses a different plugin API. A
successful `--apply` removes the incompatible sibling plugin only when its
exact ai-memory ownership markers match. It likewise migrates an alternate MCP
entry only when its endpoint and shape prove ai-memory ownership. Hook and MCP
installation reuse its endpoint and bearer across a major transition; conflicting
owned V1/V2 entries fail for explicit resolution, while user-authored entries are
preserved. Repeating either install is
byte-idempotent, and `uninstall` continues to recognize both historical files
and MCP locations.

> **Back up `~/.local/share/opencode` before running the beta against
> your real data.** The two binaries share the one `opencode.db` file and
> the beta has migrated its schema in place before, leaving stable
> `opencode` 1.x broken
> ([upstream #42260](https://github.com/anomalyco/opencode/issues/42260)).
> ai-memory only ever opens that database read-only; the migration risk
> comes from launching `opencode2` itself, not from this integration.
>
> **The beta's background service defaults to port 49374, which is also ai-memory's
> default.** Running both on defaults crash-loops the opencode2 service
> (`Managed service port 49374 ... is already in use`). Move one side:
> `opencode2 service set port <free-port>`, or start ai-memory with
> `--bind 127.0.0.1:<free-port>` (and matching `--server-url` installs).

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
not interchangeable: only Pi's bridges MCP tools.

#### OMP profiles

`omp --profile <name>` relocates OMP's agent home to
`~/.omp/profiles/<name>/agent`. Point the installers at the same profile so
the extension and MCP entry are installed in that directory:

```bash
ai-memory install-hooks --agent omp --profile work --apply
# Or pass OMP_PROFILE to each installer:
OMP_PROFILE=work ai-memory install-hooks --agent omp --apply
OMP_PROFILE=work ai-memory install-mcp --client omp --apply
```

`--profile` takes precedence over `OMP_PROFILE`. The legacy `PI_PROFILE` is
used only when `OMP_PROFILE` is unset. Names are trimmed and validated using
OMP's rules. An empty environment value, a whitespace-only name or `default`
selects the default profile; an explicit `--profile ""` is rejected.

A named profile ignores `PI_CODING_AGENT_DIR`. The default profile uses it
unless it points to the named profile directory exported by a parent OMP
process. `PI_CONFIG_DIR` changes the `.omp` root relative to your home using
OMP's path-joining rules; it does not replace an explicit agent directory.
The extension and `mcp.json` stay in the agent directory even when sessions
move to XDG storage. See [native adapter behavior](managed-workstreams.md#native-adapter-behavior)
for the session paths.

`uninstall --profile <name>` removes the profile's integration files. It also
checks the default profile and the legacy `~/.omp` locations, including when
`PI_CONFIG_DIR` selects a different root.

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
    install-mcp --client qwen-code       --auth-token "$TOKEN" \
    --server-url "http://homelab:49374/mcp"

docker run --rm akitaonrails/ai-memory:latest \
    install-hooks --agent qwen-code      --auth-token "$TOKEN" \
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

docker run --rm akitaonrails/ai-memory:latest \
    install-mcp --client dsh             --auth-token "$TOKEN" \
    --server-url "http://homelab:49374/mcp"

docker run --rm akitaonrails/ai-memory:latest \
    install-mcp --client prime-agent     --auth-token "$TOKEN" \
    --server-url "http://homelab:49374/mcp"
```

Cursor, Gemini CLI, Qwen Code, Antigravity CLI, Grok Build CLI, Kiro CLI, Command Code, and OpenClaw support both
`install-mcp` and `install-hooks`. Grok's `install-mcp --client grok` writes
`$GROK_HOME/config.toml` (default `~/.grok/config.toml`); its hooks live under
`$GROK_HOME/hooks` (default `~/.grok/hooks`). `install-hooks --agent grok`
captures lifecycle events.
Grok ignores `SessionStart` stdout and discards an allowing `UserPromptSubmit`,
so those hooks do not accept the handoff. `install-hooks --agent grok --apply`
also writes `$GROK_HOME/rules/ai-memory-handoff.md` so the first turn calls
`memory_handoff_list`. Grok reaches MCP tools only through its `use_tool`
meta-tool, so the rule hands the model the qualified catalog key
`ai-memory__memory_handoff_list` instead of leaving it to `search_tool`. That
call comes before any file read, and the rule corrects `workspace`/`project`
from `./.ai-memory.toml` only when the list comes back empty or with a scope error. The
first `PostToolUse` prints
`hookSpecificOutput.additionalContext` (pending handoff, plus an opted-in
`[briefing]`). The model sees it after that tool result. A session with no
tool call and without that rule leaves the handoff for
`memory_handoff_accept`. Claude Desktop, VS Code Copilot, Zed, ZCode, and
Prime Agent are MCP-only here, so you'll need to
nudge the model to call `memory_query` / `memory_handoff_accept` itself.
DeepSeek Harness (DSH) also has no hook stdout to carry the baton: its
`@deepseek-ai/dsh-hooks-claude-code` bridge runs `SessionStart` detached, so
`install-hooks --agent dsh` captures lifecycle events but handoff recovery
goes through `memory_handoff_list` then `memory_handoff_accept`, and finished
sessions close with `ai-memory finalize-session --agent dsh`.
Prime Agent's snippet names `$AI_MEMORY_AUTH_TOKEN` rather than embedding the
token, so export that variable where you launch `prime-agent`.
For clients with `install-hooks` support, the capture path handles
handoff injection at session start or the client's closest equivalent, except
for Zero's no-stdout SessionStart behavior. Grok delivers on the first
`PostToolUse` instead (Antigravity uses `SessionStart`, or the first turn's
`PreInvocation` on builds without it).

#### Cline CLI and FreeBuff

Cline CLI is a first-party hooks + MCP integration:

```bash
ai-memory install-mcp --client cline --apply
ai-memory install-hooks --agent cline --apply
```

The MCP writer reads `CLINE_MCP_SETTINGS_PATH` or defaults to
`~/.cline/data/settings/cline_mcp_settings.json`; it writes top-level `type`,
`url`, and optional `headers` fields and explicitly selects `streamableHttp`
rather than Cline's legacy SSE default. The hook writer targets the CLI's `~/.cline/hooks` files;
use `--scope project` for `.cline/hooks` in one checkout. Restart Cline after
installation. Yolo mode disables Cline hooks, and task-start hooks are
detached, so recover pending handoffs through MCP. The VS Code extension's
separate `~/Documents/Cline/Hooks` convention is not modified.

FreeBuff is MCP-only:

```bash
ai-memory install-mcp --client freebuff --apply
```

This merges a sessionless stdio bridge under `mcpServers` in
`~/.agents/mcp.json`. FreeBuff finds that file while walking from the current
directory to the home directory and already reads `AGENTS.md`. A file-backed
server bearer stays in ai-memory's data directory; an explicit `--auth-token`
is stored in the stdio entry's environment map. No lifecycle hooks or managed
workstream are claimed.

#### Clients without an `install-mcp` writer

The clients below can reach ai-memory over streamable HTTP, but `install-mcp`
has no `--client` value for them, so you add the entry by hand. Each recipe
follows the client's own documentation at the version named. None of them has
been run end to end against ai-memory yet; the
[support matrix](support-matrix.md) marks them `MCP-only (documented)`.
If one does not connect as described, please open an issue with the client
version.

All of them are MCP-only here: ai-memory installs no lifecycle hooks for them,
so there is no automatic capture and no handoff injection. Ask the agent to
call `memory_handoff_begin` before you leave and `memory_handoff_accept` when
you resume. None of them sends ai-memory a session id, so they are static
clients: project-scoped calls need `workspace` and `project`, which the
routing snippet from `ai-memory install-instructions --target AGENTS.md`
tells the model.

The examples use the local server, `http://127.0.0.1:49374/mcp`. For a remote
server, change the URL and add the bearer header shown. Where the client
expands environment variables in its config, the example reads the token
from `AI_MEMORY_AUTH_TOKEN`; export it in the shell that starts the client.
Where the client documents no expansion, the example shows `<token>`, and the
token has to be written into that file, so keep the file private. All of
these clients speak HTTP MCP natively, so `ai-memory mcp-bridge` is not needed
for these hand-written recipes.

##### Warp

Warp reads `~/.warp/.mcp.json` and, after you approve it, a project's
`.warp/.mcp.json`. You can also add the server under **Settings > Agents >
MCP servers > + Add**. A URL server takes `url` and an optional `headers`
map, with no `type` key:

```json
{
  "mcpServers": {
    "ai-memory": {
      "url": "http://127.0.0.1:49374/mcp",
      "headers": { "Authorization": "Bearer <token>" }
    }
  }
}
```

Warp does not document environment-variable expansion in `headers`. Leave
out `headers` when the server has no bearer token. The separate Warp Agent
CLI keeps its own file with the same `mcpServers` format (`~/.warp_cli/.mcp.json`
on macOS; the docs give no Linux path). Documented for Warp
v0.2026.10.07.08.29.
Sources: <https://docs.warp.dev/agents/capabilities/mcp>,
<https://docs.warp.dev/agents/cli/configuration/>.

##### Kilo Code

The Kilo Code VS Code extension and the Kilo CLI share one config:
`~/.config/kilo/kilo.jsonc` (or `kilo.json`) globally, or `kilo.jsonc` /
`.kilo/kilo.jsonc` in the project. Servers live under the `mcp` key, and a
remote server has `type: "remote"`; Kilo tries streamable HTTP first and
falls back to SSE. Kilo expands `{env:NAME}` in config values:

```json
{
  "mcp": {
    "ai-memory": {
      "type": "remote",
      "url": "http://127.0.0.1:49374/mcp",
      "enabled": true,
      "oauth": false,
      "headers": { "Authorization": "Bearer {env:AI_MEMORY_AUTH_TOKEN}" }
    }
  }
}
```

ai-memory uses a bearer token, not OAuth, so `"oauth": false` stops Kilo from
starting an OAuth flow. Leave out `headers` when the server has no bearer
token. The older `mcp_settings.json` and `.kilocode/mcp.json` files are not
in the current docs. Documented for Kilo Code 7.8.8.
Sources: <https://kilo.ai/docs/automate/mcp/using-in-kilo-code>,
<https://kilo.ai/docs/automate/mcp/using-in-cli>.

##### Goose

Goose keeps extensions in `~/.config/goose/config.yaml`
(`%APPDATA%\Block\goose\config\config.yaml` on Windows). Run `goose
configure`, choose **Add Extension > Remote Extension (Streamable HTTP)**,
enter the URL, and, for a server with a bearer token, add the custom request
header `Authorization: Bearer <token>`. The resulting entry has this shape;
note that the URL key is `uri`:

```yaml
extensions:
  ai-memory:
    type: streamable_http
    name: ai-memory
    enabled: true
    uri: "http://127.0.0.1:49374/mcp"
    headers:
      Authorization: "Bearer <token>"
    timeout: 300
```

Goose does not document environment-variable expansion in `headers`. Leave
`headers` empty (`{}`) when the server has no bearer token. For one session,
`goose session --with-streamable-http-extension "http://127.0.0.1:49374/mcp"`
loads the server without saving it (that flag takes no header). Documented
for Goose 1.54.0.
Sources: <https://goose-docs.ai/docs/guides/config-files>,
<https://goose-docs.ai/docs/getting-started/using-extensions>,
<https://goose-docs.ai/docs/guides/goose-cli-commands>.

##### OpenHuman

OpenHuman (tinyhumansai/openhuman) adds servers through the **mcp.json** tab
of its MCP Servers screen. That document is a view over OpenHuman's own
server store, not a file you edit on disk. A server with `url` is dialled
over streamable HTTP, and `headers` holds its credentials:

```json
{
  "mcpServers": {
    "ai-memory": {
      "url": "http://127.0.0.1:49374/mcp",
      "headers": { "Authorization": "Bearer <token>" }
    }
  }
}
```

OpenHuman does not document environment-variable expansion in that
document. It stores header values write-only and never shows them again.
Leave out `headers` when the server has no bearer token. Documented for
OpenHuman 0.64.15.
Sources: <https://tinyhumans.gitbook.io/openhuman/features/integrations/mcp-and-skills>,
<https://tinyhumans.gitbook.io/openhuman/developing/architecture/mcp-registry>.

##### Aider

Aider has no MCP support in any release (checked against 0.86.2 and its
release history), so there is no recipe. Its feature request is
[Aider-AI/aider#3314](https://github.com/Aider-AI/aider/issues/3314).

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
`--agent claude-code|codex|command-code|cursor|gemini-cli|kimi-code|kiro-cli|antigravity-cli|grok|opencode|opencode2|openclaw|omp|oh-my-pi|pi`
and `--to <dir>`; `--help` prints the full flag list. OpenCode,
OpenClaw, OMP / Oh My Pi, and Pi do not need script extraction because
`install-hooks` generates TypeScript plugin/extension files for them
instead. For Pi, the generated extension also provides the MCP bridge.

The generated TypeScript integrations survive an unreachable server the
same way the native hooks do: a delivery that fails at the network level
(or gets a 5xx, 408, 425 or 429) is written to `<data_dir>/hook-spool/` in the exact
format `ai-memory hook-drain` reads, and the plugin drains that backlog
itself once the server is reachable again. A day of laptop work off
the server's network is therefore captured, not silently dropped. Each spooled
entry carries an idempotency key, so the plugin's own drain and a
manual `hook-drain` can race without double-ingesting. Note the spool
lands in the *agent host's* data dir (`AI_MEMORY_DATA_DIR` or the
platform default), which is where a native `ai-memory` install looks.

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
other host, `mise` installs a tagged release binary directly from GitHub,
with no Rust toolchain needed:

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
registry entry is required; the backend works against any repo whose
release assets follow this naming convention. By default mise also holds back
the very newest release for a short safety window
([`minimum_release_age`](https://mise.jdx.dev/dev-tools/github-backend.html)),
so a fresh tag may resolve to the previous version for a day or so; pin an
exact tag with `mise use -g github:akitaonrails/ai-memory@1.30.0` to bypass
that.

`cargo install ai-memory` is not available: the crate name is already taken
by an unrelated project on crates.io, as is `ai-memory-core` (the workspace's
foundational internal crate). Publishing would require renaming at least
that crate for the registry, a naming decision the project hasn't made yet.

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
`init` on a fresh data dir already requires the bearer token it wrote to
`auth-tokens/default`; `AI_MEMORY_AUTH_TOKEN` in the server's environment
overrides it.

#### Optional serve flags

The `serve` subcommand also accepts:

| Flag | Env var | What it does |
|---|---|---|
| `--enable-web` | `AI_MEMORY_ENABLE_WEB=true` | Mount the read-only web browser and `/api/v1` JSON API. |
| `--enable-api` | `AI_MEMORY_ENABLE_API=true` | Mount only the protected, read-only `/api/v1` JSON API for companions and other non-browser clients. |
| `--base-path /wiki` | `AI_MEMORY_BASE_PATH` | Host the entire HTTP surface (`/mcp`, `/hook`, `/admin/*`, `/api/v1`, `/web`) under a configurable subpath, useful behind a reverse proxy sharing a hostname. `.` and `..` segments are rejected; unsafe chars cause a fallback to root with a warning. See [`docs/https-via-proxy.md`](https-via-proxy.md#hosting-under-a-subpath). |
| `--web-slug /web` | `AI_MEMORY_WEB_SLUG` | Where the web UI mounts within the base-path. Default `/web`; set to `/` to mount the UI at the base-path root. |
| `--web-ui-dir <path>` | `AI_MEMORY_WEB_UI_DIR` | Serve a custom SPA from `<path>` instead of the built-in browser. ai-memory injects `<base href>` and `<meta name="ai-memory-base-path">` so the SPA can build relative URLs and API calls under the configured prefix. |
| `--cors-allow-origin <origin>` | `AI_MEMORY_CORS_ALLOW_ORIGINS` (CSV) | Allow listed origins to call `/api/v1`. The layer is scoped only to that route; `/mcp`, `/hook`, `/admin`, and `/web` remain origin-locked. |
| _(config only)_ | `AI_MEMORY_HOOK_RATE_PER_SEC`, `AI_MEMORY_HOOK_RATE_BURST` | Optional per-actor/session hook ingest token bucket. Unset/`0` rate disables it; burst defaults to the rate (minimum one token when enabled). |

On macOS, see [`docs/macos.md`](macos.md); use the archive matching your
architecture: `aarch64` for Apple Silicon, `x86_64` for Intel. The
[menu bar app](#macos-menu-bar-app) is the self-contained GUI path (bundles
the binary, starts the LaunchAgent, opens `/web` and status). On Windows, see
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
./target/release/ai-memory install-hooks --agent claude-code --apply
```

The hooks read the token from the data dir; pass `--auth-token` only for a
server whose token lives elsewhere.

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
| **+ Codex credential reuse** | Same LLM features using the Codex CLI-owned login without copying or owning its refresh token | `AI_MEMORY_LLM_PROVIDER=codex` + an authenticated Codex CLI | Uses your ChatGPT subscription |
| **+ GitHub Copilot** | Same LLM features using a GitHub Copilot subscription | `AI_MEMORY_LLM_PROVIDER=copilot` + `ai-memory auth login copilot` or `COPILOT_GITHUB_TOKEN` | Uses your Copilot subscription |
| **+ Cursor subscription** | Same LLM features using the logged-in Cursor Agent CLI. Read-only `--mode ask`; does not pass `--yolo` | `AI_MEMORY_LLM_PROVIDER=cursor` + `agent` on `PATH` (or `AI_MEMORY_CURSOR_AGENT`) | Uses your Cursor subscription |
| **+ LLM reranking** | At most one relevance pass over up to 30 bounded project/scopes search candidates; normal order is preserved on invalid, failed, timed-out, or concurrency-saturated responses | `AI_MEMORY_RERANKER=llm` + any configured LLM provider | One LLM call per eligible query, at most four concurrently |
| **+ Hybrid retrieval** | Adds vector cosine similarity to FTS5 + entity + graph RRF. Better recall on paraphrased queries | `AI_MEMORY_EMBEDDING_PROVIDER=openai` + `OPENAI_API_KEY` (or `EMBEDDING_API_KEY`) | ~$0.0001 / page on backfill |

### Recommended models (chosen as defaults)

If you set only the provider, ai-memory picks a sensible default:

| Setting | Default | Why |
|---|---|---|
| `AI_MEMORY_LLM_PROVIDER=anthropic` | `claude-haiku-4-5` | **Recommended default.** Best balance of speed, restraint, and classification quality. Not a reasoning model. Consistently classifies durable project rules as `kind: rule`. |
| `AI_MEMORY_LLM_PROVIDER=anthropic-oauth` | `claude-sonnet-4-6` | Anthropic via Claude subscription. Run `claude setup-token` once and export the printed token as `ANTHROPIC_OAUTH_TOKEN` (or `CLAUDE_CODE_OAUTH_TOKEN`). No `ANTHROPIC_API_KEY` needed. Same `/v1/messages` endpoint, Bearer token auth. Anthropic currently refuses these tokens from third-party apps, usually as a `429` on every request. |
| `AI_MEMORY_LLM_PROVIDER=openai` | `gpt-5.4-mini` | Cheaper + faster alternative. Same parse reliability; mild over-classification on thin sessions. |
| `AI_MEMORY_LLM_PROVIDER=openai-oauth` | `gpt-5.5` | ChatGPT/Codex backend. Run `ai-memory auth login openai-oauth` once; ai-memory stores the refresh token in `<data_dir>/auth.json` and refreshes access tokens automatically. Optional `AI_MEMORY_LLM_REASONING_EFFORT` (`none`/`minimal`/`low`/`medium`/`high`/`xhigh`/`max`/`ultra`/`persistent`) is mapped to each provider's native reasoning field; omit it to keep the model default. |
| `AI_MEMORY_LLM_PROVIDER=codex` | `gpt-5.6-luna` | Reuses only `access_token` and `account_id` from Codex's `auth.json`; token renewal is delegated to `codex app-server --stdio`. |
| `AI_MEMORY_LLM_PROVIDER=copilot` | `gpt-5.5` | GitHub Copilot Chat backend. ai-memory stores a GitHub user token in `<data_dir>/auth.json`, exchanges it for a short-lived Copilot API token, and refreshes before expiry. |
| `AI_MEMORY_LLM_PROVIDER=cursor` | `cursor-grok-4.6-high` | Cursor subscription via `agent --print --mode ask --trust`. No API key and no `--yolo`. This id is "Grok 4.6" in `agent --list-models`. Override the binary with `AI_MEMORY_CURSOR_AGENT`. Any other id from that list works as `AI_MEMORY_LLM_MODEL`. |
| `AI_MEMORY_LLM_PROVIDER=gemini` | `gemini-3.5-flash` | Google's hosted option with a generous free tier. ai-memory disables Gemini 3.5 Flash's default dynamic thinking so hidden thought tokens do not truncate strict JSON. Set `GEMINI_API_KEY` (or `GOOGLE_API_KEY`). |
| `AI_MEMORY_LLM_PROVIDER=opencode` | `mimo-v2.6-flash` | [OpenCode](https://opencode.ai) cloud API. Defaults to the **Go** endpoint, `opencode.ai/zen/go/v1`, a cost-optimised model subset. Published Responses models, including GPT-5.6 Luna and GPT-6 Luna, use `/responses`; supported Claude and Qwen Messages models use `/messages`. MiniMax M3/M2.7 and Qwen3.8 Max use Messages on Go and Chat Completions on Zen; Go Muse Contributor models use Responses. Changing the base URL reselects the transport and preserves request settings. See the [Go](https://opencode.ai/docs/go/#endpoints) and [Zen](https://opencode.ai/docs/zen/#endpoints) endpoint tables. For **Zen**'s full catalogue, set `AI_MEMORY_LLM_BASE_URL=https://opencode.ai/zen/v1` plus an `AI_MEMORY_LLM_MODEL` from it, such as `claude-sonnet-5-5`; the default model id is Go's. Requests identify ai-memory by version and reuse one session header across related attempts. Both endpoints take `OPENCODE_API_KEY` (key from `opencode.ai/auth`). Alias: `opencode-zen` (historical); it selects Go like the others, and the endpoint is chosen by the base URL, not the alias, with no automatic billing-product switch. |
| `AI_MEMORY_EMBEDDING_PROVIDER=openai` | `text-embedding-3-small` (1536-dim) | 5× cheaper than `-3-large` with marginal recall loss. |
| `AI_MEMORY_EMBEDDING_PROVIDER=openai` + `AI_MEMORY_EMBEDDING_BASE_URL=https://openrouter.ai/api/v1` | `openai/text-embedding-3-small` via [OpenRouter](https://openrouter.ai) | Uses `EMBEDDING_API_KEY`, else reuses `LLM_API_KEY` or `OPENAI_API_KEY`, with the OpenAI-compatible embedding client. |
| `AI_MEMORY_EMBEDDING_PROVIDER=openai` + `AI_MEMORY_EMBEDDING_BASE_URL=https://api.orcarouter.ai/v1` | `openai/text-embedding-3-small` via [OrcaRouter](https://www.orcarouter.ai) | Uses `EMBEDDING_API_KEY`, else reuses `LLM_API_KEY`, with the OpenAI-compatible embedding client. |
| `AI_MEMORY_EMBEDDING_PROVIDER=voyage` | `voyage-3` (1024-dim) | Voyage's current general-purpose recommendation. |
| `AI_MEMORY_EMBEDDING_PROVIDER=google` / `gemini` | `gemini-embedding-001` (768-dim) | Google-hosted embeddings via `embedContent`. Set `GEMINI_API_KEY` (or `GOOGLE_API_KEY`). |
| `AI_MEMORY_EMBEDDING_PROVIDER=openai-compat` | no default; set model, dim, and base URL explicitly | Self-hosted engines (Ollama, LM Studio, vLLM). Keyless by default; `EMBEDDING_API_KEY`, else `LLM_API_KEY`, is sent as a bearer token when present (gateways). Example: `AI_MEMORY_EMBEDDING_BASE_URL=http://localhost:11434/v1`, `AI_MEMORY_EMBEDDING_MODEL=nomic-embed-text`, `AI_MEMORY_EMBEDDING_DIM=768`. Switching an existing `openai`+base-URL setup to `openai-compat` changes the stored `{provider, model, dim}` triple, so run `ai-memory embed --force` to re-embed. Asymmetric models need `AI_MEMORY_EMBEDDING_QUERY_PREFIX` / `AI_MEMORY_EMBEDDING_DOCUMENT_PREFIX`, e.g. `nvidia/Nemotron-3-Embed-1B-BF16` wants `query: ` / `passage: `. See [`docs/llm-providers.md`](llm-providers.md) for that and for Qwen3-Embedding/instruction-tuned E5, which need a different (query-only) format. |
| `AI_MEMORY_EMBEDDING_PROVIDER=copilot` | `text-embedding-3-small` (1536-dim) | Reuses the `copilot` LLM provider's OAuth login (`ai-memory auth login copilot`, `COPILOT_GITHUB_TOKEN`, or `GITHUB_COPILOT_API_TOKEN`), with no separate API key. Calls Copilot's `/embeddings` endpoint following the OpenAI-compatible contract Copilot documents for chat; that endpoint's exact shape is not covered by a live test against Copilot here, so treat it as needing a real-Copilot smoke test. |

> **What we don't recommend:** reasoning-mode models (Claude with extended
> thinking, GPT-o3, Gemini "thinking" variants). They burn token budget on
> internal reasoning and hang or emit empty responses with the strict-JSON
> consolidation prompt. Turn reasoning off if you must use one.

#### Embedding credentials (`EMBEDDING_API_KEY`)

`EMBEDDING_API_KEY` is optional and credentials the embedding role alone. Set
it when the embedder should authenticate against a different provider than the
LLM. Otherwise the `openai` and `openai-compat` embedders borrow the chat
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
> **Unofficial and against Anthropic's usage policies. Use at your own risk.**
> Anthropic provides no public OAuth API for the Claude Pro/Max subscription;
> this reuses the `claude setup-token` credential against `/v1/messages`, which
> is **not a supported or sanctioned integration**. Anthropic's terms reserve
> subscription (Claude Code) access for interactive use, and using it as an
> automated API backend may breach those terms and **could get your account
> rate-limited, flagged, or banned**. The header recipe is also undocumented
> and can change without notice. If you want a supported path, use the
> `anthropic` provider with a real Platform API key. We ship this purely as an
> opt-in convenience and make no guarantees about it.

> [!IMPORTANT]
> **Anthropic currently refuses these tokens from third-party apps.** Its
> [legal and compliance page](https://code.claude.com/docs/en/legal-and-compliance)
> does not permit third-party developers to route requests through Free, Pro,
> or Max plan credentials, and the API rejects them server-side. With
> ai-memory this usually shows up as
> `provider error 429: {"type":"error","error":{"type":"rate_limit_error",...}}`
> on every request, including `ai-memory llm-test`, even when the same token
> works in the `claude` CLI. That is a policy refusal, not a real rate limit,
> and retrying or changing headers will not fix it. Use the `anthropic`
> provider with an Anthropic Console API key (`ANTHROPIC_API_KEY`), or another
> provider from the table above.

`anthropic-oauth` is for Claude Pro/Max subscribers who want to use their
existing subscription instead of an Anthropic Platform API key. It hits the
**same** `/v1/messages` endpoint as the `anthropic` provider; only the auth
headers differ (Bearer token + `anthropic-beta: oauth-2025-04-20`).

```bash
# Obtain a token once using the Claude Code CLI. It prints the token and
# does not save it anywhere, so copy it:
claude setup-token

# Then export it yourself (CLAUDE_CODE_OAUTH_TOKEN is accepted too):
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
> **Pick a small, fast model.** ai-memory's LLM work (session
> consolidation, lint, and explore) is summarisation/extraction, not hard
> reasoning, so a Haiku-class model is enough. It is faster, cheaper, and far easier
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
> **Leave the model at the provider default (`gpt-5.5`).** The Codex/ChatGPT
> backend behind `openai-oauth` only accepts a small server-defined set of model
> ids and rejects others, including `gpt-5-mini`, with a deterministic 400, so
> do not set `AI_MEMORY_LLM_MODEL` for this backend. Consolidation / lint /
> explore are summarisation tasks, so if the default reasoning is too heavy set
> `AI_MEMORY_LLM_REASONING_EFFORT=none` or `low` instead, so hidden thought
> tokens do not eat the JSON budget. Reserve high-effort reasoning for your
> coding agent.

### Codex credential reuse

The independent `codex` provider reads `$CODEX_HOME/auth.json`, falling back to
the platform home's `.codex/auth.json`. It materializes only
`tokens.access_token` and `tokens.account_id`, reloads them before every call,
and never copies or writes the file. On the first 401, it asks
`codex app-server --stdio` to refresh the Codex-owned credential and retries
the Responses request once.

```bash
export AI_MEMORY_LLM_PROVIDER=codex
export AI_MEMORY_LLM_MODEL=gpt-5.6-luna
export AI_MEMORY_LLM_REASONING_EFFORT=medium
ai-memory llm-test --provider codex --model gpt-5.6-luna --prompt "Reply with OK"
ai-memory llm-test --provider codex --model gpt-5.6-luna --structured --prompt "Return a short answer"
```

`AI_MEMORY_CODEX_EXECUTABLE` optionally selects another Codex binary. File
storage is supported; `auto` is supported when it resolves to the same
`auth.json`. Keyring-only and ephemeral storage are not supported. Docker is
outside the automatic setup path: both the executable and credentials must be
available inside the same container/environment.

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

`AI_MEMORY_LLM_REASONING_EFFORT` is honoured on this path: OpenRouter hosts
send `reasoning: { effort, exclude: true }`; `https://api.x.ai` (Grok)
sends Chat Completions `reasoning_effort` (clamped to `low`/`medium`/`high`/
`xhigh`, because Grok cannot disable reasoning); other compat endpoints send
OpenAI-style `reasoning_effort`. Anthropic and Anthropic-OAuth map the same
key to `output_config.effort` and adaptive/disabled thinking on models that
accept those fields (Haiku 4.5 omits them because it rejects them, while Haiku
5.5 receives them; Fable 5 / Mythos 5 / Mythos Preview / Opus 5.5 omit
`thinking: disabled` because those models reject it, and Sonnet 5.5, which
rejects it too, receives `thinking: between_tools` for `none`). `ultra` and `persistent` clamp to `max` on OpenAI-style
hosts. Gemini and Copilot ignore the key.

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

[Cheaper Inference](https://cheaperinference.com) is an OpenAI-compatible LLM
gateway that advertises models below each lab's list price. It uses
the same provider; no dedicated ai-memory provider is needed.
Pass its API key through the generic compatibility credential:

```bash
-e AI_MEMORY_LLM_PROVIDER=openai-compat
-e AI_MEMORY_LLM_BASE_URL=https://api.cheaperinference.com/v1
-e AI_MEMORY_LLM_MODEL=gpt-5.4-mini
-e LLM_API_KEY=ci_live_...
```

Model ids are bare, without a `provider/` prefix. Replace the model with
another current Cheaper Inference model id (e.g. `claude-haiku-4.5` or
`deepseek-v4-flash`) when needed. Cheaper Inference serves chat models only
and has no embeddings endpoint; configure embeddings separately.

[API Route](https://www.api-route.com/) also uses the existing
`openai-compat` provider. Supply an API Route key and a model ID from its
[current catalog](https://www.api-route.com/pricing):

```bash
-e AI_MEMORY_LLM_PROVIDER=openai-compat
-e AI_MEMORY_LLM_BASE_URL=https://global.api-route.com/v1
-e AI_MEMORY_LLM_MODEL=gpt-5.5
-e LLM_API_KEY="$API_ROUTE_API_KEY"
```

Replace `gpt-5.5` with the exact model ID you intend to use. As with other
hosted compatibility endpoints, no dedicated ai-memory provider is required.
Configure embeddings separately if your chosen API Route model does not
provide an OpenAI-compatible embeddings endpoint.

[FutureInfra](https://futureinfra.ai/ai/) is an OpenAI-compatible AI API
router and uses the same provider; no dedicated ai-memory provider is needed.
Its OpenAI-compatible base is `https://futureinfra.ai/v1/ai`. That path does
not end in a version segment, so pass the full Chat Completions URL (ai-memory
uses a base URL that already ends in `/chat/completions` as-is). Pass its API
key through the generic compatibility credential:

```bash
-e AI_MEMORY_LLM_PROVIDER=openai-compat
-e AI_MEMORY_LLM_BASE_URL=https://futureinfra.ai/v1/ai/chat/completions
-e AI_MEMORY_LLM_MODEL=openai/gpt-4o-mini
-e LLM_API_KEY="$FUTUREINFRA_API_KEY"
```

Model ids use the `provider/model` format, e.g. `anthropic/claude-sonnet-4` or
`deepseek/deepseek-chat`; `GET https://futureinfra.ai/v1/ai/models` lists the
current ids. Keys are created in the
[FutureInfra console](https://futureinfra.ai/console/?screen=ai-router). This
example configures the LLM only; configure embeddings separately.

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

#### Send a caller-identifying header to a gateway that requires one

Every chat request already carries `User-Agent: ai-memory/<version>`, so a
gateway can tell what is calling it. Some also require a header of their own
for request correlation, and reject or throttle traffic without it. Declare
those once; they are sent on every chat request, whatever the provider:

```bash
-e AI_MEMORY_LLM_HEADERS=x-opencode-session=prod-01,x-opencode-client=ai-memory
```

Entries are `Name=Value` or `Name: Value`, comma separated. A header *value*
cannot contain a comma through the env var; use `llm_headers = [...]` in
`config.toml` when one must. Headers ai-memory sets itself (`authorization`,
`content-type`, `x-api-key`, `x-goog-api-key`, `anthropic-version`,
`anthropic-beta`, `openai-beta`, `host`, `content-length`) are refused at
startup rather than duplicated onto the request. An entry for `user-agent`
overrides the default. Values are never logged.

The `opencode` provider needs no configuration for this: OpenCode asks
callers for `x-opencode-session`, and that provider already sends one id per
logical operation. The id is stable across retries and the structured-output
fallback, so one consolidation pass reads as one operation in OpenCode's
metrics. Supply the header through `AI_MEMORY_LLM_HEADERS` to override that,
for instance to tell several ai-memory instances apart under one account.

#### Match the consolidation budget to a local model's context window

Consolidation defaults to an approximate 100k-token input target plus a 32k
output limit, sized for a 200k-context provider. A local model with a smaller
window can reject the whole request (`exceed_context_size_error` from
llama.cpp, HTTP 400 from most gateways). Lower both limits so their sum fits
the real context window, with additional headroom for tokenizer variance:

```bash
# e.g. a model loaded with a 12k context window
-e AI_MEMORY_CONSOLIDATION__MAX_INPUT_TOKENS=10000
-e AI_MEMORY_CONSOLIDATION__MAX_OUTPUT_TOKENS=1000
```

An 8k window cannot hold the default floor (8,350 input tokens at the 0.8
margin). It fits with `AI_MEMORY_CONSOLIDATION__INPUT_TOKEN_SAFETY_MARGIN=1.0`
and about 6,700 input tokens plus 1,000 output, at the cost of the headroom the
margin keeps for dense text such as Portuguese or code (#884).

The double underscore separates the `[consolidation]` section from each key.
The input target accounts for the rendered observations, current page body,
system prompt, page conventions, bounded slot snapshots, structured-output
schema, and provider-envelope reserve. Tokenizers differ, so this is a
conservative estimate rather than an exact provider token count. An automatic
checkpoint provider failure degrades to a rule-based page rather than losing
the checkpoint, but right-sized limits are what allow LLM consolidation to
succeed. Startup rejects a budget that leaves too little room for observations
after the fixed prompts and the structured-output schema -- 8,350 input tokens
at the default `input_token_safety_margin` of 0.8 -- and rejects output limits
below 1,000, because the batch schema and a useful response cannot fit reliably
below those floors. Lowering the safety margin shrinks the character budget, so
a smaller margin needs a proportionally larger `max_input_tokens`.

Long sessions can omit observations or truncate an individual body before the
request reaches the model. To add a bounded set of literal error lines, paths,
URLs, status codes and versions from that missing text, opt in with:

```toml
[consolidation]
verbatim_residue = true
```

or `AI_MEMORY_CONSOLIDATION__VERBATIM_RESIDUE=true`. The default is false.
Enabling it widens the subset of already-sanitized captured text sent to the
configured LLM provider; it does not append anything directly to a wiki page.
The block is capped, marked as untrusted observation data, and the provider's
reply still passes through the existing structured-output and wiki-write
boundaries.

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
| `setup [--yes] [--agent <name>]... [--no-service]` | native binary (after `install.sh`) | `init` a fresh data dir (with its root bearer token), install and start a per-user service, wire MCP, hooks and Agent Skills for each named or detected agent, then run a read-only self-check. Shows the plan and asks first; a second run changes nothing. |
| `auth enable [--no-mcp]` | native binary or host wrapper | Give an existing install without auth a root bearer token in `auth-tokens/default` and re-render its hook and MCP wiring to read it; `--no-mcp` leaves the MCP registrations alone. Idempotent. |
| `project init` | native binary, in the checkout | Ask for `workspace` and `project` and write them to a new `.ai-memory.toml` marker at the worktree root; the defaults keep the scope the checkout already resolves to, and an existing marker is never overwritten. Needs a terminal. |
| `recover [--json]` | host wrapper or native binary | After a server outage, drain the local hook spool, replay degraded-run transcripts through the backfill path, and finish managed runs whose transcript import failed. Safe to re-run; `--json` prints the report as JSON. |
| `run [harness] [args...]` | host wrapper or native binary | Opt into one managed cross-harness workstream; omit the harness to resume the newest usable local session, or name Claude Code, Codex, OpenCode, Pi, Crush, Kimi Code, Command Code, Kiro CLI v2/v3, OMP, Grok Build CLI, Antigravity CLI, or GitHub Copilot CLI explicitly; exact `--yolo` and `--fresh` flags are wrapper-owned and other native arguments pass through |
| `show [--json]` | host wrapper or native binary | Choose a client-local checkout and installed managed harness, or return structured discovery data without launching; remote servers never provide checkout paths |
| `continue [--workspace NAME]` | host wrapper or native binary | From any directory, revalidate and resume the newest client-local managed checkout; accepts `--yolo` and `--fresh` but no harness-native arguments |
| `resume [--all] [--workspace NAME] [--search TERM] [--limit N]` | host wrapper or native binary | Interactively choose from all workstreams in the current checkout (no default cutoff), or with `--all` from every valid client-local linked checkout; type to search names, Up/Down selects, Left/Right cycles `auto` plus installed harnesses, Enter launches, and Escape clears the search or cancels; an explicit limit caps initial search results; accepts `--yolo` and `--fresh` |
| `workstreams [--workspace NAME] [--project NAME] [--limit N] [--json]` | host wrapper or native binary | List recent workstreams selectable from the current checkout, including current selection and linked harnesses, without exposing paths or native session ids |
| `rename-workstream (--from NAME \| --workstream-id ID) --to NAME [--workspace NAME] [--project NAME] [--json]` | host wrapper or native binary | Retitle one workstream selectable from the current checkout; metadata only, so the stable id, the ledger, the listing order, and the current selection are unaffected |
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
| `user add-human` / `list` / `reset-password` / `disable` / `enable` / `patch` | `docker exec` or native binary | Human identities and temporary passwords; never issues an API key |
| `api-key add` / `list` / `rotate` / `revoke` | `docker exec` or native binary | Native `aim_` machine credentials, plus the `legacy-user-token` keys left by the 1.x `user add` (removed in 3.0) |
| `auth login openai-oauth` | same data volume as the server | Store a ChatGPT/Codex OAuth refresh token for the optional `openai-oauth` LLM provider |
| `auth login copilot` | same data volume as the server | Store a GitHub token for the optional `copilot` LLM provider |
| `auth login oidc-device` | same developer data dir as native hooks and thin-client CLI commands | Store a per-developer OIDC device token for native hook authentication and HTTP CLI fallback auth |
| `install-mcp --client` | `docker run --rm` | MCP-config snippet per client |
| `install-hooks --agent [--scope project]` | `docker run --rm` | Hook-config snippet for an existing hooks dir; `--scope project` targets the checkout's `.claude/settings.local.json` (Claude Code only) |
| `setup-agent --agent --to --host-prefix` | `docker run --rm -v` | Extract bundled scripts + print config (one-shot) |
| `install-instructions [--target] [--print] [--no-skills]` | same host environment used for the agent prompt files | Install or update the slim CLAUDE.md / AGENTS.md routing block and, by default, the managed ai-memory Agent Skills |
| `install-skills [--scope] [--agent]` | same host environment used for the agent skill dirs | Install or update only the managed ai-memory Agent Skills |
| `uninstall --apply` | same host environment used for install | Remove only ai-memory-owned hooks, MCP entries, instruction blocks, managed skill files, and generated plugin files after content/marker validation, plus the user service `setup` installed when nobody edited it since. Removing hooks or MCP also deletes the `ai-memory run` auto-wire sentinels under `<data_dir>/autowire-state/`, so the next managed launch wires again. Use `--mcp-url` for custom MCP endpoints and `--mcp-name` only to narrow removal. |
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
| `--skills-agent <agent>` | Choose `.claude/skills`, `.agents/skills`, `.devin/skills`, `.grok/skills`, `.hermes/skills`, GitHub Copilot CLI's `.github/skills`, Google Antigravity's roots, or both Claude/Agents roots. Values: `claude-code`, `agents`, `devin`, `grok`, `copilot-cli`, `hermes`, `antigravity`, `both`. By default, `CLAUDE.md` targets imply `claude-code`, `AGENTS.md` targets imply `agents`, and both instruction files imply `both`. |
| `--skills-target-dir <dir>` | Write managed skill directories below an explicit root instead of inferring from scope and agent. |
| `--skills-force` | Replace unmanaged same-name skills during `install-instructions`; without it, they are left untouched and the command exits with an actionable error. |

Use `install-skills` when the instruction block is already right and only the
Agent Skill files need a refresh:

```bash
ai-memory install-skills
ai-memory install-skills --scope global --agent agents
ai-memory install-skills --scope global --agent devin
ai-memory install-skills --scope global --agent grok
ai-memory install-skills --scope global --agent copilot-cli
ai-memory install-skills --scope global --agent hermes
ai-memory install-skills --scope global --agent antigravity
ai-memory install-skills --agent both --print
ai-memory install-skills --target-dir .custom/skills --force
```

`install-skills` flags:

| Flag | Meaning |
|---|---|
| `--scope <scope>` | Install into this project or the current user's global skill roots. Values: `project`, `global`. Defaults to `project`. |
| `--agent <agent>` | Install into Claude Code's skill root, the cross-agent skill root, Devin's skill root, Grok's skill root, Hermes Agent's skill root, GitHub Copilot CLI's skill root, Google Antigravity's skill roots, or both Claude/Agents roots. Values: `claude-code`, `agents`, `devin`, `grok`, `copilot-cli`, `hermes`, `antigravity` (aliases `antigravity-cli`, `agy`), `both`. Defaults to `claude-code`. |
| `--target-dir <dir>` | Write managed skill directories below an explicit root; `--scope` and `--agent` are ignored. |
| `--print` | Print target paths and `SKILL.md` contents without writing files. A global Grok plan also prints `$GROK_HOME/rules/ai-memory-handoff.md`. |
| `--force` | Replace unmanaged same-name skills (and, for global Grok, an unmanaged first-turn rule); without it, user-authored same-name files are preserved. |

Default skill target roots:

| Scope | `--agent claude-code` | `--agent agents` | `--agent devin` | `--agent grok` | `--agent copilot-cli` | `--agent hermes` | `--agent antigravity` |
|---|---|---|---|---|---|---|---|
| `project` | `.claude/skills` | `.agents/skills` | `.devin/skills` | `.grok/skills` | `.github/skills` | `.hermes/skills` | `.agents/skills` |
| `global` | `~/.claude/skills` | `~/.agents/skills` | Windows: `%APPDATA%\devin\skills`; non-Windows: `~/.devin/skills` | `$GROK_HOME/skills` (default `~/.grok/skills`), plus the first-turn rule `$GROK_HOME/rules/ai-memory-handoff.md` | `$COPILOT_HOME/skills` (default `~/.copilot/skills`) | `~/.hermes/skills` | both `~/.gemini/config/skills` (Antigravity App and IDE) and `~/.gemini/antigravity-cli/skills` (Antigravity CLI) |

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

`reconcile_tombstones_deleted_pages` (default `false`, also under
`[maintenance]`) is a separate, experimental opt-in: it lets the watcher's
own 30s reconcile pass (not the scheduled jobs above) tombstone an
OKF-imported content page (session summary pages are excluded; see
`docs/okf.md`) whose file has disappeared from disk, after it has been
missing on two consecutive passes and survived a circuit breaker that
refuses to act when more than `max(3, 50%)` of a scope's candidate pages
look missing at once, or when a walk finds nothing at all. The tombstone is
soft (`is_latest = 0` + `superseded_at`, the same shape decay eviction uses)
and is picked up by the same aged-tombstone hard-delete sweep; it is not
exempt from it. What actually protects it: a reconcile tombstone is never
itself destroyed while its chain has no successor; if the file returns, the
new version re-links to the tombstoned chain instead of starting fresh, so
nothing is orphaned for that sweep to destroy. It never runs the blocking
admission gate (nothing can refuse it), but does fire-and-forget any
non-blocking observer/mirror webhook. With it off (the default), reconcile's
behavior is unchanged: a deleted file still requires `ai-memory delete-page`.
See
`docs/okf.md` for the full design.

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
--chunk-input-tokens N     (default: 24000; per LLM call, each asking for up
                            to 16K output tokens; 0 = single call asking for
                            up to 64K, so input + 64K must fit the context)
--since "30 days ago"      (git log filter; supports "N days/months/years ago" + YYYY-MM-DD)
--exclude-git              (skip commit history)
--exclude-readme           (skip README)
--exclude-docs             (skip docs/**/*.md)
--exclude-code             (skip Rust module headers)
--dry-run                  (collect + estimate but don't call LLM or write)
--force                    (re-bootstrap, overwrites the prior manifest)
```

**Budgets and the estimate.** Bootstrap estimates tokens as bytes ÷ 4, which
undercounts non-English text and code, so it fills only 80% of
`--max-input-tokens` and of each `--chunk-input-tokens` by that estimate. The
dry run still reports tokens by the same estimate; the sources it keeps and the
chunks it plans reflect the margin.

**Cost.** With Kimi 2.6 via OpenRouter ($0.73/$3.49 per M):
- 50k input tokens cap → ~$0.04 worst case input
- 1-2k generated tokens → ~$0.007 output
- Total: well under $0.20 per run.

**Idempotency.** The first run produces a per-project `bootstrap.md`
manifest (at `<wiki>/<workspace>/<project>/bootstrap.md`) listing every
page generated + a one-paragraph rationale. Re-running without `--force`
errors out. Delete the manifest (and the generated pages) if you want a
clean re-bootstrap. If canonical scope resolution promotes a legacy project but
its `_meta.md` refresh/checkpoint fails, the result includes `manifest_warning`
and the CLI prints it; the name change is committed and startup backfill can
repair the scope manifest.

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
writable (sandboxes like [ai-jail](https://github.com/akitaonrails/ai-jail)
mount `$HOME` read-only or as throwaway tmpfs), ai-memory degrades instead
of failing: it falls back to the OS temp dir, then to stderr-only logging,
printing the exact path that failed at each step. Commands keep working
either way. To keep durable file logs (and durable hook spooling) inside a
sandbox, map the data dir read-write, e.g. `ai-jail --rw-map
~/.local/share/ai-memory …`.

## Human password bootstrap and recovery

Human console login is username/password, not a Bearer pasted into the
browser. Machine APIs keep using `Authorization: Bearer` (`AI_MEMORY_AUTH_TOKEN`
or a native `aim_` key). Before human auth activates, deprecated GET-only
browser compatibility may accept the root bearer through HTTP Basic (no cookie
is set; an `ai_memory_auth` cookie an older build set is still accepted);
activation disables that path immediately.
The active classes stay isolated: a password only issues a session; a session
never authenticates `/mcp`/hooks/workstreams; recovery never issues a session;
an API key never logs into `/auth/login`.

### First-time root (greenfield)

Set a one-shot password, then start `serve`. The engine creates the configured
`root_username` (default `root`) with `must_change_password=true` and marks
bootstrap complete. Later restarts ignore the value even if it stays in the
environment, so unset it to keep the plaintext out of the process env.

```bash
# At least 12 characters. Must not equal the root bearer, actor-proxy bearer,
# or recovery token, and must not use an `ams_`/`aim_`/`amk_` prefix.
export AI_MEMORY_AUTH__INITIAL_ROOT_PASSWORD='choose-a-long-password'
docker compose up -d   # or: ai-memory serve
unset AI_MEMORY_AUTH__INITIAL_ROOT_PASSWORD
```

The first login must change that password (`POST /auth/password` with CSRF).
If the password collides with an existing native API-credential hash, startup
fails closed and bootstrap is not marked complete.

### Break-glass recovery

If the last human root is lost, set a high-entropy recovery secret (at least
32 characters) and `POST /auth/recovery` with a new password. Success returns
204, expires the legacy `ai_memory_auth` cookie, revokes that user's sessions,
and does **not** set `ai_memory_session`. Sign in again with the new password.

```bash
export AI_MEMORY_AUTH__RECOVERY_TOKEN='at-least-32-characters-of-entropy-here'
# restart serve so the process picks up the new value
curl -sS -D - -o /dev/null -X POST http://127.0.0.1:49374/auth/recovery \
  -H 'content-type: application/json' \
  -d '{"recovery_token":"'"$AI_MEMORY_AUTH__RECOVERY_TOKEN"'","new_password":"brand-new-pass!!","new_password_confirmation":"brand-new-pass!!"}'
```

Rotate by changing the env var and restarting; the previous token stops
working immediately. Public failures (wrong token, recovery unset, password
policy) share one 401 body. The recovery token is not a Bearer, cookie, or
login password.

When human mode is on (bootstrap completed, any password hash, or
initial/recovery secrets configured), `serve` refuses to start unless a
recoverable root exists (`role=root` with a password and not disabled) or
recovery is configured. Explicit machine-only remote deploys with
`AI_MEMORY_AUTH_TOKEN` and no human secrets remain valid. Loopback with
neither Bearer nor human auth stays anonymous.

CIDRs in `AI_MEMORY_AUTH__TRUSTED_PROXY_CIDRS` may supply `X-Forwarded-For`
for login rate limits; untrusted peers' XFF is ignored. Behind HTTPS, set
`AI_MEMORY_AUTH__SECURE_COOKIE=true` as noted above.

An operator who intentionally accepts unencrypted human authentication on a
private LAN can instead pass `serve --allow-insecure-human-auth` (or set
`AI_MEMORY_ALLOW_INSECURE_HUMAN_AUTH=true`, including in Docker). The option is
off by default and does not make plaintext transport safe: passwords, Bearer
and API credentials, session cookies, and memory data can all be intercepted
or modified. It bypasses only the non-loopback human-cookie posture check; a
server without authentication still needs the separate
`--allow-insecure-no-auth` override.

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

A container started this way has no token, since it never ran `init`. To
require one, run `ai-memory auth enable` through the wrapper (it writes the
token into the volume), `docker restart ai-memory`, then re-run
`ai-memory install-hooks --agent <agent> --apply` so the host hooks get it.
`install-hooks` copies the token from the volume to an owner-only file under
`~/.local/share/ai-memory/native-runner/`, and the commands the wrapper runs
natively (`install-hooks`, `mcp-bridge`, `run`) read it from there, never from
a command line. When `install-hooks` finds no token in the volume any more (auth
turned off, or the token file gone), it deletes that copy; a container engine
that cannot be reached leaves it alone. A full `ai-memory uninstall --apply`
through the wrapper deletes it once the uninstall succeeds; a dry run or an
`--only` subset keeps it.

Then wire up the agent CLI. Both commands default to
`http://127.0.0.1:49374` - no extra flags needed for the local case:

```bash
ai-memory install-mcp   --client claude-code --apply
ai-memory install-hooks --agent  claude-code --apply
```

The installed Docker wrapper runs CLI commands inside a short-lived
helper container for server/store operations, but runs `install-hooks`,
`mcp-bridge`, and lifecycle `hook` commands through the stable
checksum-verified native client under
`~/.local/share/ai-memory/native-runner/`. This keeps hook command paths valid
on the host and enforces capture exclusions before spooling. A hook command
that calls the wrapper (`ai-memory hook --event …`) therefore runs natively in
a few milliseconds and posts to the host's `127.0.0.1:49374`; it never starts a
container. A hook only reuses the installed client: `install-hooks` and
`mcp-bridge` install and refresh it, while the hook path never checks for an
update or downloads. If no client is installed yet, the wrapper says to run
`ai-memory install-hooks` once, drops that one event, and exits 0 so the agent
is not blocked. For local loopback
servers, the wrapper automatically bridges
that helper back to the host's `127.0.0.1:49374`, so `ai-memory status`,
`ai-memory search`, `ai-memory recent`, and `ai-memory bootstrap` work with the same default
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
misleading: it degrades silently to `no .git found at /work; bootstrapping
from README/docs/rules only` before dying with `Permission denied (os error
13)`.

The two engines report these facts under different keys. Docker answers
`docker info --format '{{.SecurityOptions}}'`; podman has no such field and
fails that template, so the wrapper falls back to podman's
`{{.Host.Security.Rootless}}` and `{{.Host.Security.SELinuxEnabled}}` when the
Docker probe comes back empty. Rootless engines additionally need `-u 0:0`,
because only container UID 0 maps back to the invoking host user. On rootless
podman with SELinux enforcing, both adjustments are required and neither alone
lets the write land.

Do not add `:z` or `:Z` to the wrapper's whole `$HOME` bind. Docker's
[bind-mount documentation](https://docs.docker.com/engine/storage/bind-mounts/#configure-the-selinux-label)
warns that relabeling system directories such as `/home` can make the host
inoperable. Docker documents `label=disable` in the
[`docker run` security options](https://docs.docker.com/reference/cli/docker/container/run/#security-opt).

`ai-memory run`, `ai-memory show`, `ai-memory continue`, `ai-memory resume`,
`ai-memory workstreams`, and `ai-memory rename-workstream` are exceptions too:
the current wrapper intercepts them and starts a cached checksum-verified native
client on the host, where local checkouts, harness executables, and session
stores exist. It preserves an explicit remote `AI_MEMORY_SERVER_URL`. If one of
these commands logs `data_dir=/data`, cannot find a checkout, or cannot find
`codex`, `claude`, or another host executable, refresh the stale wrapper with
`ai-memory upgrade` on that client machine.

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

### Docker wrapper

The wrapper checks Docker Hub at most once every 24 hours and prints a
one-line warning when a newer image is available. Upgrade with:

```bash
ai-memory upgrade
```

The command downloads the wrapper and its SHA-256 checksum from the latest
GitHub Release, refuses an unverified update, pulls the latest Docker
image, refreshes the checksum-verified native host client, rewrites previously
staged hook registrations to that native command when supported, and
prints how to restart the server container so the new binary is used.
Re-running `install-hooks --apply` remains idempotent: ai-memory
replaces only the hook entries it owns and leaves unrelated hooks alone.
When a Compose file is found, the wrapper first verifies that its project owns
the running `ai-memory` container. A standalone container is never handed to an
unrelated Compose project just because its file occupies a conventional path;
the wrapper instead writes the inspected standalone recreation script for
review, preserving the existing `/data` mount and other runtime options.

The macOS menu bar app is not covered by `ai-memory upgrade`. Rebuild with
`./companions/ai-memory-macos/build.sh` (or replace `/Applications/AI Memory.app`
with a newer staged bundle). Wiki, SQLite, config, and models stay in
`~/Library/Application Support/ai-memory`.

Set `AI_MEMORY_NO_VERSION_CHECK=1` to silence the daily check. To pin wrapper
self-upgrades to a fork or tagged release, set `AI_MEMORY_WRAPPER_URL=<url>`;
the wrapper requires `<url>.sha256` unless
`AI_MEMORY_WRAPPER_SHA256_URL=<checksum-url>` is also set.

### Native release binary (Linux / macOS / Windows x86_64)

When `PATH` points at a GitHub-release `ai-memory` binary under a writable
user prefix (for example `~/.local/bin`, or `%LOCALAPPDATA%\ai-memory` on
Windows), the same command upgrades the binary itself:

```bash
ai-memory upgrade
# optional: pin a tag, or force a re-download of the current tag
ai-memory upgrade --version v2.3.2
ai-memory upgrade --force
```

The native path downloads the matching release archive
(`ai-memory-<os>-<arch>.tar.gz` on Unix, `ai-memory-windows-x86_64.zip` on
Windows) and its `.sha256` sidecar from GitHub Releases, verifies the
checksum, replaces the on-disk binary (and a sibling `hooks/` directory when
present), then re-stages hooks for agents already under the data-dir hooks
tree. The archive's packaged service files, default config, and documentation
are checked against the release workflow's exact layout but are not extracted
during self-upgrade. Archives are capped at 4,096 entries and 512 MiB expanded,
in addition to the 128 MiB response-body cap. Windows uses rename-aside
(`.exe` → `.old`, then promote `.new`) because
a running image cannot be overwritten in place. It refuses Homebrew/AUR/`/usr`
installs (use the package manager), unwritable prefixes (for example Program
Files; download the zip manually), and in-container binaries (upgrade the
host wrapper/image instead). Windows aarch64 has no release asset yet. For
mirrors or hermetic tests, set `AI_MEMORY_RELEASE_BASE_URL` (or
`release_base_url` in config.toml) to a Releases-compatible base that serves
`{base}/latest/tag` and `{base}/download/<tag>/<asset>` (+ `.sha256`). That
override is a trust boundary: archive and checksum are fetched from the same
base, so the `.sha256` only proves the base served a consistent pair, not that
the binary is genuine. Point it only at origins you control. Prefer an
`https://` base; a plain-`http://` base has no transit protection, so an
on-path attacker can substitute both the archive and its matching checksum.
Each response body is capped at 128 MiB.

### Shared notes

When the upgraded server starts, it applies SQLite schema migrations and
pending wiki-structure migrations automatically. No manual database
reset or wiki rewrite is required for normal upgrades. Migrations are
forward-only: after a newer version has applied its schema, an older binary
will refuse to open that data dir (it fails closed rather than risk
corruption), so take a `ai-memory backup` before upgrading if you might need
to roll back to the previous version.

If startup instead reports that the **migration history has a gap**, an
earlier migration is missing even though a later one is recorded. This can
occur with a development build whose migration sequence differed from the
released sequence. Keep a backup and report the missing migration and the
build used to create the store for review; startup does not automatically
repair the history or apply the skipped migration.

### Upgrading to 3.0

3.0 removes the 1.x `user add`, `user expire`, `user revive` and
`user rotate-token` commands; tokens they issued keep working as API keys.
[`upgrading-to-3.0.md`](upgrading-to-3.0.md) covers what changed, the upgrade
steps and how to roll back.

### Upgrading to 2.3.0

2.3.0 is a normal forward upgrade (the only new migration, `V64`, just adds the
cross-project `agent_messages` table; nothing existing is altered or removed).
Two capture/UX conveniences are **on by default**; both are additive and
non-destructive, but worth knowing about for your first session after upgrading:

- **First `ai-memory run <harness>` auto-installs that harness's hooks + MCP** if
  they were not already wired (idempotent, one-time per harness; it preserves
  your existing hook config, including a `--capture-assistant` opt-in). Disable
  with `ai-memory run --no-autowire` or `AI_MEMORY_RUN_AUTOWIRE=false`.
- **The first session in a brand-new (empty) project imports that project's
  existing local harness history once**, so installing ai-memory mid-project is
  not amnesiac. It only ever runs on an empty project (never touches one that
  already has captured memory) and is hard-capped. Disable with
  `AI_MEMORY_BACKFILL_ON_START=false`; run it by hand with `ai-memory backfill`.

If the server runs on another host, `ai-memory upgrade` refreshes only
the local client (wrapper/image or native binary) and local hook scripts.
Redeploy the remote server separately with `bin/deploy` or
`docker compose pull && docker compose up -d` in that deploy directory.

Inside ai-jail or another bwrap sandbox, the wrapper is usable from the
sandbox, but run `install-*` commands outside the sandbox because they
write to `~/.local/share/ai-memory/hooks/`.

---

## See also

- [`docs/macos.md`](macos.md) - macOS install paths: menu bar app, native
  release tarball, source build, Docker wrapper, and launchd
- [`docs/deploy.md`](deploy.md) - homelab deploy walkthrough
  (`bin/deploy`, cloudflared TLS, env-file management)
- [`docs/usage.md`](usage.md) - handoffs, proactive querying, web UI, slim
  routing snippet + managed Agent Skills, migration from other memory tools, and raw-wiki inspection
- [`docs/mcp-install.md`](mcp-install.md) - per-client MCP config reference for
  every client in the [README Support Matrix](../README.md#support-matrix)
- [`docs/ARCHITECTURE.md`](ARCHITECTURE.md) - what's actually
  running inside ai-memory
