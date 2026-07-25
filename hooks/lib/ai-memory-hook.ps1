function Get-AiMemoryCwd {
    param([string] $Payload)
    if (-not $Payload) { return $null }
    try {
        $Parsed = $Payload | ConvertFrom-Json -ErrorAction Stop
        foreach ($Name in @("cwd", "current_dir", "working_dir", "directory")) {
            $Value = $Parsed.$Name
            if ($Value -is [string] -and $Value.Length -gt 0) { return $Value }
        }
        $Paths = $Parsed.workspacePaths
        if ($null -ne $Paths -and $Paths.Count -gt 0 -and $Paths[0] -is [string] -and $Paths[0].Length -gt 0) {
            return $Paths[0]
        }
    } catch {
    }
    $match = [regex]::Match($Payload, '"cwd"\s*:\s*"([^"]*)"')
    if ($match.Success) { return $match.Groups[1].Value }
    $workspaceMatch = [regex]::Match($Payload, '"workspacePaths"\s*:\s*\[\s*"([^"]*)"')
    if ($workspaceMatch.Success) { return $workspaceMatch.Groups[1].Value }
    return $null
}

function Resolve-AiMemoryCwd {
    param([string] $Payload, [string] $Agent)
    $Cwd = Get-AiMemoryCwd -Payload $Payload
    if ($Cwd) { return $Cwd }
    if ($Agent -eq "devin" -and $env:DEVIN_PROJECT_DIR) { return $env:DEVIN_PROJECT_DIR }
    if ($Agent -eq "devin") {
        try {
            $Location = (Get-Location).Path
            if ($Location) { return $Location }
        } catch {
        }
    }
    return $null
}

function Get-AiMemoryMarkerToml {
    param([string] $Cwd)
    if (-not $Cwd) { return $null }
    $dir = $Cwd
    while ($dir -and (Test-Path $dir)) {
        $candidate = Join-Path $dir ".ai-memory.toml"
        if (Test-Path $candidate -PathType Leaf) { return $candidate }
        if ($env:HOME -and $dir -eq $env:HOME) { return $null }
        if ($env:USERPROFILE -and $dir -eq $env:USERPROFILE) { return $null }
        $parent = Split-Path $dir -Parent
        if (-not $parent -or $parent -eq $dir) { return $null }
        $dir = $parent
    }
    return $null
}

