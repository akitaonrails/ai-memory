# macOS Support

macOS is a supported platform: the workspace test suite runs on macOS CI and
tagged releases publish native `ai-memory-macos-aarch64.tar.gz` (Apple Silicon)
and `ai-memory-macos-x86_64.tar.gz` (Intel) binaries.

On macOS the **native binary** (a prebuilt release or a source build) is the
recommended way to run ai-memory. It binds the server on `127.0.0.1:49374`, and
both the MCP endpoint and the lifecycle hooks talk to that loopback address —
which the native agent can reach and which is already in the default Host-header
allowlist. The Docker wrapper is also supported when you prefer a containerised
server.

Unlike Windows there is only one "path world" on macOS: POSIX paths and POSIX
`.sh` hooks throughout. There is no WSL-vs-native split to get wrong.

## Rule Of Thumb

Run `install-mcp` / `install-hooks` from the same shell that launches Claude
Code, Codex, Cursor, Gemini CLI, or another agent — on macOS that is just your
normal Terminal.

- The agent runs as a native macOS process, so its config must point at a
  **host-reachable** server URL. Native installs and Docker-wrapper
  `install-mcp` / `install-hooks` commands render `http://127.0.0.1:49374`,
  which works from the host agent.
- Hooks are rendered for one of two platforms:
  - `posix-native` — a direct `ai-memory hook --event …` call. The default for
    native macOS/Linux Claude Code installs (cargo / release binary); it uses
    the local event spool + OIDC-token fallback.
  - `posix` — `sh` runs the bundled `.sh` script. The Docker wrapper's default.

  Set `AI_MEMORY_HOOK_PLATFORM` before wiring hooks to override the default.

## Scenario A: Prebuilt Release Binary (Recommended, No Toolchain)

Use this when you want a local server plus native hooks without a Rust toolchain
or Docker. Each tagged release publishes a macOS tarball per architecture.

```bash
# 1. Download the archive for your chip and extract it to a stable location.
#    aarch64 = Apple Silicon (M-series); x86_64 = Intel.
mkdir -p ~/Applications/ai-memory && cd ~/Applications/ai-memory
curl -fsSL -O https://github.com/akitaonrails/ai-memory/releases/latest/download/ai-memory-macos-aarch64.tar.gz
tar -xzf ai-memory-macos-aarch64.tar.gz
# `curl` downloads are not Gatekeeper-quarantined, so the binary runs as-is.
# If you downloaded via a browser instead, clear the quarantine flag once:
#   xattr -d com.apple.quarantine ./ai-memory

# 2. Initialise the data dir (defaults to
#    ~/Library/Application Support/ai-memory; override with AI_MEMORY_DATA_DIR).
./ai-memory init

# 3. Start the server (loopback only).
./ai-memory serve --transport http --bind 127.0.0.1:49374
```

> **The server from step 3 must stay running for every other command.**
> `ai-memory init` only creates the data dir — it does **not** start a
> server. `bootstrap`, `install-hooks`, `install-mcp`, and `status` are all
> clients that talk to the running server over HTTP, so running them while
> nothing is serving fails with `Connection refused (os error 61)` /
> `could not reach http://localhost:49374`. Leave `serve` running in its own
> terminal (or set it up as a login service, below), then run the rest in a
> second terminal.

In a second terminal, wire the agent:

```bash
cd ~/Applications/ai-memory
# `install-hooks` auto-discovers the bundled hooks/ directory beside the binary.
./ai-memory install-hooks --agent claude-code --apply
./ai-memory install-mcp --client claude-code --apply
```

Optionally, once hooks are wired, put the binary on `PATH` so later commands
(`ai-memory status`, a fresh terminal tab, the checklist below) don't need
`cd`/`./`:

```bash
sudo ln -sf ~/Applications/ai-memory/ai-memory /usr/local/bin/ai-memory
```

