# ai-memory.ps1 - Windows PowerShell wrapper for the Docker image.
#
# This mirrors bin/ai-memory for Windows users who run Docker Desktop.
# It forwards CLI commands into the Linux container with the user's home
# directory mounted at /host-home and the current project mounted at /work.
#
# The wrapper tells the Linux container to render Windows PowerShell hook
# commands that point at the host's staged .ps1 scripts.
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

$Image = Get-EnvOrDefault "AI_MEMORY_IMAGE" "akitaonrails/ai-memory:latest"
$Docker = Get-EnvOrDefault "AI_MEMORY_DOCKER" "docker"
$DataVolume = Get-EnvOrDefault "AI_MEMORY_DATA_VOLUME" "ai-memory-data"

if (-not (Get-Command $Docker -ErrorAction SilentlyContinue)) {
    Write-Error "Could not find Docker command '$Docker'. Install Docker Desktop or set AI_MEMORY_DOCKER."
    exit 127
}

if ($CommandArgs.Count -gt 0 -and $CommandArgs[0] -eq "upgrade") {
    & $Docker pull $Image
    exit $LASTEXITCODE
}

$HomePath = (Resolve-Path -LiteralPath $HOME).Path
$WorkPath = (Get-Location).Path
$HookHostRoot = ($HomePath -replace '\\', '/') + "/.local/share/ai-memory/hooks"

$HomeRoot = $HomePath.TrimEnd([char[]]@('/', '\\'))
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
        $DetectedScopeRoot = (& git -C $WorkPath rev-parse --show-toplevel 2>$null)
        if (-not [string]::IsNullOrWhiteSpace($DetectedScopeRoot)) {
            $ScopeRoot = [IO.Path]::GetFullPath($DetectedScopeRoot.Trim())
        }
    }
    $ScopeRoot = $ScopeRoot.TrimEnd([char[]]@('/', '\\'))
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
    "AI_MEMORY_EMBEDDING_PROVIDER",
    "AI_MEMORY_EMBEDDING_MODEL",
    "AI_MEMORY_EMBEDDING_BASE_URL",
    "AI_MEMORY_EMBEDDING_DIM",
    "AI_MEMORY_ALLOWED_HOSTS",
    "CLAUDE_CODE_SESSION_ID",
    "ANTHROPIC_API_KEY",
    "OPENAI_API_KEY",
    "VOYAGE_API_KEY",
    "LLM_API_KEY",
    "RUST_LOG"
)) {
    if (-not [string]::IsNullOrEmpty([Environment]::GetEnvironmentVariable($Name))) {
        $DockerArgs += @("-e", $Name)
    }
}

$DockerArgs += $Image
$DockerArgs += $CommandArgs

& $Docker @DockerArgs
exit $LASTEXITCODE