function Get-AiMemoryTomlKey {
    param([string] $File, [string] $Key)
    if (-not (Test-Path $File -PathType Leaf)) { return $null }
    foreach ($line in Get-Content $File) {
        $m = [regex]::Match($line, "^\s*$Key\s*=\s*`"([^`"]*)`"")
        if ($m.Success) { return $m.Groups[1].Value }
    }
    return $null
}

# Like Get-AiMemoryTomlKey but also accepts a BARE value (`key = true` /
# `key = 6000`), so section-style flags such as
# `[briefing] inject_on_session_start = true` work quoted or not. Parity
# with `parse_toml_flag` in hook_capture.rs: line-based, first match wins,
# trailing `# comment` stripped.
function Get-AiMemoryTomlFlag {
    param([string] $File, [string] $Key)
    if (-not (Test-Path $File -PathType Leaf)) { return $null }
    foreach ($line in Get-Content $File) {
        $m = [regex]::Match($line, "^\s*$Key\s*=\s*`"?([^`"#]*)`"?\s*(#.*)?$")
        if ($m.Success) { return $m.Groups[1].Value.Trim() }
    }
    return $null
}

function Test-AiMemoryTruthy {
    param([string] $Value)
    if (-not $Value) { return $false }
    return @("1", "true", "yes", "on") -contains $Value.Trim().ToLowerInvariant()
}

# Build `&briefing=<v>[&briefing_budget=<v>]` from the `[briefing]` section
# of the marker walked up from $Cwd. Returns "" when the repo did not opt
# in. Used by agents that deliver the compiled project brief once per
# session (kimi-code, via the first user prompt — kimi discards
# SessionStart hook stdout) so the server does not recompose the brief on
# every request. The char-budget clamp is server-side.
function Get-AiMemoryBriefingQuery {
    param([string] $Cwd)
    if (-not $Cwd) { return "" }
    $marker = Get-AiMemoryMarkerToml -Cwd $Cwd
    if (-not $marker) { return "" }
    $briefing = Get-AiMemoryTomlFlag -File $marker -Key "inject_on_session_start"
    if (-not (Test-AiMemoryTruthy -Value $briefing)) { return "" }
    $budget = Get-AiMemoryTomlFlag -File $marker -Key "max_chars"
    $qs = "&briefing=$([uri]::EscapeDataString($briefing))"
    if ($budget) { $qs += "&briefing_budget=$([uri]::EscapeDataString($budget))" }
    return $qs
}

# Path of the once-per-session "brief delivered" marker for $Key (a session
# id or a caller-built fallback key), sanitized to a safe file name under
# the shared state dir.
function Get-AiMemoryBriefedFile {
    param([string] $Key)
    $safe = ($Key -replace '[^A-Za-z0-9._-]', '_')
    return (Join-Path (Join-Path (Get-AiMemoryStateDir) "briefed") $safe)
}

function Set-AiMemoryBriefed {
    param([string] $Path)
    if (-not $Path) { return }
    $dir = Split-Path $Path -Parent
    New-Item -ItemType Directory -Force -Path $dir -ErrorAction SilentlyContinue | Out-Null
    New-Item -ItemType File -Force -Path $Path -ErrorAction SilentlyContinue | Out-Null
    Get-ChildItem -Path $dir -File -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTimeUtc -Descending |
        Select-Object -Skip 512 |
        Remove-Item -Force -ErrorAction SilentlyContinue
}

# Resolve the basename of the MAIN git repository root for $Cwd, following the
# worktree commondir pointer so every linked worktree collapses to one stable
# name. Mirrors the POSIX `ai_memory_repo_root_project`: a containerized server
# cannot see the host checkout, so repo-root must be resolved here. Returns
# $null when git is unavailable or $Cwd is not inside a git work tree.
function Get-AiMemoryRepoRootProject {
    param([string] $Cwd)
    if (-not $Cwd) { return $null }
    if (-not (Get-Command git -ErrorAction SilentlyContinue)) { return $null }
    $inside = (& git -C $Cwd rev-parse --is-inside-work-tree 2>$null)
    if ($inside -ne "true") { return $null }
    $common = (& git -C $Cwd rev-parse --path-format=absolute --git-common-dir 2>$null)
    if (-not $common) { return $null }
    $root = Split-Path $common -Parent
    if (-not $root -or $root -eq [System.IO.Path]::GetPathRoot($root)) { return $null }
    return Split-Path $root -Leaf
}

function Get-AiMemoryMarkerQuery {
    param([string] $Cwd)
    if (-not $Cwd) { return "" }
    $qs = "&cwd=$([uri]::EscapeDataString($Cwd))"
    $ws = $null
    $proj = $null
    $strategy = $null
    $dropSubagent = $null
    $marker = Get-AiMemoryMarkerToml -Cwd $Cwd
    if ($marker) {
        $ws = Get-AiMemoryTomlKey -File $marker -Key "workspace"
        $proj = Get-AiMemoryTomlKey -File $marker -Key "project"
        $strategy = Get-AiMemoryTomlKey -File $marker -Key "project_strategy"
        $dropSubagent = Get-AiMemoryTomlKey -File $marker -Key "drop_subagent_captures"
    }
    # Install-time default baked into the hook command by
    # `install-hooks --project-strategy` fills the strategy only when no marker
    # pinned one. A marker's explicit project / project_strategy still win.
    if (-not $strategy -and $env:AI_MEMORY_PROJECT_STRATEGY) {
        $strategy = $env:AI_MEMORY_PROJECT_STRATEGY
    }
    # repo-root must be resolved host-side (the server may not see this checkout);
    # only when no explicit project is pinned. Explicit project always wins.
    if (-not $proj -and ($strategy -eq "repo-root" -or $strategy -eq "repo_root")) {
        $proj = Get-AiMemoryRepoRootProject -Cwd $Cwd
    }
    if ($ws) { $qs += "&workspace=$([uri]::EscapeDataString($ws))" }
    if ($proj) { $qs += "&project=$([uri]::EscapeDataString($proj))" }
    if ($strategy) { $qs += "&project_strategy=$([uri]::EscapeDataString($strategy))" }
    # Per-project drop_subagent_captures opt-in: forward to the server, which
    # interprets truthiness (1/true/...) and scopes the drop to this project.
    if ($dropSubagent) { $qs += "&drop_subagent=$([uri]::EscapeDataString($dropSubagent))" }
    return $qs
}

function Get-AiMemoryStateDir {
    if ($env:AI_MEMORY_DATA_DIR) { return $env:AI_MEMORY_DATA_DIR }
    if ($env:XDG_DATA_HOME) { return (Join-Path $env:XDG_DATA_HOME "ai-memory") }
    if ($env:LOCALAPPDATA) { return (Join-Path $env:LOCALAPPDATA "ai-memory") }
    if ($env:HOME) { return (Join-Path $env:HOME ".local/share/ai-memory") }
    return ".ai-memory"
}

function Get-AiMemorySessionIdPath {
    param([string] $Agent)
    return (Join-Path (Join-Path (Get-AiMemoryStateDir) "hook-state") "$Agent-session-id")
}

function New-AiMemorySessionId {
    param([string] $Agent)
    return "$Agent-$([DateTimeOffset]::UtcNow.ToUnixTimeSeconds())-$PID"
}

function Get-AiMemorySessionIdQuery {
    param([string] $Agent, [string] $Event)
    if ($env:AI_MEMORY_SESSION_ID) {
        return "&session_id=$([uri]::EscapeDataString($env:AI_MEMORY_SESSION_ID))"
    }

    $Path = Get-AiMemorySessionIdPath -Agent $Agent
    $SessionId = $null
    if ($Event -ne "session-start" -and (Test-Path $Path -PathType Leaf)) {
        $SessionId = (Get-Content $Path -TotalCount 1 -ErrorAction SilentlyContinue)
    }
    if (-not $SessionId) {
        $SessionId = New-AiMemorySessionId -Agent $Agent
        $Parent = Split-Path $Path -Parent
        New-Item -ItemType Directory -Force -Path $Parent -ErrorAction SilentlyContinue | Out-Null
        Set-Content -Path $Path -Value $SessionId -NoNewline -ErrorAction SilentlyContinue
    }
    return "&session_id=$([uri]::EscapeDataString($SessionId))"
}

function Clear-AiMemorySessionId {
    param([string] $Agent)
    $Path = Get-AiMemorySessionIdPath -Agent $Agent
    Remove-Item -Force -ErrorAction SilentlyContinue $Path
}

function Read-AiMemoryStdin {
    try {
        if (-not [Console]::IsInputRedirected) { return "" }
        $StdinStream = [Console]::OpenStandardInput()
        $StdinReader = [System.IO.StreamReader]::new($StdinStream, [System.Text.Encoding]::UTF8, $false, 4096)
        $ReadTask = $StdinReader.ReadToEndAsync()
        if ($ReadTask.Wait(2000)) {
            $result = $ReadTask.Result
            $StdinReader.Dispose()
            $StdinStream.Dispose()
            return $result
        }
        $StdinReader.Dispose()
        $StdinStream.Dispose()
    } catch {
    }
    return ""
}

function Invoke-AiMemoryHook {
    param(
        [Parameter(Mandatory = $true)] [string] $Event,
        [Parameter(Mandatory = $true)] [string] $Agent,
        [switch] $FetchHandoff,
        [switch] $AntigravityPreInvocationOutput,
        # Deliver the `[briefing]` compiled project brief on the FIRST
        # handoff fetch of a session only (kimi-code's user-prompt path:
        # kimi discards SessionStart hook stdout, so the brief rides the
        # first prompt — parity with Claude's once-per-SessionStart brief).
        # Later fetches keep the handoff but drop the briefing params so the
        # server does not recompose the brief per prompt.
        [switch] $BriefingOncePerSession
    )

    $Server = if ($env:AI_MEMORY_HOOK_URL) { $env:AI_MEMORY_HOOK_URL } else { "http://127.0.0.1:49374" }
    $Payload = Read-AiMemoryStdin
    # Assistant/Stop capture (#196) is native-only. Scoped to claude-code + stop
    # so a PostToolUse whose tool output legitimately contains the literal string
    # is unaffected. If a Stop payload still carries the raw field, drop the whole
    # event rather than POST it verbatim (the script fallback cannot sanitize it).
    if ($Agent -eq "claude-code" -and $Event -eq "stop" -and $Payload -and $Payload.Contains('"last_assistant_message"')) {
        return
    }
    $Cwd = Resolve-AiMemoryCwd -Payload $Payload -Agent $Agent
    $QS = Get-AiMemoryMarkerQuery -Cwd $Cwd
    if ($env:AI_MEMORY_RUN_ID) {
        $QS += "&managed_run=$([Uri]::EscapeDataString($env:AI_MEMORY_RUN_ID))"
    }
    $SessionQS = ""
    if ($Agent -eq "devin") {
        $SessionQS = Get-AiMemorySessionIdQuery -Agent $Agent -Event $Event
    }
    $Headers = @{}

    if ($env:AI_MEMORY_AUTH_TOKEN) {
        $Headers["Authorization"] = "Bearer $env:AI_MEMORY_AUTH_TOKEN"
    }

    try {
        Invoke-WebRequest `
            -UseBasicParsing `
            -TimeoutSec 3 `
            -Method Post `
            -Uri "$Server/hook?event=$Event&agent=$Agent$QS$SessionQS" `
            -Headers $Headers `
            -ContentType "application/json" `
            -Body $Payload | Out-Null
    } catch {
    }
    if ($Agent -eq "devin" -and $Event -eq "session-end") {
        Clear-AiMemorySessionId -Agent $Agent
    }

    if ($FetchHandoff) {
        $NativeSessionQS = ""
        try {
            $ParsedPayload = $Payload | ConvertFrom-Json
            $NativeSessionId = @(
                $ParsedPayload.session_id,
                $ParsedPayload.sessionId,
                $ParsedPayload.sessionID,
                $ParsedPayload.session,
                $ParsedPayload.conversationId
            ) | Where-Object { $_ } | Select-Object -First 1
            if ($NativeSessionId) {
                $NativeSessionQS = "&session_id=$([Uri]::EscapeDataString([string]$NativeSessionId))"
            }
        } catch {
        }
        # Once-per-session briefing gate. Marker files are created only for
        # repositories that opt in. Prefer the native session id when Kimi
        # supplies one; otherwise use a stable hash of agent+cwd.
        $BriefQS = ""
        $BriefFile = $null
        if ($BriefingOncePerSession) {
            $BriefQS = Get-AiMemoryBriefingQuery -Cwd $Cwd
            if ($BriefQS) {
                $BriefKey = [string]$NativeSessionId
                if (-not $BriefKey) {
                    $Sha = [System.Security.Cryptography.SHA256]::Create()
                    $Bytes = $Sha.ComputeHash([System.Text.Encoding]::UTF8.GetBytes("$Agent`n$Cwd"))
                    $BriefKey = (($Bytes | ForEach-Object { $_.ToString("x2") }) -join "")
                }
                $BriefFile = Get-AiMemoryBriefedFile -Key $BriefKey
                if (Test-Path $BriefFile -PathType Leaf) {
                    $BriefQS = ""
                }
            }
        }
        try {
            $Response = Invoke-WebRequest `
                -UseBasicParsing `
                -TimeoutSec 2 `
                -Uri "$Server/handoff?agent=$Agent$QS$NativeSessionQS$BriefQS" `
                -Headers $Headers
            if ($null -ne $Response -and $Response.Content) {
                if ($AntigravityPreInvocationOutput) {
                    $Payload = @{
                        injectSteps = @(@{ ephemeralMessage = $Response.Content })
                    }
                    [Console]::Out.Write(($Payload | ConvertTo-Json -Depth 5 -Compress))
                } else {
                    [Console]::Out.Write($Response.Content)
                }
            } elseif ($AntigravityPreInvocationOutput) {
                [Console]::Out.Write("{}")
            }
        } catch {
            if ($AntigravityPreInvocationOutput) {
                [Console]::Out.Write("{}")
            }
        }
        # Mark the session as briefed only AFTER the GET completed —
        # success or error (fail-open: with the server down, re-sending the
        # brief-flagged request on every prompt would deliver nothing
        # anyway, and the one lost brief returns on the next session).
        if ($BriefFile) {
            Set-AiMemoryBriefed -Path $BriefFile
        }
    } elseif ($AntigravityPreInvocationOutput) {
        [Console]::Out.Write("{}")
    }
}
