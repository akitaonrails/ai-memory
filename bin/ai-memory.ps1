# ai-memory.ps1 - Windows PowerShell wrapper for the Docker image.
#
# This mirrors bin/ai-memory for Windows users who run Docker Desktop.
# It forwards CLI commands into the Linux container with the user's home
# directory mounted at /host-home and the current project mounted at /work.
#
# The wrapper tells the Linux container to render Windows PowerShell hook
# commands that point at the host's staged .ps1 scripts.
#
# Managed launch commands (run, show, continue, resume, workstreams,
# rename-workstream) never enter the container: they run through the
# native Windows client instead, mirroring the intercept bin/ai-memory
# does (see the native-host branch below).
[CmdletBinding()]
param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]]$CommandArgs
)

$ErrorActionPreference = "Stop"

function Get-EnvOrDefault {
    param(
        [Parameter(Mandatory = $true)][string]$Name,
        [Parameter(Mandatory = $true)][string]$Default
    )
    $value = [Environment]::GetEnvironmentVariable($Name)
    if ([string]::IsNullOrWhiteSpace($value)) {
        return $Default
    }
    return $value
}

# Resolve the real subcommand, skipping the global options that may precede
# it. `--config` and `--data-dir` take a separate value, so their argument has
# to be stepped over as well or it would be mistaken for the subcommand.
function Get-WrapperSubcommand {
    param([string[]]$WrapperArgs)

    $Index = 0
    while ($Index -lt $WrapperArgs.Count) {
        $Arg = $WrapperArgs[$Index]
        if ($Arg -ceq "--config" -or $Arg -ceq "--data-dir") {
            if ($Index + 1 -ge $WrapperArgs.Count -or
                [string]::IsNullOrEmpty($WrapperArgs[$Index + 1]) -or
                ($WrapperArgs[$Index + 1].Length -gt 1 -and
                    $WrapperArgs[$Index + 1].StartsWith("-"))) {
                return ""
            }
            $Index += 2
        } elseif ($Arg -clike "--config=*" -or $Arg -clike "--data-dir=*") {
            if ($Arg.EndsWith("=")) {
                return ""
            }
            $Index += 1
        } elseif ($Arg.StartsWith("-")) {
            # Leave unknown and malformed options to the CLI parser.
            return ""
        } else {
            return $Arg
        }
    }
    return ""
}

$Image = Get-EnvOrDefault "AI_MEMORY_IMAGE" "akitaonrails/ai-memory:latest"
$Docker = Get-EnvOrDefault "AI_MEMORY_DOCKER" "docker"
$DataVolume = Get-EnvOrDefault "AI_MEMORY_DATA_VOLUME" "ai-memory-data"

# Managed launch/discovery commands execute on the host: checkouts, harness
# CLIs and native transcript stores are host resources the Linux helper
# container cannot see (it has no claude/codex). Mirror the intercept in
# bin/ai-memory (the same six subcommands, kept in sync by tests) and run
# them through the native Windows client. These commands never touch the
# container engine, so this branch sits above the engine check below.
$NativeHostCommands = @(
    "run",
    "show",
    "continue",
    "resume",
    "workstreams",
    "rename-workstream"
)

