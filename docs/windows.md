# Windows Support

Windows support has two modes. Pick the mode that matches where your
agent CLI actually runs.

## Rule Of Thumb

Run `install-mcp` and `install-hooks` from the same environment that
launches Claude Code, Codex, Command Code, Devin CLI, Cursor, Gemini CLI, Kimi Code, Kiro CLI,
Antigravity CLI, or another agent.

- If the agent runs inside WSL2, install ai-memory inside WSL2.
- If the agent runs as a native Windows process, install ai-memory from
  PowerShell on Windows.
- Do not mix the Windows wrapper with WSL2-launched agents unless you
  deliberately override every config and hook path.
- To keep the server running across logoff and reboot on native
  Windows, use a service wrapper — see [Scenario
  E](#scenario-e-keep-the-server-running-native-windows-service). Do not
  launch it from a Scheduled Task via `Start-Process`; that looks like it
  works and is silently killed at the next reboot.

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
wrapper_base=https://github.com/akitaonrails/ai-memory/releases/latest/download/ai-memory-wrapper
wrapper_tmp="$(mktemp -d)"
trap 'rm -rf "$wrapper_tmp"' EXIT
curl -fsSL "$wrapper_base" -o "$wrapper_tmp/ai-memory-wrapper"
curl -fsSL "$wrapper_base.sha256" -o "$wrapper_tmp/ai-memory-wrapper.sha256"
(cd "$wrapper_tmp" && sha256sum -c ai-memory-wrapper.sha256)
install -m 0755 "$wrapper_tmp/ai-memory-wrapper" ~/.local/bin/ai-memory
rm -rf "$wrapper_tmp"
trap - EXIT
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
the ai-memory server to run from the Docker image. The wrapper renders
host-side agent config with `http://127.0.0.1:49374`, but its own thin-client
commands reach the server from inside a helper container via Docker Desktop's
`host.docker.internal` alias, because Docker Desktop gives Linux containers no
host networking on Windows. Set `AI_MEMORY_SERVER_URL` only when the server
lives somewhere else (a homelab or remote host); the wrapper honours it and
skips the alias.

```powershell
# Install the Windows Docker wrapper.
$UserBin = "$HOME\bin"
New-Item -ItemType Directory -Force $UserBin | Out-Null
$ReleaseBase = "https://github.com/akitaonrails/ai-memory/releases/latest/download"
$WrapperAssets = @{
    "ai-memory.ps1" = "ai-memory-wrapper.ps1"
    "ai-memory.cmd" = "ai-memory-wrapper.cmd"
}
foreach ($Entry in $WrapperAssets.GetEnumerator()) {
    $File = $Entry.Key
    $Asset = $Entry.Value
    Invoke-WebRequest `
        -Uri "$ReleaseBase/$Asset" `
        -OutFile "$UserBin\$File"
    Invoke-WebRequest `
        -Uri "$ReleaseBase/$Asset.sha256" `
        -OutFile "$UserBin\$File.sha256"
    $Expected = ((Get-Content "$UserBin\$File.sha256" -Raw) -split '\s+')[0]
    $Actual = (Get-FileHash "$UserBin\$File" -Algorithm SHA256).Hash.ToLower()
    if ($Actual -ne $Expected.ToLower()) {
        throw "Checksum mismatch for $Asset"
    }
    Remove-Item "$UserBin\$File.sha256"
}
Get-ChildItem "$UserBin\ai-memory.*" | Unblock-File

# Put the wrapper directory on your user PATH for future terminals.
$UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (($UserPath -split ';') -notcontains $UserBin) {
    $NewUserPath = (($UserPath, $UserBin) | Where-Object { $_ }) -join ";"
    [Environment]::SetEnvironmentVariable("Path", $NewUserPath, "User")
    $env:Path = "$env:Path;$UserBin"
}

# Start the server with Docker Desktop. The image default allowlist includes
# host.docker.internal so wrapper thin-client commands (status, search, …) are
# not rejected with 403.
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
example `codex`, `command-code`, `devin`, `kimi-code`, `kiro-cli`, `cursor`, or `gemini-cli`.

For Devin, `install-mcp --client devin --apply` writes MCP config to
`%USERPROFILE%\.devin\config.json`. `install-hooks --agent devin --apply`
writes lifecycle hooks to `%USERPROFILE%\.devin\hooks.v1.json` by default;
pass `--config-file "%USERPROFILE%\.devin\config.json"` if you want hooks under
the `hooks` key in Devin's main config file.

For Kiro CLI, `install-mcp --client kiro-cli --apply` writes
`%USERPROFILE%\.kiro\settings\mcp.json` unless `$env:KIRO_HOME` overrides the
root. `install-hooks --agent kiro-cli --apply` updates existing v2 agent files
under `.kiro\agents`; use `--config-file` for a project-local agent. Kiro v3
hook capture remains unsupported pending sanitized live lifecycle and built-in
tool payload fixtures for its documented standalone schema. Run these commands
from the same native Windows environment that launches Kiro so generated
executable paths remain valid.

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

This leaves the server running only while a terminal is open. To keep it
up across logoff and reboot, continue to [Scenario
E](#scenario-e-keep-the-server-running-native-windows-service).

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

That last command holds the terminal. For a persistent server, see
[Scenario E](#scenario-e-keep-the-server-running-native-windows-service).

### If the build fails with `os error 4551`

On a machine with **Smart App Control** or **App Control for Business**
enforced, `cargo build` can fail before compiling anything of ours:

```
error: failed to run custom build command for `proc-macro2`
  An Application Control policy has blocked this file. (os error 4551)
```

This is not a toolchain problem and re-installing Rust will not fix it.
Cargo compiles each crate's `build.rs` into an unsigned executable under
`target\debug\build\`, and those policies block unsigned binaries from
running out of user-writable directories. `proc-macro2` and
`icu_properties_data` are usually the first to hit it because they build
early.

Add a path exclusion for the checkout's `target` directory (or build inside
a location your policy already trusts). Reported by @CaioCoelhoChaves while
running the measurements in #478.

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

## Scenario E: Keep The Server Running (Native Windows Service)

Scenarios C and D both end at `ai-memory serve` in a foreground terminal.
That is fine for a trial and wrong for daily use: close the window, log
off, or reboot, and the server is gone.

On Linux the packaged units in `packaging/systemd/` supply
`Restart=on-failure` and `RestartSec=5s`. Note where that resiliency
lives — systemd performs the restarts, not ai-memory. Native Windows
gets the same bargain through a **service wrapper**. ai-memory does not
implement a Windows Service control-code dispatcher, so `sc create` or
`New-Service` pointed straight at `ai-memory.exe` will not work: the SCM
starts the process, waits for it to report in, never hears from it, and
kills it.

### ⚠️ Do not launch the server from a Scheduled Task with `Start-Process`

**Symptoms: `LastTaskResult = 0` reports success forever, the server is
gone after every reboot, and there is no crash dialog, no Application
Error event, and no further line in the server log after its startup
line.** A failure whose only symptom is a green status is why this
warning exists.

The broken shape is a task whose action is
`powershell.exe -File start-server.ps1`, where the script calls
`Start-Process` to launch `ai-memory.exe`:

1. The task fires and `powershell.exe` runs the script.
2. `Start-Process` launches `ai-memory.exe` and returns **immediately**;
   it does not wait for the child.
3. The script has nothing left to do, so `powershell.exe` exits. Task
   Scheduler records the instance as completed with return code 0
   (operational-log events 201/102).
4. Task Scheduler runs each task instance inside a **job object**. When
   the action process exits, the job is torn down and every process
   still assigned to it is terminated. `ai-memory.exe` was started as a
   child of the task's PowerShell and never broke away from the job, so
   it dies with it — in the same second it started.

The server's log therefore contains exactly one line, its own startup
line, and nothing after it.

**If you specifically want Task Scheduler**, remove the wrapper: point
the task action at `ai-memory.exe` directly, with `serve` in the
arguments, so the process Task Scheduler monitors *is* the server and
stays in the job for as long as it runs. Then also:

- run the task **whether the user is logged on or not**, so it survives
  logoff and starts without an interactive session;
- clear the default **3-day execution time limit**
  (`-ExecutionTimeLimit ([TimeSpan]::Zero)`), which otherwise stops a
  perfectly healthy long-running server;
- set restart-on-failure explicitly (`-RestartCount`,
  `-RestartInterval`). A task has no `Restart=on-failure` unless asked.

Even corrected, that is a task pretending to be a service. Prefer the
wrapper below.

### WinSW

[WinSW](https://github.com/winsw/winsw) wraps any console binary as a
real Windows Service. Its config is a single XML file that sits beside
the executable, and it needs no separate service-manager install.

WinSW derives its config filename from its own, so **rename the
downloaded executable and give the XML the same base name**:

```powershell
$Dest = "$env:LOCALAPPDATA\ai-memory"
# Download the WinSW release matching your runtime (.NET Framework 4.6.1+
# or the self-contained .NET build) from https://github.com/winsw/winsw/releases
# Pin a specific release tag rather than "latest", and keep a note of which
# one you installed — it is a third-party binary running as a service.
Move-Item .\WinSW-x64.exe "$Dest\ai-memory-service.exe"
```

`ai-memory-service.xml`, beside it. Substitute your own expanded `$Dest`
for `C:\Users\you\AppData\Local\ai-memory` — spell it out rather than
leaving a variable in the file:

```xml
<service>
  <id>ai-memory</id>
  <name>ai-memory MCP server</name>
  <description>Long-term memory server for AI coding agents.</description>
  <executable>C:\Users\you\AppData\Local\ai-memory\ai-memory.exe</executable>
  <arguments>--data-dir "C:\Users\you\AppData\Local\ai-memory" serve --transport http --bind 127.0.0.1:49374</arguments>
  <startmode>Automatic</startmode>
  <onfailure action="restart" delay="5 sec"/>
  <log mode="roll"/>
</service>
```

`<onfailure action="restart" delay="5 sec"/>` is the SCM recovery action
that mirrors the units' `Restart=on-failure` + `RestartSec=5s`.

**Use absolute paths here, not `%LOCALAPPDATA%`.** A Windows service runs
as `LocalSystem` unless you say otherwise, and `%LOCALAPPDATA%` expands
against *that* account — `C:\Windows\System32\config\systemprofile\AppData\Local`
— so the service would quietly serve an empty data directory somewhere
you never look, while your real wiki sits untouched in your profile. Either
write the path out in full as above, or add a `<serviceaccount>` block so
the service runs as your user.

Install and start it:

```powershell
& "$Dest\ai-memory-service.exe" install
& "$Dest\ai-memory-service.exe" start
& "$Dest\ai-memory-service.exe" status

# Verify the server is actually answering, not merely "Running".
Get-Service ai-memory
ai-memory status
```

`stop`, `restart`, and `uninstall` are the remaining commands. Because
the bind is loopback, the service account does not affect reachability:
agents running as your user still reach `127.0.0.1:49374`. Only the data
directory is account-sensitive, which is what the absolute path above
settles.

Keep running `install-mcp` and `install-hooks` **as your own user**, not
as the service — they write per-user agent config, and the rule at the
top of this page still applies.

> Verified from the WinSW project documentation and the ai-memory CLI
> surface, then run end to end on real hardware by the reporter of
> [#530](https://github.com/akitaonrails/ai-memory/issues/530), who also
> root-caused the job-object failure mode above from the Task Scheduler
> event IDs.
>
> **Validated on** Windows 11 25H2 (build 26200.9168) with WinSW v2.12.0
> (`WinSW-x64.exe`) and ai-memory v1.38.0, from an elevated PowerShell
> session, against a throwaway service id, port and data directory.
> Confirmed: `install`; `start`; the service `Running` as `LocalSystem`
> with `StartType Automatic`; an MCP `initialize` answered over the bound
> port; the absolute `--data-dir` honoured, with no
> `systemprofile\AppData\Local\ai-memory` created; crash recovery — the
> wrapped `ai-memory.exe` was force-killed and a new process was answering
> 8s later, consistent with the 5-second `<onfailure>` delay plus startup;
> and a clean `stop` + `uninstall`. The service was configured with
> `StartType Automatic`; actual boot-time startup was not independently
> exercised. At the time of this validation (2026-08-31), v2.12.0 was the
> latest stable WinSW release; the published 3.x releases were
> prereleases. Corrections from anyone running a different Windows or
> WinSW version are welcome.

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
  `cargo install --locked --path crates/ai-memory-cli` puts it on a stable
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

- There is no Windows Service dispatcher in `ai-memory.exe`; persistence
  comes from a wrapper ([Scenario
  E](#scenario-e-keep-the-server-running-native-windows-service)), the same
  way it comes from systemd on Linux.

- Claude Code may be used natively on Windows or from inside WSL2. Native
  Claude Code invokes hooks as a direct binary call (no shell) by default;
  `AI_MEMORY_HOOK_PLATFORM=windows-bash` restores the Git Bash `bash -c`
  path. WSL2 Claude Code uses normal WSL `.sh` paths.
- Codex, Command Code, Devin CLI, OpenCode, Cursor, Gemini CLI, Antigravity
  CLI, Grok Build CLI, Zero, Kimi Code, and OpenClaw may each choose different
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