As of v1.39.0, running `install-hooks` through the symlink works: hook
discovery canonicalises the running binary's path before walking up to
the sibling `hooks/` directory
([#546](https://github.com/akitaonrails/ai-memory/issues/546), fixed in
v1.39.0). **On v1.38.x or older**, the walk did not resolve through a
symlink — running `install-hooks` via `/usr/local/bin/ai-memory` sent
discovery to the wrong parent directories, failing outright on a clean
machine:

```
Error: could not locate hooks directory. Tried: ["/…/hooks/claude-code",
"/usr/local/share/ai-memory/hooks/claude-code", "/usr/share/ai-memory/hooks/claude-code",
"…/Library/Application Support/ai-memory/hooks/claude-code"]
```

— or, worse, silently wiring a stale hooks cache from
`~/Library/Application Support/ai-memory` on a machine with an earlier
install. If you are on an older release, run `install-hooks` via the
extracted `./ai-memory` path (or upgrade).

Notes:

- The MCP endpoint, capture hooks, and `ai-memory status` work without a token
  in this single-user loopback setup. If you explicitly configure
  `AI_MEMORY_AUTH_TOKEN` for the server, pass the same token with `--auth-token`
  or export it for CLI commands.
- Keep the extracted `ai-memory` at a stable path; the hook commands (and the
  symlink, if you made one) reference it. Re-run `install-hooks` and re-point
  the symlink if you move it.

## Scenario B: Source Build

Use this when developing ai-memory itself. Requires Rust 1.95
(`rust-toolchain.toml`) plus the Xcode Command Line Tools
(`xcode-select --install`); SQLite is bundled and libgit2 is vendored, so no
extra system libraries are needed.

```bash
git clone https://github.com/akitaonrails/ai-memory
cd ai-memory
cargo build --release --workspace
./target/release/ai-memory init
./target/release/ai-memory serve --transport http --bind 127.0.0.1:49374
```

From another shell in the repo, `install-hooks` finds the bundled `hooks/`
automatically (no `--source` needed from the repo root):

```bash
./target/release/ai-memory install-hooks --agent claude-code --apply
./target/release/ai-memory install-mcp   --client claude-code --apply
```

If you symlink the built binary onto `PATH` for convenience (e.g.
`ln -sf "$(pwd)/target/release/ai-memory" ~/.local/bin/ai-memory`), do it
*after* the `install-hooks` call above, not before — see the `install-hooks`
symlink caution in Scenario A
([#546](https://github.com/akitaonrails/ai-memory/issues/546)); it applies
here too and is the exact layout that bug was filed against.

## Scenario C: Docker Wrapper

Use this when you want the server data in a Docker volume while the agent still
runs as a native macOS process. The wrapper renders host-side agent config with
`http://127.0.0.1:49374`, but its own thin-client commands reach the server from
inside a helper container via Docker Desktop's `host.docker.internal` alias.

This assumes the `ai-memory` thin-client wrapper is already on `PATH`; if
`ai-memory --version` doesn't resolve yet, install it first via the
[README Docker quick-start](../README.md#docker) (downloads a small shell
script to `~/.local/bin/ai-memory`). On a stock macOS Terminal `~/.local/bin`
is **not** on `PATH` by default — add
`export PATH="$HOME/.local/bin:$PATH"` to `~/.zshrc` if `which ai-memory`
comes up empty after installing the wrapper.

```bash
# Start the server. The image default allowlist includes host.docker.internal so
# wrapper thin-client commands (status, search, …) are not rejected with 403.
docker run -d --name ai-memory --restart unless-stopped \
    -p 127.0.0.1:49374:49374 -v ai-memory-data:/data \
    akitaonrails/ai-memory:latest

# Wire the native host agent. The wrapper keeps these rendered URLs on loopback.
ai-memory install-mcp   --client claude-code --apply
ai-memory install-hooks --agent  claude-code --apply
```

The wrapper is a shell script, not the native binary, so the `install-hooks`
symlink caution above (#546) does not apply here.

The published Docker image includes both `linux/amd64` and `linux/arm64`, so
Apple Silicon pulls the native arm64 image without `--platform linux/amd64`.

## Run as a Login Service (launchd)

Every scenario above leaves the server in the foreground: close that terminal
and capture stops. The macOS counterpart of a systemd user unit is a
**LaunchAgent** — a plist in `~/Library/LaunchAgents/` that the per-user
launchd domain starts at login and restarts on failure. The repo ships one at
`packaging/launchd/com.github.akitaonrails.ai-memory.plist`, and the macOS
release tarballs include it.

launchd expands nothing. A plist has no home specifier and no
`EnvironmentFile`, so every path in it is a literal and the template carries
two placeholders you substitute at install time. Run this from the extracted
tarball (Scenario A) or the repo root (Scenario B):

```bash
# launchd creates the log files but not their parent directory, and a missing
# one is a silent redirect failure.
mkdir -p ~/Library/Logs/ai-memory

# Wherever you keep the binary: ~/Applications/ai-memory/ai-memory for a
# release tarball, ./target/release/ai-memory for a source build.
AI_MEMORY_BIN=~/Applications/ai-memory/ai-memory

sed -e "s|__AI_MEMORY_BIN__|$AI_MEMORY_BIN|" \
    -e "s|__HOME__|$HOME|" \
    packaging/launchd/com.github.akitaonrails.ai-memory.plist \
    > ~/Library/LaunchAgents/com.github.akitaonrails.ai-memory.plist

launchctl bootstrap gui/$(id -u) \
    ~/Library/LaunchAgents/com.github.akitaonrails.ai-memory.plist
```

The agent runs `ai-memory serve --transport http --enable-web` and passes
neither `--data-dir` nor `--config`: on macOS the binary already defaults to
`~/Library/Application Support/ai-memory` with the config file inside it, so
naming them would only add two more paths to substitute. `bind` comes from that
config, defaulting to `127.0.0.1:49374`. Re-render and reload the plist if you
move the binary.

Verify it came up:

```bash
launchctl print gui/$(id -u)/com.github.akitaonrails.ai-memory | grep state
curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:49374/mcp   # 405
tail -f ~/Library/Logs/ai-memory/stderr.log
```

### Coming from systemd

| systemd `--user` | launchd (per-user domain) |
|---|---|
| `systemctl --user enable --now ai-memory` | `launchctl bootstrap gui/$(id -u) <plist>` |
| `systemctl --user disable --now ai-memory` | `launchctl bootout gui/$(id -u)/<label>` |
| `systemctl --user status ai-memory` | `launchctl print gui/$(id -u)/<label>` |
| `systemctl --user restart ai-memory` | `launchctl kickstart -k gui/$(id -u)/<label>` |
| `journalctl --user -u ai-memory -f` | `tail -f ~/Library/Logs/ai-memory/stderr.log` |
| `loginctl enable-linger $USER` | no equivalent — a LaunchAgent stops at logout |
| `EnvironmentFile=` | no equivalent — see the token note below |

`<label>` is `com.github.akitaonrails.ai-memory`. After editing the plist,
`bootout` then `bootstrap` again; `kickstart -k` only restarts the process and
does not re-read the definition.

### If you configure a bearer token

`AI_MEMORY_AUTH_TOKEN` is read from the process environment only — it is not a
`config.toml` key, and launchd has no `EnvironmentFile`. A single-user loopback
setup needs no token at all. If you do set one, add it to your rendered plist
and tighten the file, because `~/Library/LaunchAgents` is not private:

```xml
  <key>EnvironmentVariables</key>
  <dict>
    <key>AI_MEMORY_AUTH_TOKEN</key>
    <string>…</string>
  </dict>
```

```bash
chmod 600 ~/Library/LaunchAgents/com.github.akitaonrails.ai-memory.plist
```

### Removing the agent

```bash
launchctl bootout gui/$(id -u)/com.github.akitaonrails.ai-memory
rm ~/Library/LaunchAgents/com.github.akitaonrails.ai-memory.plist
```

Nothing rotates the two log files; they grow without bound. Add a
`newsyslog.d` entry or truncate them periodically if that matters to you.

> **Validated on** macOS 26.6.2 (build 25G83, Apple Silicon) with ai-memory
> v1.38.0 installed per Scenario A, and separately with v1.21.0 to confirm the
> agent does not depend on a recently added flag. Confirmed: `launchctl
> bootstrap`; the job `running` with `last exit code = (never exited)` rather
> than crash-looping; the launchd child (`PPID 1`) owning `127.0.0.1:49374`, so
> the reply came from the agent rather than a foreground server left over on the
> same port; `~/Library/Application Support/ai-memory` resolved and logged as
> the data dir with no `--data-dir` passed; `405` from `GET /mcp` on the bound
> port; `KeepAlive` — the served process was `SIGKILL`ed and a replacement was
> answering about a second later, with `runs` incrementing; and a clean
> `launchctl bootout`. Start-at-login was configured but not independently
> exercised, since that needs a logout. `ThrottleInterval` is left at its 10s
> default, so a crash within 10s of startup is respawned after that delay rather
> than immediately. Corrections from anyone on a different macOS version are
> welcome.

## Hook Platform on macOS

`AI_MEMORY_HOOK_PLATFORM` selects how hook commands are rendered. On macOS the
two relevant values are `posix-native` (direct binary call; the native default)
and `posix` (the bundled `.sh` scripts; the Docker-wrapper default). Set it
before running `install-hooks` so the choice is baked into the rendered
commands. The native hook spools events locally, does short session-start
cleanup, and starts a detached session-end `hook-drain` helper; the whole-minute
spool-timing overrides are shared with Windows and documented in
[`docs/windows.md`](windows.md#tuning-the-spool-timings-high-latency-instances).

Native `posix-native` `ai-memory hook` commands enforce the nearest-marker
`[capture] ignore_paths` policy before spool or network delivery. The Docker
wrapper's `posix` shell-script path does not. Re-run `install-hooks --agent
<agent> --apply` after upgrading to refresh an existing native install; see
[Capture exclusions](marker-file.md#capture-exclusions).

## Troubleshooting on macOS

- **`403 forbidden host` from Docker-wrapper CLI commands:** update the Docker
  image and wrapper script. Current images allowlist `host.docker.internal` for
  loopback-published Docker Desktop servers.
- **Agent config points at `host.docker.internal`:** re-run `ai-memory
  install-mcp --client <client> --apply` and `ai-memory install-hooks --agent
  <agent> --apply` with the current wrapper. Host-side agent config should use
  `http://127.0.0.1:49374`.
- **Hooks bundle not found from a release archive:** ensure you extracted the
  whole tarball, not just the binary. Current `install-hooks` probes the sibling
  `hooks/` directory automatically.
- **Platform-mismatch warning on Apple Silicon:** update to a current Docker
  tag. Tagged releases publish a multi-arch manifest with `linux/arm64`.
- **`ai-memory: command not found` in a new terminal tab:** Scenario A/B's
  `./ai-memory`/`./target/release/ai-memory` is a relative path, so it only
  resolves from inside the install/build directory. Either keep `cd`-ing there
  first, or symlink the binary onto `PATH` once you're done wiring hooks (see
  the Scenario A/B notes above) so plain `ai-memory` works everywhere.
- **`install-hooks` wires the wrong (or no) `hooks/` bundle even though the
  tarball was extracted whole:** if the binary is reached through a symlink
  (e.g. you put it on `PATH` before running `install-hooks`), macOS discovery
  does not resolve the symlink and searches the wrong parent directories —
  see [#546](https://github.com/akitaonrails/ai-memory/issues/546). On a
  clean machine this fails outright with `Error: could not locate hooks
  directory. Tried: [...]`; if a hooks cache from an earlier install already
  exists under `~/Library/Application Support/ai-memory`, it can silently
  reuse that stale copy instead and report success. Run `install-hooks` via
  the real extracted/built path instead of the symlink
  until that's fixed.

## Suggested Test Checklist

1. `ai-memory serve --bind 127.0.0.1:49374` starts and logs `bind=127.0.0.1:49374`
   (`./ai-memory serve …`, or `./target/release/ai-memory serve …` for
   Scenario B, if you haven't put it on `PATH` yet).
2. `curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:49374/mcp` returns
   `405` (reachable; GET not allowed), confirming the loopback server is up.
3. `install-hooks --agent claude-code --apply` writes hook commands that
   reference `http://127.0.0.1:49374` and host-side paths.
4. `install-mcp --client claude-code` renders `http://127.0.0.1:49374/mcp`.
5. Launch the agent, call `memory_status`, send a prompt, then confirm capture
   (`ai-memory status` shows non-zero observations, or query the SQLite
   `observations` table).

Report which scenario you used, your chip (Apple Silicon / Intel), the agent and
version, and whether hooks executed or failed with a connect/resolve error.