# Resolve the native client: AI_MEMORY_NATIVE_BIN wins when it exists,
# otherwise the documented install location: install.ps1 (docs/windows.md,
# Scenario C) unpacks the release zip to %LOCALAPPDATA%\Programs\ai-memory.
function Get-NativeHostBin {
    $Explicit = [Environment]::GetEnvironmentVariable("AI_MEMORY_NATIVE_BIN")
    if (-not [string]::IsNullOrWhiteSpace($Explicit)) {
        if (-not (Test-Path -LiteralPath $Explicit -PathType Leaf)) {
            Write-Error "ai-memory: AI_MEMORY_NATIVE_BIN does not point at an existing ai-memory.exe: $Explicit"
            exit 127
        }
        $Resolved = [IO.Path]::GetFullPath($Explicit)
        # Handing the wrapper itself back as the client would recurse through
        # this same intercept forever (bin/ai-memory refuses its own path too).
        foreach ($SelfPath in @($PSCommandPath, (Join-Path $PSScriptRoot "ai-memory.cmd"))) {
            if ($SelfPath -and (Test-Path -LiteralPath $SelfPath -PathType Leaf) -and
                ([IO.Path]::GetFullPath($SelfPath) -ieq $Resolved)) {
                Write-Error "ai-memory: AI_MEMORY_NATIVE_BIN points at the container wrapper itself; point it at the native release ai-memory.exe"
                exit 127
            }
        }
        return $Resolved
    }
    if ($env:LOCALAPPDATA) {
        $DefaultBin = Join-Path (Join-Path $env:LOCALAPPDATA "Programs") "ai-memory\ai-memory.exe"
        if (Test-Path -LiteralPath $DefaultBin -PathType Leaf) {
            return $DefaultBin
        }
        $Expected = $DefaultBin
    } else {
        $Expected = "%LOCALAPPDATA%\Programs\ai-memory\ai-memory.exe"
    }
    Write-Error "ai-memory: the native host client was not found (expected at $Expected). Managed launch commands (run, show, continue, resume, workstreams, rename-workstream) execute host harnesses through the native Windows client, not the Linux helper container. Install it with: irm https://github.com/akitaonrails/ai-memory/releases/latest/download/install.ps1 | iex (docs/windows.md), or point AI_MEMORY_NATIVE_BIN at an existing ai-memory.exe."
    exit 127
}

$VolumeTokenFile = if ($env:LOCALAPPDATA) {
    Join-Path $env:LOCALAPPDATA "ai-memory\server-token"
} else {
    Join-Path $HOME ".local\share\ai-memory\server-token"
}

function Test-VolumeTokenApplies {
    if (-not [string]::IsNullOrEmpty([Environment]::GetEnvironmentVariable("AI_MEMORY_AUTH_TOKEN")) -or
        -not [string]::IsNullOrEmpty([Environment]::GetEnvironmentVariable("AI_MEMORY_AUTH__BEARER_TOKEN_FILE"))) {
        return $false
    }
    return -not ($env:AI_MEMORY_DATA_DIR -and
        (Test-Path -LiteralPath $env:AI_MEMORY_DATA_DIR -PathType Container))
}

function Use-VolumeToken {
    if ((Test-VolumeTokenApplies) -and
        (Test-Path -LiteralPath $VolumeTokenFile -PathType Leaf) -and
        (Get-Item -LiteralPath $VolumeTokenFile).Length -gt 0) {
        $env:AI_MEMORY_AUTH__BEARER_TOKEN_FILE = $VolumeTokenFile
    }
}

# Refresh once during native hook setup. Managed launches stay independent of
# the container engine and reuse this host copy, matching the POSIX wrapper.
function Update-VolumeToken {
    if (-not (Test-VolumeTokenApplies)) {
        return
    }
    if (-not (Get-Command $Docker -ErrorAction SilentlyContinue)) {
        Use-VolumeToken
        return
    }
    $PreviousErrorAction = $ErrorActionPreference
    $ErrorActionPreference = "SilentlyContinue"
    try {
        $Token = (& $Docker run --rm -v "${DataVolume}:/data:ro" --entrypoint /bin/sh $Image -c 'if grep -qs ''^[[:space:]]*bearer_token_file[[:space:]]*=[[:space:]]*"auth-tokens/default"'' /data/config.toml; then cat /data/auth-tokens/default 2>/dev/null; fi; exit 0' 2>$null)
        $Succeeded = $LASTEXITCODE -eq 0
    } finally {
        $ErrorActionPreference = $PreviousErrorAction
    }
    if (-not $Succeeded) {
        Use-VolumeToken
        return
    }
    $Token = ([string]$Token).Trim()
    if ([string]::IsNullOrEmpty($Token)) {
        Remove-Item -LiteralPath $VolumeTokenFile -Force -ErrorAction SilentlyContinue
        return
    }
    $TokenDir = Split-Path -Parent $VolumeTokenFile
    New-Item -ItemType Directory -Force -Path $TokenDir | Out-Null
    $Temporary = "$VolumeTokenFile.tmp.$PID"
    [IO.File]::WriteAllText($Temporary, "$Token`n", [Text.UTF8Encoding]::new($false))
    Move-Item -LiteralPath $Temporary -Destination $VolumeTokenFile -Force
    Use-VolumeToken
}

