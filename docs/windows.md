# Windows Support

Windows support has two modes. Pick the mode that matches where your
agent CLI actually runs.

## Rule Of Thumb

Run `install-mcp` and `install-hooks` from the same environment that
launches Claude Code, Codex, Devin CLI, Cursor, Gemini CLI, Kimi Code, or
another agent.

- If the agent runs inside WSL2, install ai-memory inside WSL2.
- If the agent runs as a native Windows process, install ai-memory from
  PowerShell on Windows.
- Do not mix the Windows wrapper with WSL2-launched agents unless you
  deliberately override every config and hook path.

The difference matters because hook configs contain executable paths.
WSL2 agents need Linux paths and POSIX `.sh` hooks. Native Windows
agents need Windows paths, but the hook runner is agent-specific: local
supported profiles default to host-native commands. Claude Code may use
its supported direct exec form (`command: "…ai-memory.exe"`, `args: ["hook",
"--event", …]`) with no shell — see [Native Hook
Command](#native-hook-command-claude-code-on-windows). Other agents use native
single command strings matching their hook schema. PowerShell/Git Bash script
bundles are compatibility fallbacks and do not enforce capture-policy v1.

## Scenario A: Everything Inside WSL2

This is the most Linux-like Windows setup. Use it when your agent CLI is
installed and launched inside a WSL2 distro.

```bash
# Inside WSL2.
mkdir -p ~/.local/bin
curl -fsSL https://raw.githubusercontent.com/akitaonrails/ai-memory/main/bin/ai-memory \
    -o ~/.local/bin/ai-memory
chmod +x ~/.local/bin/ai-memory
export PATH="$HOME/.local/bin:$PATH"

docker run -d --name ai-memory \
    --restart unless-stopped \
    -p 127.0.0.1:49374:49374 \
    -v ai-memory-data:/data \
    akitaonrails/ai-memory:latest

ai-memory install-mcp --client claude-code --apply
ai-memory install-hooks --agent claude-code --apply
```

In this mode, ai-memory behaves like Linux:

- Config files are written under your WSL2 home directory.
- Hook scripts are staged under `~/.local/share/ai-memory/hooks/`.
- Hook commands point at `.sh` scripts.
- The agent should also be launched from WSL2 so it can execute those
  WSL paths.

If Docker Desktop provides the Docker engine to WSL2, enable WSL
integration for the distro first. If you run a native Docker engine
inside WSL2, no Windows wrapper is involved.

## Scenario B: Native Windows With Docker Desktop

Use this when the agent CLI runs as a native Windows process and you want
the ai-memory server to run from the Docker image.

```powershell
# Install the Windows Docker wrapper.
$UserBin = "$HOME\bin"
New-Item -ItemType Directory -Force $UserBin | Out-Null
foreach ($File in @("ai-memory.ps1", "ai-memory.cmd")) {
    Invoke-WebRequest `
        -Uri "https://raw.githubusercontent.com/akitaonrails/ai-memory/main/bin/$File" `
        -OutFile "$UserBin\$File"
}
Get-ChildItem "$UserBin\ai-memory.*" | Unblock-File

# Put the wrapper directory on your user PATH for future terminals.
$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (($UserPath -split ';') -notcontains $UserBin) {
    $NewUserPath = (($UserPath, $UserBin) | Where-Object { $_ }) -join ";"
    [Environment]::SetEnvironmentVariable("Path", $NewUserPath, "User")
    $env:Path = "$env:Path;$UserBin"
}

# Start the server with Docker Desktop.
docker run -d --name ai-memory `
    --restart unless-stopped `
    -p 127.0.0.1:49374:49374 `
    -v ai-memory-data:/data `
    akitaonrails/ai-memory:latest

# Do not also run `ai-memory serve`; the long-lived container above is the server.

# Verify the wrapper can reach the server.
ai-memory status

# Wire MCP and lifecycle hooks for a native Windows agent.
ai-memory install-mcp --client claude-code --apply
ai-memory install-hooks --agent claude-code --apply
```

In this mode, the PowerShell wrapper runs the Linux container but tells
the CLI to render hook commands for the native Windows agent:

- Config files are written through the mounted Windows home directory.
- Hook scripts are staged under `$HOME\.local\share\ai-memory\hooks\`.
- Local supported profiles default to host-native commands. Claude Code may use
  its exec form (`command` executable + `args` argv array), while other agents
  use native single command strings matching their hook schema. The wrapper
  renders those `.ps1` fallback commands with PowerShell `-EncodedCommand` so a
  hook runner cannot expand their `$env:` setup before the inner PowerShell
  process receives it. Those commands force text output and suppress progress
  records so nested PowerShell runners do not emit `CLIXML` hook stderr.
  PowerShell/Git Bash script bundles are compatibility fallbacks and do not
  enforce capture-policy v1.

After upgrading the wrapper/image, rerun `install-hooks --agent <agent> --apply`
for each native Windows agent so existing hook entries receive the current
command form.

Use the matching `--client` / `--agent` values for other clients, for
example `codex`, `devin`, `kimi-code`, `cursor`, or `gemini-cli`.

For Devin, `install-mcp --client devin --apply` writes MCP config to
`%USERPROFILE%\.devin\config.json`. `install-hooks --agent devin --apply`
writes lifecycle hooks to `%USERPROFILE%\.devin\hooks.v1.json` by default;
pass `--config-file "%USERPROFILE%\.devin\config.json"` if you want hooks under
the `hooks` key in Devin's main config file.

## Scenario C: Prebuilt Release Binary (No Toolchain)

Use this when the agent CLI runs as a native Windows process and you want
the fast native hook path **without** installing a Rust toolchain or
Docker. Each tagged release publishes
`ai-memory-windows-x86_64.zip` (see the repo's Releases page).

```powershell
# Download + extract into your user data dir (any stable path works; the
# native hook exec-form command is rendered from wherever ai-memory.exe lives).
$Dest = "$env:LOCALAPPDATA\ai-memory"
New-Item -ItemType Directory -Force $Dest | Out-Null
Invoke-WebRequest `
    -Uri "https://github.com/akitaonrails/ai-memory/releases/latest/download/ai-memory-windows-x86_64.zip" `
    -OutFile "$env:TEMP\ai-memory.zip"
Expand-Archive "$env:TEMP\ai-memory.zip" -DestinationPath $Dest -Force
Get-ChildItem "$Dest\ai-memory.exe" | Unblock-File

# Put it on PATH for future terminals (optional but convenient).
$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (($UserPath -split ';') -notcontains $Dest) {
    [Environment]::SetEnvironmentVariable("Path", "$UserPath;$Dest", "User")
    $env:Path = "$env:Path;$Dest"
}

# Wire MCP + lifecycle hooks against your server.
& "$Dest\ai-memory.exe" install-mcp --client claude-code --apply
& "$Dest\ai-memory.exe" install-hooks --agent claude-code --apply `
    --server-url "https://memory.example.com" --auth-token "<token>"
```

The zip mirrors the Linux release tarball, minus the Linux-only service
assets: it contains `ai-memory.exe`, the full `hooks/` bundle (`.ps1` +
`.sh`), `crates/ai-memory-cli/templates/config.default.toml`, `README.md`,
`LICENSE`, and `docs/{install,windows}.md`. Because `install-hooks` reads
the `ai-memory.exe` path from the running binary, keep the extracted `.exe`
at a stable location (re-run `install-hooks` if you move it).

## Scenario D: Native Windows Source Build

Use this when developing ai-memory itself on Windows or when you do not
want the Docker wrapper for CLI commands.

```powershell
git clone https://github.com/akitaonrails/ai-memory .\ai-memory
Set-Location .\ai-memory
cargo build --workspace
cargo test --workspace

target\debug\ai-memory.exe init
target\debug\ai-memory.exe serve --transport http --bind 127.0.0.1:49374
```

For release validation from Git Bash on native Windows, use the same checkout
with the Rust MSVC toolchain active:

```bash
cargo test --workspace
cargo build --locked --release -p ai-memory-cli
./target/release/ai-memory.exe --version
```

The version output should match the package version for the checkout.

The Tailwind build step supports the pinned
`tailwindcss-windows-x64.exe` binary and falls back to PowerShell
`Invoke-WebRequest` when `curl`/`wget` are unavailable. You should not
need `TAILWIND_SKIP=1` for normal Windows builds.

Keep Git for Windows' `git.exe` on `PATH` for native builds and hook runs. When
libgit2 hits a Windows path-resolution error while opening a newly initialized
wiki repository, ai-memory falls back to the Git CLI instead of treating that
specific condition as fatal.

From another PowerShell window in the repo:

```powershell
target\debug\ai-memory.exe install-mcp --client claude-code --apply
target\debug\ai-memory.exe install-hooks --agent claude-code --apply
```

Native Windows builds render agent-specific host-native lifecycle commands.
Claude Code may use its supported binary exec form (see below); other agents
use native single command strings matching their hook schema. The bundled `.sh`
and `.ps1` event scripts are compatibility fallbacks and do not enforce
capture-policy v1; tests enforce one-to-one event/agent parity between them.

### Capture-policy support

Native `ai-memory.exe hook` commands enforce the nearest-marker `[capture]
ignore_paths` policy before spool or network delivery. The legacy `.ps1` and
`.sh` paths do not. After upgrading, rerun `install-hooks --agent <agent>
--apply` to refresh native hook entries; selected-install capability output says
whether enforcement is active. See [Capture exclusions](marker-file.md#capture-exclusions).

## Native Hook Command (Claude Code on Windows)

By default on native Windows, Claude Code hooks are rendered using Claude's
exec form: `command` is the real `ai-memory.exe` path and `args` is an argv
array. This directly spawns the binary instead of sending one quoted string to a
shell or using a `bash -c` wrapper around a `.sh` script:

```json
{
  "type": "command",
  "command": "C:\\Users\\you\\.cargo\\bin\\ai-memory.exe",
  "args": ["hook", "--event", "pre-tool-use", "--agent", "claude-code", "--server-url", "http://host:49374", "--auth-token", "..."]
}
```

This avoids spawning Git Bash plus `cat`/`sed`/`curl` child processes on
every tool call. Process spawning is expensive on Windows, so the native
path is roughly 3-5× faster per hook (measured ~735 ms shell → ~150-205 ms
native on an i7-6700HQ). Notes:

- The binary path comes from the `ai-memory` that runs `install-hooks`, so
  `cargo install --path crates/ai-memory-cli` puts it on a stable
  `~/.cargo/bin` path.
- Exec form requires a real executable path (`.exe`). It does not run `.cmd` or
  `.bat` shims through a shell. `install-hooks` uses the path of the running
  `ai-memory.exe`, so release binaries and Cargo-built binaries work directly.
- The `.sh`/`.ps1` scripts stay bundled as a fallback — the Docker /
  `setup-agent` flow (no local binary) keeps emitting the shell command.
- `AI_MEMORY_HOOK_PLATFORM` accepts five values:
  - `windows-native` — Claude exec-form direct binary call (default on native Windows).
  - `windows` — PowerShell `-EncodedCommand` + staged `.ps1` script. The native
    Windows Docker-wrapper default because the helper container cannot install
    its Linux binary into a host hook entry.
  - `windows-bash` — `bash -c` + `.sh` through Git Bash (the previous
    default; set this to opt back in, or as a fallback for older Claude Code
    builds that do not support exec form).
  - `posix` — POSIX `.sh`. The Linux/macOS Docker-wrapper default (the host has
    no local binary); set it explicitly to opt a native install back into the
    scripts.
  - `posix-native` — direct binary call on macOS / Linux (`<exe> hook
    --event …`) instead of the `.sh` script, so the hook uses the local event
    spool + OIDC-token fallback. The **default for native macOS / Linux
    Claude Code installs** (cargo / release binary), mirroring
    `windows-native`. The Linux/macOS Docker wrapper forces `posix`, so its
    host-rendered config keeps the `.sh` scripts.

  Set the env var before running `install-hooks` so the chosen platform
  is baked into the rendered hook commands.

Project auto-scope treats Windows backslashes and POSIX slashes as the same path
separator when comparing hook `cwd`, stored `repo_path`, and the home-directory
catch-all guard. Wrappers or tests that need a host home different from the
process `HOME` can set `AI_MEMORY_HOME`; it is normalized through the same path
boundary before startup healing or cwd-prefix matching.

### Tuning the spool timings (high-latency instances)

The native hook spools events locally. Session start does a short bounded cleanup
drain before fetching a handoff; session end starts a detached `hook-drain`
helper so Claude Code and other agents are not kept open by a large backlog. The
built-in timings stay short on agent-facing paths, but high-latency or
large-backlog instances can raise them with whole-minute overrides. Unlike
`AI_MEMORY_HOOK_PLATFORM`, these are read by the hook **at runtime**, so they
apply to the agent's environment (no re-`install-hooks` needed):

Native hooks accept well-formed JSON with or without one leading UTF-8 BOM, as
some PowerShell pipelines add that marker when writing to a native process. Any
other malformed stdin is not spooled or sent; the hook prints a fixed warning
to stderr, returns `{}` on stdout, and exits successfully so it cannot break the
host agent. The warning never includes payload contents.

| Env var | Built-in default | Max override | What it caps |
|---|---:|---:|---|
| `AI_MEMORY_HOOK_DRAIN_TIMEOUT_MINUTES` | 3 seconds | 60 minutes | each event POST during a drain |
| `AI_MEMORY_HOOK_HANDOFF_TIMEOUT_MINUTES` | 3 seconds | 60 minutes | the synchronous `session-start` handoff GET |
| `AI_MEMORY_HOOK_START_BUDGET_MINUTES` | 3 seconds | 60 minutes | total time `session-start` may spend waiting for the drain lock and cleanup draining |
| `AI_MEMORY_HOOK_BACKGROUND_DRAIN_BUDGET_MINUTES` | 5 minutes | 60 minutes | total time the detached `hook-drain` helper may spend after `session-end` |
| `AI_MEMORY_HOOK_INCREMENTAL_THRESHOLD` | 32 events | positive integer | spool backlog size that triggers a 250 ms `post-tool-use` catch-up drain |

Timing values must be positive whole minutes. Missing, empty, non-numeric, or
zero values fall back to the built-in defaults; values above 60 are clamped. The
incremental threshold is a positive event count; invalid values fall back to 32.
The session-start budget caps how long the hook may block before handoff fetch;
the background budget caps detached cleanup after session-end and does not keep
the agent waiting.

On Windows, a contended drain lock can be reported as the native
`ERROR_LOCK_VIOLATION` code instead of Rust's `WouldBlock` error kind.
ai-memory treats both as normal lock-busy states, so concurrent drains wait,
skip, or expire according to the same spool timing rules instead of failing the
hook.

## Current Harness Caveats

Windows hook support is new and needs real-world testing against native
Windows agent builds.

- Claude Code may be used natively on Windows or from inside WSL2. Native
  Claude Code invokes hooks as a direct binary call (no shell) by default;
  `AI_MEMORY_HOOK_PLATFORM=windows-bash` restores the Git Bash `bash -c`
  path. WSL2 Claude Code uses normal WSL `.sh` paths.
- Codex, Devin CLI, OpenCode, Cursor, Gemini CLI, Grok Build CLI, Zero,
  Kimi Code, and OpenClaw may each choose different
  Windows config locations or shell execution behavior. ai-memory uses
  the current best-known defaults, but they need validation on real
  installations.
- MCP over HTTP should be less path-sensitive than hooks, but
  `install-mcp --apply` still writes to a client-specific config file;
  confirm the agent actually loads it.
- OpenClaw, OpenCode, OMP / Oh My Pi, and Pi use generated TypeScript
  integrations rather than the shell hook bundle, so their Windows
  behavior depends on the host runtime loading those files correctly.
  Pi's generated extension also bridges MCP tools because Pi has no native
  `mcp.json` install surface.

## Suggested Test Checklist

For WSL2:

1. Run all install commands inside WSL2.
2. Confirm generated hook commands reference `.sh` files under WSL paths.
3. Launch the agent from WSL2.
4. Call `memory_status` from the agent.
5. Record `ai-memory status`, send a prompt, then confirm its `sessions` or
   `observations` count increased.

For native Windows:

1. Run all install commands from PowerShell or `cmd.exe` using
   `ai-memory` / `ai-memory.ps1`.
2. Confirm generated hook commands match the agent: Claude Code should use
   the native `"…ai-memory.exe" hook --event …` command (or `bash -c` + `.sh`
   when `AI_MEMORY_HOOK_PLATFORM=windows-bash`); other script-hook agents
   should use `powershell.exe ... -EncodedCommand <payload>` entries for the
   generated `.ps1` hooks under your Windows home directory.
3. Launch the native Windows agent.
4. Call `memory_status` from the agent.
5. Record `ai-memory status`, send a prompt, then confirm its `sessions` or
   `observations` count increased.

Report which mode you tested, which agent and version you used, and
whether the hook command executed or failed with a path/shell error.
The built-in `/web` browser lists compiled wiki pages; zero pages there does
not mean the raw hook observations were missed.