if ((Get-WrapperSubcommand $CommandArgs) -in $NativeHostCommands) {
    $NativeBin = Get-NativeHostBin
    Use-VolumeToken
    # The intercepted commands run on the host, so AI_MEMORY_SERVER_URL is
    # inherited untouched: the client's own default (http://127.0.0.1:49374)
    # already reaches the port Docker Desktop publishes there; an explicit
    # value wins, the same precedence bin/ai-memory gives its native host
    # commands. The host.docker.internal alias is only for the helper
    # container path below.
    #
    # Hooks and MCP keep the wiring install-hooks/install-mcp rendered through
    # the container (windows-platform entries). A native-side autowire would
    # re-render them around this separately installed client, and could not
    # read the named volume's auth token from the host either, so run must
    # not rewire harness config unless the operator opted in (#1545).
    if ($null -eq [Environment]::GetEnvironmentVariable("AI_MEMORY_RUN_AUTOWIRE")) {
        $env:AI_MEMORY_RUN_AUTOWIRE = "false"
    }
    & $NativeBin @CommandArgs
    exit $LASTEXITCODE
}

if ((Get-WrapperSubcommand $CommandArgs) -eq "install-hooks" -and
    [string]::IsNullOrEmpty([Environment]::GetEnvironmentVariable("AI_MEMORY_HOOK_PLATFORM"))) {
    $NativeBin = Get-NativeHostBin
    Update-VolumeToken
    & $NativeBin @CommandArgs
    exit $LASTEXITCODE
}

if (-not (Get-Command $Docker -ErrorAction SilentlyContinue)) {
    Write-Error "Could not find Docker command '$Docker'. Install Docker Desktop or set AI_MEMORY_DOCKER."
    exit 127
}

if ((Get-WrapperSubcommand $CommandArgs) -ceq "upgrade") {
    & $Docker pull $Image
    if ($LASTEXITCODE -ne 0) {
        exit $LASTEXITCODE
    }
    # Pulling the image refreshes only one of three pieces. The wrapper
    # scripts are refreshed by re-running the Scenario B install snippet in
    # docs/windows.md, and the native client used by run/show/... upgrades
    # itself with its own subcommand (docs/windows.md, Scenario C).
    Write-Output "Refreshed the container image only."
    Write-Output "  wrapper scripts: re-run the install snippet in docs/windows.md (Scenario B)"
    Write-Output "  native client:   ai-memory.exe upgrade (docs/windows.md, Scenario C)"
    exit 0
}

$HomePath = (Resolve-Path -LiteralPath $HOME).Path
$WorkPath = (Get-Location).Path
$HookHostRoot = ($HomePath -replace '\\', '/') + "/.local/share/ai-memory/hooks"

$HomeRoot = $HomePath.TrimEnd([char[]]@('/', '\'))
$HomePrefix = $HomeRoot + [IO.Path]::DirectorySeparatorChar
$InsideHome = $WorkPath.Equals($HomeRoot, [StringComparison]::OrdinalIgnoreCase) -or
    $WorkPath.StartsWith($HomePrefix, [StringComparison]::OrdinalIgnoreCase)
$ScopeMountArgs = @()
if ($InsideHome) {
    $ScopeSuffix = if ($WorkPath.Length -eq $HomeRoot.Length) {
        ""
    } else {
        $WorkPath.Substring($HomeRoot.Length) -replace '\\', '/'
    }
    $ScopeCwd = "/host-home$ScopeSuffix"
} else {
    $ScopeRoot = $WorkPath
    if (Get-Command git -ErrorAction SilentlyContinue) {
        # Probe for the repo root, but never let git's "not a git
        # repository" message abort the wrapper. Under this script's
        # $ErrorActionPreference = 'Stop', Windows PowerShell 5.1 turns a
        # REDIRECTED native stderr (the 2>$null below) into a *terminating*
        # error, so running `status` outside a repo crashed the wrapper
        # (#591). Suppress locally, gate on the exit code, and restore. The
        # unredirected `& $Docker` calls are unaffected, which is why only
        # this probe tripped it.
        $PrevErrorAction = $ErrorActionPreference
        $ErrorActionPreference = 'SilentlyContinue'
        try {
            $DetectedScopeRoot = (& git -C $WorkPath rev-parse --show-toplevel 2>$null)
            if ($LASTEXITCODE -eq 0 -and -not [string]::IsNullOrWhiteSpace($DetectedScopeRoot)) {
                $ScopeRoot = [IO.Path]::GetFullPath($DetectedScopeRoot.Trim())
            }
        } finally {
            $ErrorActionPreference = $PrevErrorAction
        }
    }
    $ScopeRoot = $ScopeRoot.TrimEnd([char[]]@('/', '\'))
    $ScopePrefix = $ScopeRoot + [IO.Path]::DirectorySeparatorChar
    if ($WorkPath.Equals($ScopeRoot, [StringComparison]::OrdinalIgnoreCase)) {
        $ScopeSuffix = ""
    } elseif ($WorkPath.StartsWith($ScopePrefix, [StringComparison]::OrdinalIgnoreCase)) {
        $ScopeSuffix = $WorkPath.Substring($ScopeRoot.Length) -replace '\\', '/'
    } else {
        $ScopeRoot = $WorkPath
        $ScopeSuffix = ""
    }
    $ScopeMountArgs = @("-v", "${ScopeRoot}:/scope:ro")
    $ScopeCwd = "/scope$ScopeSuffix"
}

$DockerArgs = @("run", "--rm", "-i")
# Keep stdin attached in every mode. `AI_MEMORY_NO_TTY` suppresses only the
# pseudo-terminal allocation.
if (-not $env:AI_MEMORY_NO_TTY -and -not [Console]::IsInputRedirected -and -not [Console]::IsOutputRedirected) {
    $DockerArgs += "-t"
}

$DockerArgs += @(
    "-v", "${HomePath}:/host-home",
    "-v", "${WorkPath}:/work",
    "-w", "/work",
    "-e", "HOME=/host-home",
    "-e", "AI_MEMORY_HOST_CWD=$WorkPath",
    "-e", "AI_MEMORY_SCOPE_CWD=$ScopeCwd",
    "-e", "AI_MEMORY_DATA_DIR=/data",
    "-e", "AI_MEMORY_HOOK_PLATFORM=windows",
    "-e", "AI_MEMORY_HOOKS_HOST_ROOT=$HookHostRoot"
)
$DockerArgs += $ScopeMountArgs

if ($env:AI_MEMORY_DATA_DIR -and (Test-Path -LiteralPath $env:AI_MEMORY_DATA_DIR -PathType Container)) {
    $DataPath = (Resolve-Path -LiteralPath $env:AI_MEMORY_DATA_DIR).Path
    $DockerArgs += @("-v", "${DataPath}:/data")
} else {
    $DockerArgs += @("-v", "${DataVolume}:/data")
}

foreach ($Name in @(
    "AI_MEMORY_SERVER_URL",
    "AI_MEMORY_AUTH_TOKEN",
    "AI_MEMORY_LLM_PROVIDER",
    "AI_MEMORY_LLM_MODEL",
    "AI_MEMORY_LLM_BASE_URL",
    "AI_MEMORY_COPILOT_CLIENT_ID",
    "AI_MEMORY_EMBEDDING_PROVIDER",
    "AI_MEMORY_EMBEDDING_MODEL",
    "AI_MEMORY_EMBEDDING_BASE_URL",
    "AI_MEMORY_EMBEDDING_DIM",
    "AI_MEMORY_ALLOWED_HOSTS",
    "AI_MEMORY_WORKSTREAM_ID",
    "CLAUDE_CONFIG_DIR",
    "CLAUDE_CODE_SESSION_ID",
    "MUSE_SESSION_ID",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_OAUTH_TOKEN",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "OPENAI_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "COPILOT_GITHUB_TOKEN",
    "GITHUB_COPILOT_API_TOKEN",
    "COPILOT_API_URL",
    "VOYAGE_API_KEY",
    "LLM_API_KEY",
    "EMBEDDING_API_KEY",
    "OPENCODE_API_KEY",
    "RUST_LOG"
)) {
    if (-not [string]::IsNullOrEmpty([Environment]::GetEnvironmentVariable($Name))) {
        $DockerArgs += @("-e", $Name)
    }
}

# Presence-based, not non-empty-based like the loop above: an operator sets
# one of these to the empty string to deliberately clear a
# config.toml-configured prefix without editing the file (see
# Config::load's figment overlay), and a present-but-empty value must reach
# the container for that to work — `IsNullOrEmpty` above would drop it,
# making the wrapper indistinguishable from the variable never having been
# set at all. `GetEnvironmentVariable` returns `$null` only when the
# variable is truly unset, and `""` when it is set-but-empty, so a `-ne
# $null` check is exactly the presence test needed here.
#
# An operator's own `$env:NAME = ''` additionally needs PowerShell 7.5+
# (first built on .NET 9) to leave a set-but-empty variable rather than
# deleting it; not exercised on a real pwsh runtime.
# https://learn.microsoft.com/en-us/dotnet/api/system.environment.setenvironmentvariable
# https://learn.microsoft.com/en-us/powershell/scripting/whats-new/what-s-new-in-powershell-75
foreach ($Name in @(
    "AI_MEMORY_EMBEDDING_QUERY_PREFIX",
    "AI_MEMORY_EMBEDDING_DOCUMENT_PREFIX"
)) {
    if ($null -ne [Environment]::GetEnvironmentVariable($Name)) {
        $DockerArgs += @("-e", $Name)
    }
}

# Docker Desktop gives Windows no host networking for Linux containers, so a
# thin-client command (status, search, bootstrap, ...) reaches the loopback-
# published server from this helper container through Docker Desktop's host
# alias; 127.0.0.1 would mean the helper container itself and the call dies
# with "Connection refused". But install-mcp/install-hooks/setup-agent RENDER
# the URL into the *host* agent config, and host.docker.internal does NOT
# resolve on the Windows host: baking it in silently breaks MCP and every
# capture hook. So for those commands leave AI_MEMORY_SERVER_URL unset, letting
# the CLI render its host-reachable default (http://127.0.0.1:49374).
# (issue #107)
$RendersHostConfig = (Get-WrapperSubcommand $CommandArgs) -in @(
    "install-mcp",
    "install-hooks",
    "setup-agent"
)
if (-not $RendersHostConfig -and
    [string]::IsNullOrEmpty([Environment]::GetEnvironmentVariable("AI_MEMORY_SERVER_URL"))) {
    $DockerArgs += @("-e", "AI_MEMORY_SERVER_URL=http://host.docker.internal:49374")
}

$DockerArgs += $Image
$DockerArgs += $CommandArgs

& $Docker @DockerArgs
exit $LASTEXITCODE
