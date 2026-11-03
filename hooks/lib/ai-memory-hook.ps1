function Get-AiMemoryCwd {
    param([string] $Payload)
    if (-not $Payload) { return $null }
    try {
        $Parsed = $Payload | ConvertFrom-Json -ErrorAction Stop
        foreach ($Name in @("cwd", "current_dir", "working_dir", "directory")) {
            $Value = $Parsed.$Name
            if ($Value -is [string] -and $Value.Length -gt 0) { return $Value }
        }
        # `workspacePaths` is Antigravity's spelling, `workspace_roots`
        # Cursor's. Cursor never sends a usable `cwd` (session events omit it,
        # tool events send ""), so the empty checks above fall through here.
        foreach ($Name in @("workspacePaths", "workspace_roots")) {
            $Paths = $Parsed.$Name
            if ($null -ne $Paths -and $Paths.Count -gt 0 -and $Paths[0] -is [string] -and $Paths[0].Length -gt 0) {
                return $Paths[0]
            }
        }
    } catch {
    }
    $match = [regex]::Match($Payload, '"cwd"\s*:\s*"([^"]+)"')
    if ($match.Success) { return $match.Groups[1].Value }
    foreach ($Name in @("workspacePaths", "workspace_roots")) {
        $workspaceMatch = [regex]::Match($Payload, '"' + $Name + '"\s*:\s*\[\s*"([^"]+)"')
        if ($workspaceMatch.Success) { return $workspaceMatch.Groups[1].Value }
    }
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

# $HOME (else USERPROFILE) without trailing separators, so the walks can
# compare it to `Split-Path` output; a root such as `C:\` keeps its own.
function Get-AiMemoryUserHome {
    $userHome = if ($env:HOME) { $env:HOME } else { $env:USERPROFILE }
    if (-not $userHome) { return $userHome }
    $trimmed = $userHome.TrimEnd([char[]]@('/', '\'))
    if (-not $trimmed -or $trimmed.EndsWith(':')) { return $userHome }
    return $trimmed
}

function Test-AiMemoryMarkerDeclaresSettings {
    param([string] $File)
    if (-not (Test-Path $File -PathType Leaf)) { return $false }
    try {
        $text = [IO.File]::ReadAllText($File)
    } catch {
        return $false
    }
    foreach ($key in @("workspace", "project", "project_strategy", "drop_subagent_captures", "identity", "identity_style")) {
        if ([regex]::IsMatch($text, "(?m)^\s*$key\s*=")) { return $true }
    }
    if ([regex]::IsMatch($text, '(?m)^\s*aliases\s*=')) { return $true }
    if ([regex]::IsMatch($text, '(?m)^[\s﻿]*server\s*=')) { return $true }
    foreach ($key in @("default_global", "inject_on_session_start", "inject_on_subagent_start", "max_chars", "contribute", "consume", "deliver_on_session_start")) {
        if ([regex]::IsMatch($text, "(?m)^\s*$key\s*=")) { return $true }
    }
    return $false
}

function Get-AiMemoryMarkerToml {
    param([string] $Cwd)
    if (-not $Cwd) { return $null }
    $dir = $Cwd
    $userHome = Get-AiMemoryUserHome
    $boundary = $null
    if ($userHome) {
        $userHomePrefix = $userHome.TrimEnd([char[]]@('/', '\')) + [IO.Path]::DirectorySeparatorChar
        $insideHome = ($dir -eq $userHome) -or $dir.StartsWith(
            $userHomePrefix,
            [StringComparison]::OrdinalIgnoreCase
        )
        if ($insideHome) {
            $boundary = $userHome
        } else {
            $probe = $dir
            while ($probe -and (Test-Path $probe)) {
                if (Test-Path (Join-Path $probe ".git")) {
                    $boundary = $probe
                    break
                }
                $parent = Split-Path $probe -Parent
                if (-not $parent -or $parent -eq $probe) { break }
                $probe = $parent
            }
            if (-not $boundary) { $boundary = $dir }
        }
    }
    while ($dir -and (Test-Path $dir)) {
        $candidate = Join-Path $dir ".ai-memory.toml"
        if (Test-Path $candidate -PathType Leaf) {
            if ($userHome -and $dir -eq $userHome) { return $candidate }
            if (Test-AiMemoryMarkerDeclaresSettings -File $candidate) { return $candidate }
        }
        if ($boundary -and $dir -eq $boundary) { return $null }
        $parent = Split-Path $dir -Parent
        if (-not $parent -or $parent -eq $dir) { return $null }
        $dir = $parent
    }
    return $null
}

# Whether any marker on the walk from $Cwd (default: the current directory)
# selects a server profile (`server = ...`, #992). This script fallback cannot
# route profiles — only native `ai-memory hook` commands can — so a routed
# repository must emit nothing rather than reach the install-default server.
# Mirrors the native walk: inside home it stops at home; outside it continues
# past the checkout root. An unreadable marker counts as a selection.
function Test-AiMemoryServerRouted {
    param([string] $Cwd)
    $dir = if ($Cwd) { $Cwd } else { (Get-Location).Path }
    $userHome = Get-AiMemoryUserHome
    $boundary = $null
    if ($userHome) {
        $userHomePrefix = $userHome.TrimEnd([char[]]@('/', '\')) + [IO.Path]::DirectorySeparatorChar
        if (($dir -eq $userHome) -or $dir.StartsWith($userHomePrefix, [StringComparison]::OrdinalIgnoreCase)) {
            $boundary = $userHome
        }
    }
    while ($dir) {
        $candidate = Join-Path $dir ".ai-memory.toml"
        if (Test-Path $candidate -PathType Leaf) {
            try {
                $text = [IO.File]::ReadAllText($candidate)
            } catch {
                return $true
            }
            if ([regex]::IsMatch($text, '(?m)^[\s﻿]*server\s*=')) { return $true }
        }
        if ($boundary -and $dir -eq $boundary) { return $false }
        $parent = Split-Path $dir -Parent
        if (-not $parent -or $parent -eq $dir) { return $false }
        $dir = $parent
    }
    return $false
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
function Get-AiMemoryTomlAliases {
    param([string] $File)
    if (-not (Test-Path $File -PathType Leaf)) { return $null }
    try {
        $text = [IO.File]::ReadAllText($File)
        $lines = $text -split "`r?`n"
        $inTable = $false
        $declarations = [Collections.Generic.List[string]]::new()
        foreach ($line in $lines) {
            $trimmed = $line.Trim()
            if ($trimmed.StartsWith("[")) { $inTable = $true }
            if ($trimmed -match '^aliases\s*=') {
                if ($inTable) { return "invalid" }
                $declarations.Add($trimmed)
            }
        }
        if ($declarations.Count -eq 0) { return $null }
        if ($declarations.Count -ne 1) { return "invalid" }
        $match = [regex]::Match($declarations[0], '^aliases\s*=\s*\[([^\]]*)\]\s*$')
        if (-not $match.Success) { return "invalid" }
        $aliases = [Collections.Generic.List[string]]::new()
        if (-not $match.Groups[1].Value.Trim()) { return $null }
        $parts = $match.Groups[1].Value.Split(',')
        if ($parts.Count -gt 16) { return "invalid" }
        foreach ($part in $parts) {
            if ($part.Contains("\")) { return "invalid" }
            $item = [regex]::Match($part, '^\s*"([^"]*)"\s*$')
            if (-not $item.Success) { return "invalid" }
            $value = $item.Groups[1].Value.Trim()
            if (-not $value -or $value.Length -gt 128 -or $value -cnotmatch '^[A-Za-z0-9][A-Za-z0-9._-]*$') { return "invalid" }
            if (-not $aliases.Contains($value)) {
                $aliases.Add($value)
            }
        }
        return [string](ConvertTo-Json -InputObject @($aliases) -Compress)
    } catch {
        return "invalid"
    }
}

function Get-AiMemoryRemoteIdentity {
    param([string] $Cwd)
    if (-not $Cwd -or -not (Get-Command git -ErrorAction SilentlyContinue)) { return $null }
    foreach ($name in @("upstream", "origin")) {
        $url = (& git -C $Cwd config --get "remote.$name.url" 2>$null)
        if (-not $url) { continue }
        $value = ConvertTo-AiMemoryRepositoryIdentity -Url ([string]$url)
        if ($value) { return $value }
    }
    return $null
}

function ConvertTo-AiMemoryRouteAliases {
    param([string] $Raw)
    if (-not $Raw) { return $null }
    $match = [regex]::Match($Raw, '^\s*\[([^\]]*)\]\s*$')
    if (-not $match.Success) { return "invalid" }
    if (-not $match.Groups[1].Value.Trim()) { return $null }
    $parts = $match.Groups[1].Value.Split(',')
    if ($parts.Count -gt 16) { return "invalid" }
    $aliases = [Collections.Generic.List[string]]::new()
    foreach ($part in $parts) {
        $item = [regex]::Match($part, '^\s*"([^"\\]*)"\s*$')
        if (-not $item.Success) { return "invalid" }
        $value = $item.Groups[1].Value.Trim()
        if (-not $value -or $value.Length -gt 128 -or $value -cnotmatch '^[A-Za-z0-9][A-Za-z0-9._-]*$') { return "invalid" }
        if (-not $aliases.Contains($value)) { $aliases.Add($value) }
    }
    return [string](ConvertTo-Json -InputObject @($aliases) -Compress)
}

function ConvertTo-AiMemoryRoutePath {
    param([string] $Raw, [string] $HomePath, [bool] $Bounded = $true)
    if (-not $Raw -or ($Bounded -and [Text.Encoding]::UTF8.GetByteCount($Raw) -gt 512)) { return $null }
    $value = $Raw.Replace('\', '/')
    if ($value.StartsWith("~/")) {
        $relativeDepth = 0
        foreach ($part in $value.Substring(2).Split('/')) {
            if (-not $part -or $part -eq ".") { continue }
            if ($part -eq "..") {
                if ($relativeDepth -eq 0) { return $null }
                $relativeDepth--
            } else {
                $relativeDepth++
            }
        }
        $value = $HomePath.Replace('\', '/').TrimEnd([char[]]@('/')) + "/" + $value.Substring(2)
    }
    $root = $null
    $rest = $null
    $windows = $false
    if ($value.StartsWith("//")) {
        $parts = @($value.Substring(2).Split('/') | Where-Object { $_ })
        if ($parts.Count -lt 2) { return $null }
        $root = "unc:" + $parts[0].ToLowerInvariant() + "/" + $parts[1].ToLowerInvariant()
        $rest = @($parts | Select-Object -Skip 2)
        $windows = $true
    } elseif ($value -cmatch '^[A-Za-z]:/') {
        $root = "drive:" + $value.Substring(0, 1).ToLowerInvariant()
        $rest = @($value.Substring(3).Split('/') | Where-Object { $_ })
        $windows = $true
    } elseif ($value.StartsWith("/")) {
        $root = "posix"
        $rest = @($value.Substring(1).Split('/') | Where-Object { $_ })
    } else {
        return $null
    }
    $stack = [Collections.Generic.List[string]]::new()
    foreach ($part in $rest) {
        if ($part -eq ".") { continue }
        if ($part -eq "..") {
            if ($stack.Count) { $stack.RemoveAt($stack.Count - 1) }
        } else {
            $stack.Add($(if ($windows) { $part.ToLowerInvariant() } else { $part }))
        }
    }
    if (-not $stack.Count) { return $null }
    $key = $root + "/" + ($stack -join "/")
    return [pscustomobject]@{ Key=$key; Depth=$stack.Count }
}

function Add-AiMemoryHomeRouteEntry {
    param([object] $Entry, [object] $Entries, [hashtable] $PathSeen, [string] $HomePath)
    if ($null -eq $Entry) { return $true }
    $workspace = $Entry.Fields["route_workspace"]
    $project = $Entry.Fields["route_project"]
    $style = $Entry.Fields["route_identity_style"]
    if (-not $workspace -or -not $project -or [Text.Encoding]::UTF8.GetByteCount($workspace) -gt 512 -or [Text.Encoding]::UTF8.GetByteCount($project) -gt 512 -or $workspace -cnotmatch '^[a-z0-9][a-z0-9._-]*$' -or $project -cnotmatch '^[a-z0-9][a-z0-9._-]*$') { return $false }
    if ($style -and @("path", "host_path") -cnotcontains $style) { return $false }
    $aliases = if ($Entry.Fields.ContainsKey("route_aliases")) { ConvertTo-AiMemoryRouteAliases $Entry.Fields["route_aliases"] } else { $null }
    if ($aliases -eq "invalid") { return $false }
    if ($Entry.Kind -eq "identity") {
        $hostName = $Entry.Selector.Split('/')[0]
        if ($Entry.Selector -cne $Entry.Selector.ToLowerInvariant() -or $Entry.Selector -cnotmatch '^[a-z0-9.-]+(/[a-z0-9._-]+)+$' -or $hostName.StartsWith('.') -or $hostName.EndsWith('.')) { return $false }
    }
    if ($Entry.Kind -eq "path") {
        $normalized = ConvertTo-AiMemoryRoutePath -Raw $Entry.Selector -HomePath $HomePath
        if ($null -eq $normalized -or $PathSeen.ContainsKey($normalized.Key)) { return $false }
        $PathSeen[$normalized.Key] = $true
        $Entry | Add-Member -NotePropertyName NormalizedPath -NotePropertyValue $normalized.Key -Force
        $Entry | Add-Member -NotePropertyName PathDepth -NotePropertyValue $normalized.Depth -Force
    }
    $Entry | Add-Member -NotePropertyName Workspace -NotePropertyValue $workspace -Force
    $Entry | Add-Member -NotePropertyName Project -NotePropertyValue $project -Force
    $Entry | Add-Member -NotePropertyName Style -NotePropertyValue $style -Force
    $Entry | Add-Member -NotePropertyName Aliases -NotePropertyValue $aliases -Force
    $null = $Entries.Add($Entry)
    return $true
}

function Get-AiMemoryHomeRoute {
    param([string] $File, [string] $Cwd, [string] $Identity)
    if (-not (Test-Path $File -PathType Leaf)) { return $null }
    try {
        $stream = [IO.File]::OpenRead($File)
        try {
            if ($stream.Length -gt 65536) { return "invalid" }
            $bytes = New-Object byte[] ([int]$stream.Length)
            $offset = 0
            while ($offset -lt $bytes.Length) {
                $read = $stream.Read($bytes, $offset, $bytes.Length - $offset)
                if ($read -eq 0) { break }
                $offset += $read
            }
            if ($offset -ne $bytes.Length) { return "invalid" }
            $text = (New-Object Text.UTF8Encoding($false, $true)).GetString($bytes)
        } finally {
            $stream.Dispose()
        }
        if (-not [regex]::IsMatch($text, '(?m)^\s*(?:\[routes|routes\s*=|routes\.|route_)')) { return $null }
        $entries = [Collections.Generic.List[object]]::new()
        $rawSeen = @{}
        $pathSeen = @{}
        $current = $null
        $userHome = Get-AiMemoryUserHome
        foreach ($line in ($text -split "`r?`n")) {
            $trimmed = $line.Trim()
            $header = [regex]::Match($trimmed, '^\[routes\.(identity|path)\."([^"\\]+)"\]$')
            if ($header.Success) {
                if (-not (Add-AiMemoryHomeRouteEntry -Entry $current -Entries $entries -PathSeen $pathSeen -HomePath $userHome)) { return "invalid" }
                $selector = $header.Groups[2].Value
                if ($entries.Count -ge 64 -or [Text.Encoding]::UTF8.GetByteCount($selector) -gt 512 -or $rawSeen.ContainsKey($selector)) { return "invalid" }
                $rawSeen[$selector] = $true
                $current = [pscustomobject]@{ Kind=$header.Groups[1].Value; Selector=$selector; Fields=@{} }
                continue
            }
            if ($trimmed.StartsWith("[routes") -or $trimmed.StartsWith("routes.") -or $trimmed -match '^routes\s*=' -or ($trimmed.StartsWith("route_") -and $null -eq $current)) { return "invalid" }
            if ($trimmed.StartsWith("[")) {
                if (-not (Add-AiMemoryHomeRouteEntry -Entry $current -Entries $entries -PathSeen $pathSeen -HomePath $userHome)) { return "invalid" }
                $current = $null
                continue
            }
            if ($null -eq $current) {
                if ($trimmed -match '^(workspace|project|project_strategy|drop_subagent_captures|identity|identity_style|server)\s*=' -and $trimmed -notmatch '^[A-Za-z0-9_]+\s*=\s*"[^"]*"$') { return "invalid" }
                continue
            }
            if (-not $trimmed -or $trimmed.StartsWith("#")) { continue }
            $field = [regex]::Match($trimmed, '^(route_workspace|route_project|route_identity_style|route_aliases)\s*=\s*(.*)$')
            if (-not $field.Success -or $current.Fields.ContainsKey($field.Groups[1].Value)) { return "invalid" }
            $name = $field.Groups[1].Value
            $raw = $field.Groups[2].Value
            if ($raw.Contains("\")) { return "invalid" }
            if ($name -eq "route_aliases") {
                if ([Text.Encoding]::UTF8.GetByteCount($raw) -gt 512 -or $raw -notmatch '^\[[^\]]*\]$') { return "invalid" }
                $current.Fields[$name] = $raw
            } else {
                $value = [regex]::Match($raw, '^"([^"\\]+)"$')
                if (-not $value.Success -or [Text.Encoding]::UTF8.GetByteCount($value.Groups[1].Value) -gt 512) { return "invalid" }
                $current.Fields[$name] = $value.Groups[1].Value
            }
        }
        if (-not (Add-AiMemoryHomeRouteEntry -Entry $current -Entries $entries -PathSeen $pathSeen -HomePath $userHome)) { return "invalid" }
        $exact = @($entries | Where-Object { $_.Kind -eq "identity" -and $_.Selector -ceq $Identity })
        if ($exact.Count -eq 1) { return $exact[0] }
        $target = ConvertTo-AiMemoryRoutePath -Raw $Cwd -HomePath (Get-AiMemoryUserHome) -Bounded $false
        if ($null -eq $target) { return "invalid" }
        $routeMatches = [Collections.Generic.List[object]]::new()
        foreach ($entry in $entries) {
            if ($entry.Kind -ne "path") { continue }
            if ($target.Key -eq $entry.NormalizedPath -or $target.Key.StartsWith($entry.NormalizedPath + "/", [StringComparison]::Ordinal)) {
                $entry | Add-Member -NotePropertyName MatchLength -NotePropertyValue $entry.PathDepth -Force
                $null = $routeMatches.Add($entry)
            }
        }
        if ($routeMatches.Count) { return @($routeMatches | Sort-Object MatchLength -Descending)[0] }
    } catch {
        return "invalid"
    }
    return $null
}

function Get-AiMemoryTomlFlag {
    param([string] $File, [string] $Key)
    if (-not (Test-Path $File -PathType Leaf)) { return $null }
    foreach ($line in Get-Content $File) {
        $m = [regex]::Match($line, "^\s*$Key\s*=\s*`"?([^`"#]*)`"?\s*(#.*)?$")
        if ($m.Success) { return $m.Groups[1].Value.Trim() }
    }
    return $null
}

# A resolved marker's `[profile]` flag as the explicit value the hook sends:
# "0" for a falsy value, "1" for anything else, an absent key included.
# Parity with `profile_flag_value` in hook_capture.rs.
function ConvertTo-AiMemoryProfileFlag {
    param([string] $Value)
    if ($Value -and (@("0", "false", "no", "off") -contains $Value.Trim().ToLowerInvariant())) { return "0" }
    return "1"
}

function Test-AiMemoryTruthy {
    param([string] $Value)
    if (-not $Value) { return $false }
    return @("1", "true", "yes", "on") -contains $Value.Trim().ToLowerInvariant()
}

# Build `&briefing=<v>[&briefing_budget=<v>]` from the `[briefing]` section
# of the marker walked up from $Cwd, for the opt-in $Key (by default
# inject_on_session_start; a subagent start passes inject_on_subagent_start).
# Returns "" when the repo did not opt in. Used by agents that deliver the
# compiled project brief once per session (kimi-code, via the first user
# prompt — kimi discards SessionStart hook stdout) so the server does not
# recompose the brief on every request. The char-budget clamp is server-side.
function Get-AiMemoryBriefingQuery {
    param([string] $Cwd, [string] $Key = "inject_on_session_start")
    if (-not $Cwd) { return "" }
    $marker = Get-AiMemoryMarkerToml -Cwd $Cwd
    if (-not $marker) { return "" }
    $briefing = Get-AiMemoryTomlFlag -File $marker -Key $Key
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

# Normalise a git remote URL into a repository identity, or $null when it
# names no network-reachable repository (a local path, a bare host). Port of
# `normalize_remote_url` in crates/ai-memory-core/src/repository_identity.rs,
# checked against the same fixture
# (crates/ai-memory-core/fixtures/remote_identity_cases.json) so the two
# cannot drift. Credentials are dropped here, on the host.
function ConvertTo-AiMemoryRepositoryIdentity {
    param([string] $Url)
    if ($null -eq $Url) { return $null }
    $raw = $Url.Trim()
    if (-not $raw) { return $null }
    $scheme = $null
    $hasScheme = $false
    $rest = $raw
    $idx = $raw.IndexOf("://")
    if ($idx -ge 0) {
        $hasScheme = $true
        $scheme = $raw.Substring(0, $idx).ToLowerInvariant()
        $rest = $raw.Substring($idx + 3)
    }
    if ($scheme -eq "file") { return $null }
    # Credentials: everything up to the LAST `@` before the path.
    $pathStart = $rest.IndexOf("/")
    if ($pathStart -lt 0) { $pathStart = $rest.Length }
    $at = $rest.Substring(0, $pathStart).LastIndexOf("@")
    $hp = if ($at -ge 0) { $rest.Substring($at + 1) } else { $rest }
    if ($hasScheme) {
        $slash = $hp.IndexOf("/")
        if ($slash -ge 0) {
            $h = $hp.Substring(0, $slash)
            $p = $hp.Substring($slash + 1)
        } else {
            $h = $hp
            $p = ""
        }
        $colon = $h.LastIndexOf(":")
        if ($colon -ge 0 -and $h.Substring($colon + 1) -match '^[0-9]*$') {
            $h = $h.Substring(0, $colon)
        }
        if (-not $h -or -not $p) { return $null }
    } else {
        # scp-like `host:path`, or a filesystem path: a `:` before any `/`.
        $colon = $hp.IndexOf(":")
        if ($colon -lt 0) { return $null }
        $slash = $hp.IndexOf("/")
        if ($slash -ge 0 -and $slash -lt $colon) { return $null }
        $h = $hp.Substring(0, $colon)
        $p = $hp.Substring($colon + 1)
        # A one-character host is a Windows drive letter.
        if ($h.Length -le 1 -or -not $p) { return $null }
        if ($p.Contains("\")) { return $null }
    }
    $id = "$h/$p".ToLowerInvariant().TrimEnd('/')
    if ($id.EndsWith(".git")) { $id = $id.Substring(0, $id.Length - 4) }
    $id = $id.TrimEnd('/')
    while ($id.Contains("//")) { $id = $id.Replace("//", "/") }
    if (-not $id -or -not $id.Contains("/")) { return $null }
    return $id
}

# Send a style with every valid git-remote identity. Explicit marker/home-route
# values win; omission or invalid input sends `path`. A server receiving a truly
# omitted field keeps legacy `host_path` behavior for old clients.
function Get-AiMemoryIdentityStyleQuery {
    param([string] $Style)
    if ($Style -and $Style.Trim() -ceq "host_path") { return "&identity_style=host_path" }
    return "&identity_style=path"
}

# `&identity=<v>&identity_src=<rung>` for the checkout at $Cwd, or "".
# Mirrors `repository_identity` in hook_capture.rs: an explicit marker
# `identity` is sent; a declared `project` outranks the remote and routes by
# name, so git is not consulted; otherwise the `upstream` remote, else `origin`.
# $Style (the marker's `identity_style`) is forwarded only with a remote
# identity.
function Get-AiMemoryIdentityQuery {
    param([string] $Cwd, [string] $Explicit, [string] $Project, [string] $Style, [string] $Aliases)
    if (-not $Aliases -and $Explicit -and $Explicit.Trim()) {
        $value = $Explicit.Trim().ToLowerInvariant()
        return "&identity=$([uri]::EscapeDataString($value))&identity_src=explicit"
    }
    if (-not $Aliases -and $Project -and $Project.Trim()) { return "" }
    if (-not $Cwd) { return "" }
    if (-not (Get-Command git -ErrorAction SilentlyContinue)) { return "" }
    foreach ($name in @("upstream", "origin")) {
        $url = (& git -C $Cwd config --get "remote.$name.url" 2>$null)
        if (-not $url) { continue }
        $value = ConvertTo-AiMemoryRepositoryIdentity -Url ([string]$url)
        if ($value) {
            return "&identity=$([uri]::EscapeDataString($value))&identity_src=git_remote" + (Get-AiMemoryIdentityStyleQuery -Style $Style)
        }
    }
    return ""
}

function Get-AiMemoryMarkerQuery {
    param([string] $Cwd)
    if (-not $Cwd) { return "" }
    $qs = "&cwd=$([uri]::EscapeDataString($Cwd))"
    $ws = $null
    $proj = $null
    $strategy = $null
    $dropSubagent = $null
    $defaultGlobal = $null
    # Provenance of $proj, forwarded as `project_src` so the server can tell a
    # deliberate marker rescope from a host-derived repo-root name. Only the
    # latter may yield to session-sticky attribution (#394).
    $projSrc = $null
    $explicitIdentity = $null
    $identityStyle = $null
    $profileContribute = $null
    $profileConsume = $null
    $handoffOff = $false
    $aliases = $null
    $identityQuery = ""
    $homeRouted = $false
    $marker = Get-AiMemoryMarkerToml -Cwd $Cwd
    $homeMarker = if (Get-AiMemoryUserHome) { Join-Path (Get-AiMemoryUserHome) ".ai-memory.toml" } else { $null }
    if (-not $marker -or $marker -eq $homeMarker) {
        $routeIdentity = Get-AiMemoryRemoteIdentity -Cwd $Cwd
        $route = Get-AiMemoryHomeRoute -File $homeMarker -Cwd $Cwd -Identity $routeIdentity
        if ($route -eq "invalid") { return $null }
        if ($route) {
            $ws = $route.Workspace
            $proj = $route.Project
            $projSrc = "marker"
            $homeRouted = $true
            $identityStyle = $route.Style
            $aliases = $route.Aliases
            if ($routeIdentity) {
                $identityQuery = "&identity=$([uri]::EscapeDataString($routeIdentity))&identity_src=git_remote" + (Get-AiMemoryIdentityStyleQuery -Style $identityStyle)
            }
            $marker = $homeMarker
        }
    }
    if ($marker) {
        if (-not $homeRouted) {
            $ws = Get-AiMemoryTomlKey -File $marker -Key "workspace"
            $proj = Get-AiMemoryTomlKey -File $marker -Key "project"
            $strategy = Get-AiMemoryTomlKey -File $marker -Key "project_strategy"
            $explicitIdentity = Get-AiMemoryTomlKey -File $marker -Key "identity"
            $identityStyle = Get-AiMemoryTomlKey -File $marker -Key "identity_style"
            $aliases = Get-AiMemoryTomlAliases -File $marker
        }
        $dropSubagent = Get-AiMemoryTomlKey -File $marker -Key "drop_subagent_captures"
        $defaultGlobal = Get-AiMemoryTomlFlag -File $marker -Key "default_global"
        # `[profile] contribute` / `consume`, quoted or bare; the server
        # decides truthiness and keeps both on unless explicitly falsy.
        # Always explicit once a marker resolved (0 when falsy, else 1): removing
        # the key re-enables; no marker sends nothing and the server keeps
        # what it stored.
        $profileContribute = ConvertTo-AiMemoryProfileFlag (Get-AiMemoryTomlFlag -File $marker -Key "contribute")
        $profileConsume = ConvertTo-AiMemoryProfileFlag (Get-AiMemoryTomlFlag -File $marker -Key "consume")
        # `[handoff] deliver_on_session_start = false`: sent only when explicitly falsy.
        $handoffOff = (ConvertTo-AiMemoryProfileFlag (Get-AiMemoryTomlFlag -File $marker -Key "deliver_on_session_start")) -eq "0"
        if ($proj) { $projSrc = "marker" }
    }
    # Before repo-root can fill $proj: a repo-root name is an inference, while
    # the identity chain's declared-project rung means a name in the marker.
    if (-not $identityQuery) { $identityQuery = Get-AiMemoryIdentityQuery -Cwd $Cwd -Explicit $explicitIdentity -Project $proj -Style $identityStyle -Aliases $aliases }
    # Install-time default baked into the hook command by
    # `install-hooks --project-strategy` fills the strategy only when no marker
    # pins one. Explicit project/identity and home routing win; otherwise a
    # valid remote identity wins and repo-root is the remote-less fallback.
    if (-not $strategy -and $env:AI_MEMORY_PROJECT_STRATEGY) {
        $strategy = $env:AI_MEMORY_PROJECT_STRATEGY
    }
    # repo-root must be resolved host-side (the server may not see this checkout);
    # only when no explicit project is pinned. Explicit project always wins.
    if (-not $proj -and ($strategy -eq "repo-root" -or $strategy -eq "repo_root")) {
        $proj = Get-AiMemoryRepoRootProject -Cwd $Cwd
        if ($proj) { $projSrc = "repo-root" }
    }
    if ($ws) { $qs += "&workspace=$([uri]::EscapeDataString($ws))" }
    if ($proj) { $qs += "&project=$([uri]::EscapeDataString($proj))" }
    if ($projSrc) { $qs += "&project_src=$([uri]::EscapeDataString($projSrc))" }
    if ($strategy) { $qs += "&project_strategy=$([uri]::EscapeDataString($strategy))" }
    $qs += $identityQuery
    if ($aliases) {
        if (-not $proj -or -not $identityQuery.Contains("identity_src=git_remote")) { $aliases = if ($homeRouted) { $null } else { "invalid" } }
        if ($aliases) { $qs += "&aliases=$([uri]::EscapeDataString($aliases))" }
    }
    # Per-project drop_subagent_captures opt-in: forward to the server, which
    # interprets truthiness (1/true/...) and scopes the drop to this project.
    if ($dropSubagent) { $qs += "&drop_subagent=$([uri]::EscapeDataString($dropSubagent))" }
    if ($defaultGlobal) { $qs += "&default_global=$([uri]::EscapeDataString($defaultGlobal))" }
    if ($profileContribute) { $qs += "&profile_contribute=$([uri]::EscapeDataString($profileContribute))" }
    if ($profileConsume) { $qs += "&profile_consume=$([uri]::EscapeDataString($profileConsume))" }
    if ($handoffOff) { $qs += "&handoff=0" }
    return $qs
}

function Get-AiMemoryStateDir {
    if ($env:AI_MEMORY_DATA_DIR) { return $env:AI_MEMORY_DATA_DIR }
    if ($env:XDG_DATA_HOME) { return (Join-Path $env:XDG_DATA_HOME "ai-memory") }
    if ($env:LOCALAPPDATA) { return (Join-Path $env:LOCALAPPDATA "ai-memory") }
    if ($env:HOME) { return (Join-Path $env:HOME ".local/share/ai-memory") }
    return ".ai-memory"
}

# --- offline spool -----------------------------------------------------
# Parity with the shell bundle (`ai_memory_spool_event` /
# `ai_memory_drain_spool` / `ai_memory_kick_drain` in hooks/_lib.sh): an
# undelivered event is written to `<state dir>/hook-spool/` in the same
# on-disk contract `ai-memory hook-drain` reads — same
# `<ms:013>-<pid>-<seq:016x>.json` file name, same `SpoolEntry` JSON
# fields, tmp+rename — so the native drainer, the shell bundle, and this
# bundle consume one another's entries on a shared data dir. A down server
# then costs latency instead of the event.

function Get-AiMemorySpoolDir {
    return (Join-Path (Get-AiMemoryStateDir) "hook-spool")
}

# Best-effort chmod for the pwsh-on-Unix case: the shell and native writers
# keep the spool 0700/0600 (the spool holds private capture until it
# drains). `chmod` is absent or a no-op on Windows, where the profile-scoped
# data dir already restricts access.
function Set-AiMemoryPrivateMode {
    param([string] $Path, [string] $Mode)
    if (Get-Command chmod -ErrorAction SilentlyContinue) {
        & chmod $Mode $Path 2>$null
    }
}

# Persist one undelivered event. Like every capture path here this is
# best-effort: each failure is swallowed so a hook never fails because of
# the spool.
function Write-AiMemorySpoolEvent {
    param([string] $Url, [string] $Body)
    try {
        $dir = Get-AiMemorySpoolDir
        New-Item -ItemType Directory -Force -Path $dir -ErrorAction Stop | Out-Null
        Set-AiMemoryPrivateMode -Path $dir -Mode "700"
        if (-not $script:AiMemorySpoolSeq) { $script:AiMemorySpoolSeq = 0 }
        $script:AiMemorySpoolSeq = $script:AiMemorySpoolSeq + 1
        $now = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
        # Same name shape as the shell/native writers, so lexical order is
        # enqueue order for every reader of the directory.
        $name = "{0:D13}-{1}-{2:x16}.json" -f $now, $PID, $script:AiMemorySpoolSeq
        $entry = [ordered]@{
            url = $Url
            body = $Body
            created_ms = $now
        }
        if ($env:AI_MEMORY_AUTH_TOKEN) {
            # The credential this hook's own POST would have carried, stored
            # in the 0600 entry rather than on any command line.
            $entry["auth_mode"] = "static"
            $entry["token"] = $env:AI_MEMORY_AUTH_TOKEN
        } else {
            $entry["auth_mode"] = "none"
        }
        $entry["attempts"] = 0
        # PS 5.1 escapes a few ASCII characters (`<`, `&`, …) as \uXXXX. The
        # emitted JSON stays a valid SpoolEntry; the shell drain leaves such
        # entries to `ai-memory hook-drain` by design, and the native
        # drainer and ConvertFrom-Json decode them.
        $json = ConvertTo-Json -InputObject $entry -Compress
        $tmp = Join-Path $dir "$name.tmp"
        $final = Join-Path $dir $name
        # WriteAllText is UTF-8 without a BOM in both engines, which the
        # native drainer's parser requires.
        [IO.File]::WriteAllText($tmp, $json)
        Set-AiMemoryPrivateMode -Path $tmp -Mode "600"
        Move-Item -Force -Path $tmp -Destination $final -ErrorAction Stop
    } catch {
        if ($tmp -and (Test-Path $tmp -ErrorAction SilentlyContinue)) {
            Remove-Item -Force -LiteralPath $tmp -ErrorAction SilentlyContinue
        }
    }
}

# Deliver the queued backlog, oldest first (name order is enqueue order).
# Mirrors `ai_memory_drain_spool`: bounded by count so a pass never becomes
# an unbounded upload, a 2xx or a 4xx retires the entry (delivered, or a
# permanent rejection that must not be retried), and anything else stops
# the pass and keeps the remainder for the next one. The per-entry timeout
# is this bundle's own POST budget. ConvertFrom-Json reads every escape
# either serializer emits, so entries written by the shell bundle or the
# native binary drain here too, except OIDC entries that need native auth.json
# credential resolution.
function Invoke-AiMemoryDrainSpool {
    param([int] $Max = 64)
    try {
        $dir = Get-AiMemorySpoolDir
        if (-not (Test-Path $dir -PathType Container)) { return }
        # -Filter alone can match 8.3 short names on Windows; the extension
        # re-check keeps a writer's in-flight `*.json.tmp` out of the pass.
        $files = @(
            Get-ChildItem -LiteralPath $dir -Filter "*.json" -File -ErrorAction Stop |
                Where-Object { $_.Extension -eq ".json" } |
                Sort-Object Name
        )
    } catch {
        return
    }
    $count = 0
    foreach ($file in $files) {
        if ($count -ge $Max) { break }
        $count = $count + 1
        try {
            $entry = [IO.File]::ReadAllText($file.FullName) | ConvertFrom-Json -ErrorAction Stop
        } catch {
            continue
        }
        # OIDC entries have no inline bearer. The native drain resolves/refreshes
        # auth.json; an anonymous POST here can retire the event without it.
        if ($entry.auth_mode -eq "oidc") { continue }
        if (-not $entry.url) { continue }
        $headers = @{}
        $headers["X-AI-Memory-Client"] = "hook-ps"
        if ($entry.auth_mode -eq "static" -and $entry.token) {
            $headers["Authorization"] = "Bearer $($entry.token)"
        }
        $code = 0
        $NoProxy = Get-AiMemoryNoProxy
        try {
            $Response = Invoke-WebRequest @NoProxy `
                -UseBasicParsing `
                -TimeoutSec 3 `
                -Method Post `
                -Uri ([string]$entry.url) `
                -Headers $headers `
                -ContentType "application/json" `
                -Body ([Text.Encoding]::UTF8.GetBytes([string]$entry.body))
            $code = Get-AiMemoryResponseStatus $Response
        } catch {
            $response = $_.Exception.Response
            if ($response) {
                try { $code = [int]$response.StatusCode } catch { $code = 0 }
            }
        }
        if ($code -eq 408 -or $code -eq 425 -or $code -eq 429) {
            break
        } elseif (($code -ge 200 -and $code -lt 300) -or ($code -ge 400 -and $code -lt 500)) {
            Remove-Item -Force -LiteralPath $file.FullName -ErrorAction SilentlyContinue
        } else {
            break
        }
    }
}

# Piggyback drain: a delivery that just succeeded proves the server is
# reachable, so flush the backlog behind it — detached, so the agent never
# waits, and a no-op when nothing is queued (every call on a healthy
# install).
#
# Detach idiom: this bundle had no background-job or Start-Process pattern
# to copy (each hook script is already its own short-lived process), so the
# kick re-runs the running engine on this same lib file in drain mode
# through System.Diagnostics.Process:
# - CreateNoWindow with UseShellExecute=$false never flashes a console
#   window on Windows and behaves identically under pwsh on Linux/macOS;
# - RedirectStandardOutput/Error keep the child off THIS hook's stdout
#   pipe — the agent reads that pipe to EOF, and a detached child holding
#   the inherited handle would stall the hook's own completion (the shell
#   bundle's `( ... >/dev/null 2>&1 &)` is that same protection). Drain
#   mode writes nothing, so the unread redirected pipes never fill.
function Invoke-AiMemoryKickDrain {
    try {
        $dir = Get-AiMemorySpoolDir
        if (-not (Test-Path $dir -PathType Container)) { return }
        $queued = @(
            Get-ChildItem -LiteralPath $dir -Filter "*.json" -File -ErrorAction Stop |
                Where-Object { $_.Extension -eq ".json" } |
                Select-Object -First 1
        )
        if (-not $queued.Count) { return }
        if (-not $script:AiMemoryLibFile) { return }
        # Re-invoke the engine already running this hook; fall back to any
        # PowerShell on PATH if the current process's binary cannot be
        # resolved.
        $engine = $null
        try { $engine = (Get-Process -Id $PID -ErrorAction Stop).Path } catch { }
        if (-not $engine) {
            $cmd = Get-Command pwsh -ErrorAction SilentlyContinue
            if (-not $cmd) { $cmd = Get-Command powershell -ErrorAction SilentlyContinue }
            if ($cmd) { $engine = $cmd.Source }
        }
        if (-not $engine) { return }
        $startInfo = New-Object System.Diagnostics.ProcessStartInfo
        $startInfo.FileName = $engine
        $startInfo.Arguments = '-NoProfile -ExecutionPolicy Bypass -File "{0}" --ai-memory-drain-spool 64' -f $script:AiMemoryLibFile
        $startInfo.UseShellExecute = $false
        $startInfo.CreateNoWindow = $true
        $startInfo.RedirectStandardOutput = $true
        $startInfo.RedirectStandardError = $true
        $null = [System.Diagnostics.Process]::Start($startInfo)
    } catch {
    }
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

# PowerShell 7 hands Invoke-WebRequest the HTTP_PROXY, HTTPS_PROXY and
# ALL_PROXY proxy even for a loopback URL, so a sandbox with an empty NO_PROXY
# (ai-jail) routes the hook to a proxy that answers 4xx. -NoProxy (PowerShell
# 6+) connects directly, as the native hook and the shell bundle do. Windows
# PowerShell 5.1 reads no environment proxy and has no -NoProxy.
#
# MaximumRedirection 0 matches the native hook (`Policy::none`) and the
# generated TypeScript integrations (`redirect: "error"`): a 3xx from an SSO
# portal, proxy or captive portal must not be followed, so a login page's 200
# cannot count as delivery, a 307/308 cannot replay the POST, and a 302 body
# cannot be injected as a handoff. Splat the result into every Invoke-WebRequest.
function Get-AiMemoryNoProxy {
    $splat = @{ MaximumRedirection = 0 }
    if ($PSVersionTable.PSVersion.Major -ge 6) { $splat['NoProxy'] = $true }
    return $splat
}

# Status of a completed Invoke-WebRequest. Windows PowerShell 5.1 can return
# a 3xx object instead of throwing when MaximumRedirection is 0; PowerShell 7
# throws. Mocks that omit StatusCode keep the historical "did not throw means
# 2xx" default so existing tests stay valid.
function Get-AiMemoryResponseStatus {
    param($Response, [int] $Default = 200)
    if ($null -eq $Response) { return $Default }
    $code = $Response.StatusCode
    if ($null -eq $code) { return $Default }
    try { return [int]$code } catch { return $Default }
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

# Parity with `antigravity_boundary` in the native hook: only SessionStart
# omits invocationNum, a PreInvocation is a start boundary at invocation zero,
# and a missing or malformed counter on a PreInvocation fails closed.
function Test-AiMemoryAntigravityStartBoundary {
    param([string] $Payload, [switch] $PreInvocation)
    try {
        $Parsed = $Payload | ConvertFrom-Json
        $Property = $Parsed.PSObject.Properties["invocationNum"]
        if ($null -eq $Property) { return (-not $PreInvocation) }
        $Value = $Property.Value
        return (($Value -is [int] -or $Value -is [long]) -and [long]$Value -eq 0)
    } catch {
        return $false
    }
}

# Parity with `ai_memory_capture_owned_externally` in hooks/_lib.sh:
# AI_MEMORY_CAPTURE_OWNER names an external producer of this session's capture
# events. Non-blank claims ownership; unset, empty and whitespace-only keep
# capture on. The value is only ever tested, never written to output.
function Test-AiMemoryCaptureOwnedExternally {
    return (-not [string]::IsNullOrWhiteSpace($env:AI_MEMORY_CAPTURE_OWNER))
}

# Parity with `payload_is_subagent` in the native router
# (`commands/hook.rs`): a subagent/child payload must not take the parent
# session's handoff. True when any known child-session marker key holds a
# non-empty string.
function Test-AiMemorySubagentPayload {
    param([object] $ParsedPayload)
    if ($null -eq $ParsedPayload) { return $false }
    foreach ($Name in @(
        "subagentType", "subagent_type", "agent_type", "agent_id", "parentSessionId"
    )) {
        $Value = $ParsedPayload.$Name
        if ($Value -is [string] -and $Value.Trim().Length -gt 0) { return $true }
    }
    return $false
}

# Parity with `clip_chars` in the native router: Grok's additionalContext
# is capped (10 000 chars) so an oversized handoff cannot flood the model
# context through the script fallback either.
function Clip-AiMemoryChars {
    param([string] $Text, [int] $MaxChars)
    if ($Text.Length -le $MaxChars) { return $Text }
    $keep = [Math]::Max(0, $MaxChars - 12)
    return ($Text.Substring(0, $keep) + "`n[truncated]")
}

function Invoke-AiMemoryHook {
    param(
        [Parameter(Mandatory = $true)] [string] $Event,
        [Parameter(Mandatory = $true)] [string] $Agent,
        [switch] $FetchHandoff,
        # Copilot CLI requires SessionStart output as a top-level
        # `{ "additionalContext": ... }` envelope rather than Claude Code's
        # nested hookSpecificOutput shape.
        [switch] $CopilotCliSessionStartOutput,
        # Antigravity SessionStart output: a fetched handoff becomes an
        # `injectSteps` system message. With -AntigravityPreInvocation the
        # payload is a PreInvocation, a start boundary only at invocation 0.
        [switch] $AntigravityStartOutput,
        # Claude Code prepends a SessionStart hook's stdout to the resuming
        # session. Bare text does not start with "{", so it logs every start as
        # "Hook output does not start with {, treating as plain text" and the
        # handoff lands unstructured. The POSIX peers of these bundles already
        # emit the envelope; this is their PowerShell half.
        [switch] $SessionStartEnvelope,
        [switch] $AntigravityPreInvocation,
        # Deliver the `[briefing]` compiled project brief on the FIRST
        # handoff fetch of a session only (kimi-code's user-prompt path:
        # kimi discards SessionStart hook stdout, so the brief rides the
        # first prompt — parity with Claude's once-per-SessionStart brief).
        # Later fetches keep the handoff but drop the briefing params so the
        # server does not recompose the brief per prompt.
        [switch] $BriefingOncePerSession,
        # Grok PostToolUse: wrap a fetched handoff as additionalContext and
        # only fetch once per session. Other events must not set this.
        [switch] $GrokPostTool,
        # `claude-code` SubagentStart: hand the subagent the `[briefing]
        # inject_on_subagent_start` brief from the read-only `/brief`.
        [switch] $SubagentStartBrief
    )

    $Server = if ($env:AI_MEMORY_HOOK_URL) { $env:AI_MEMORY_HOOK_URL } else { "http://127.0.0.1:49374" }
    $Payload = Read-AiMemoryStdin
    if ($AntigravityStartOutput -and -not (Test-AiMemoryAntigravityStartBoundary -Payload $Payload -PreInvocation:$AntigravityPreInvocation)) {
        [Console]::Out.Write("{}")
        return
    }
    # invocationNum restarts at 0 on every user turn: only a conversation's
    # first start boundary may post session-start and claim a handoff.
    if ($AntigravityStartOutput) {
        $Conversation = $null
        try { $Conversation = [string](($Payload | ConvertFrom-Json).conversationId) } catch { }
        if ($Conversation -and $Conversation.Trim()) {
            $StartMarker = Get-AiMemoryBriefedFile -Key ("agy-start-" + $Conversation.Trim())
            if (Test-Path -LiteralPath $StartMarker) {
                [Console]::Out.Write("{}")
                return
            }
            Set-AiMemoryBriefed -Path $StartMarker
        }
    }
    # Assistant/Stop capture (#196) is native-only. Scoped to agents whose Stop
    # payload may carry the field, so unrelated tool output containing the literal
    # string is unaffected. If a Stop payload still carries the raw field, drop the
    # whole event rather than POST it verbatim (the fallback cannot sanitize it).
    if (($Agent -eq "claude-code" -or $Agent -eq "dsh") -and $Event -eq "stop" -and $Payload -and $Payload.Contains('"last_assistant_message"')) {
        return
    }
    $Cwd = Resolve-AiMemoryCwd -Payload $Payload -Agent $Agent
    if (Test-AiMemoryServerRouted -Cwd $Cwd) {
        if ($AntigravityStartOutput) { [Console]::Out.Write("{}") }
        return
    }
    $QS = Get-AiMemoryMarkerQuery -Cwd $Cwd
    if ($Cwd -and $null -eq $QS) {
        if ($AntigravityStartOutput -or $GrokPostTool) { [Console]::Out.Write("{}") }
        return
    }
    if ($env:AI_MEMORY_RUN_ID) {
        $QS += "&managed_run=$([Uri]::EscapeDataString($env:AI_MEMORY_RUN_ID))"
    }
    $SessionQS = ""
    if ($Agent -eq "devin") {
        $SessionQS = Get-AiMemorySessionIdQuery -Agent $Agent -Event $Event
    }
    # Every request names this client: an unauthenticated server reached
    # through a non-loopback host name refuses a handoff fetch without the
    # header, because a web page cannot add it to a request it makes.
    $Headers = @{}
    $Headers["X-AI-Memory-Client"] = "hook-ps"

    if ($env:AI_MEMORY_AUTH_TOKEN) {
        $Headers["Authorization"] = "Bearer $env:AI_MEMORY_AUTH_TOKEN"
    }

    # Capture producer path, with the shell bundle's offline-spool parity
    # (hooks/_lib.sh): a 2xx kicks a detached backlog drain, a terminal 4xx
    # is a permanent rejection and is dropped, and anything undeliverable
    # (connection failure, timeout, 5xx) is spooled for the drain to retry.
    # Session identity and the handoff/briefing GET below are delivery, so
    # an external owner leaves them, and the stdout contract, alone.
    if (-not (Test-AiMemoryCaptureOwnedExternally)) {
        $BodyBytes = [Text.Encoding]::UTF8.GetBytes($Payload)
        # The idempotency key is minted before the POST and rides any spooled
        # replay too, so the server can discard a replay whose original
        # response was lost after the observation committed — the same
        # reason the shell bundle mints `ai_memory_ingest_key`. `ps` plus a
        # GUID's hex fits the server's key grammar.
        $HookUrl = "$Server/hook?event=$Event&agent=$Agent$QS$SessionQS&ingest_key=ps$([Guid]::NewGuid().ToString('N').Substring(0, 16))"
        $NoProxy = Get-AiMemoryNoProxy
        try {
            $Response = Invoke-WebRequest @NoProxy `
                -UseBasicParsing `
                -TimeoutSec 3 `
                -Method Post `
                -Uri $HookUrl `
                -Headers $Headers `
                -ContentType "application/json; charset=utf-8" `
                -Body $BodyBytes
            $Status = Get-AiMemoryResponseStatus $Response
            if ($Status -ge 200 -and $Status -lt 300) {
                Invoke-AiMemoryKickDrain
            } elseif ($Status -lt 400 -or $Status -ge 500 -or $Status -eq 408 -or $Status -eq 425 -or $Status -eq 429) {
                Write-AiMemorySpoolEvent -Url $HookUrl -Body $Payload
            }
        } catch {
            $Status = 0
            if ($_.Exception.Response) {
                try { $Status = [int]$_.Exception.Response.StatusCode } catch { $Status = 0 }
            }
            # 4xx = permanent rejection (not retried); 408/425/429 (transient timeout
            # / saturation) and everything else that failed to deliver is spooled.
            if ($Status -lt 400 -or $Status -ge 500 -or $Status -eq 408 -or $Status -eq 425 -or $Status -eq 429) {
                Write-AiMemorySpoolEvent -Url $HookUrl -Body $Payload
            }
        }
    }
    if ($Agent -eq "devin" -and $Event -eq "session-end") {
        Clear-AiMemorySessionId -Agent $Agent
    }

    # A subagent runs inside its parent's session, so its brief comes from the
    # read-only `/brief`, never `/handoff`, whose fetch would claim the
    # parent's baton (parity with the native hook and subagent-start.sh). A
    # Cursor payload gets none: its handling of the envelope is unverified.
    # No brief prints nothing, as this hook always did.
    if ($SubagentStartBrief) {
        if ($Cwd -and -not $Payload.Contains('"cursor_version"')) {
            $BriefQS = Get-AiMemoryBriefingQuery -Cwd $Cwd -Key "inject_on_subagent_start"
            if ($BriefQS) {
                $NoProxy = Get-AiMemoryNoProxy
                try {
                    $Response = Invoke-WebRequest @NoProxy `
                        -UseBasicParsing `
                        -TimeoutSec 2 `
                        -Uri "$Server/brief?agent=$Agent$QS$BriefQS" `
                        -Headers $Headers
                    $Status = Get-AiMemoryResponseStatus $Response
                    if ($Status -ge 200 -and $Status -lt 300 -and $null -ne $Response -and $Response.Content) {
                        $Wrapped = @{
                            hookSpecificOutput = @{
                                hookEventName = "SubagentStart"
                                additionalContext = [string]$Response.Content
                            }
                        }
                        [Console]::Out.Write(($Wrapped | ConvertTo-Json -Depth 5 -Compress))
                    }
                } catch {
                }
            }
        }
        return
    }

    # `GET /handoff` claims the newest open baton in whatever project the
    # server resolves. A payload that names no directory would send no `cwd`,
    # the server would fall back to its default or the caller's last active
    # project, and the claim could consume another repository's handoff.
    # Without a directory the handoff stays open for memory_handoff_accept
    # (parity with the native hook and `ai_memory_get_handoff`).
    if ($FetchHandoff -and $Cwd) {
        $NativeSessionQS = ""
        $NativeSessionId = $null
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
        $Shown = $null
        if ($GrokPostTool) {
            if (Test-AiMemorySubagentPayload $ParsedPayload) {
                # A child session must not accept the parent handoff (and the
                # GET is destructive), so bail before any fetch. Parity with
                # the native router and the shell `post-tool-use.sh` gate.
                [Console]::Out.Write("{}")
                return
            }
            $ShownKey = [string]$NativeSessionId
            if (-not $ShownKey) {
                # Stable fallback (parity with the shell bundle's
                # `cksum("grok:$CWD")`): hash agent+cwd, never the process id —
                # each hook invocation is a new process, so a `$PID`-keyed
                # marker would never match and the destructive GET would run
                # on every tool call.
                $Sha = [System.Security.Cryptography.SHA256]::Create()
                $Bytes = $Sha.ComputeHash([System.Text.Encoding]::UTF8.GetBytes("grok:$Cwd"))
                $ShownKey = "grok-post-" + (($Bytes | ForEach-Object { $_.ToString("x2") }) -join "").Substring(0, 16)
            }
            $Shown = Get-AiMemoryBriefedFile -Key "post-$ShownKey"
            if (Test-Path $Shown -PathType Leaf) {
                [Console]::Out.Write("{}")
                return
            }
        }
        # Once-per-session briefing gate. Marker files are created only for
        # repositories that opt in. Prefer the native session id when Kimi
        # supplies one; otherwise use a stable hash of agent+cwd.
        $BriefQS = ""
        $BriefFile = $null
        $ProfileFile = $null
        $ProfileQS = ""
        if ($BriefingOncePerSession) {
            # The cross-project profile digest rides the first prompt only,
            # like the brief, but is not tied to the [briefing] opt-in.
            $ProfileKey = [string]$NativeSessionId
            if (-not $ProfileKey) {
                $ProfileSha = [System.Security.Cryptography.SHA256]::Create()
                $ProfileBytes = $ProfileSha.ComputeHash([System.Text.Encoding]::UTF8.GetBytes("$Agent`n$Cwd"))
                $ProfileKey = (($ProfileBytes | ForEach-Object { $_.ToString("x2") }) -join "")
            }
            $ProfileFile = Get-AiMemoryBriefedFile -Key "profile-$ProfileKey"
            if (Test-Path $ProfileFile -PathType Leaf) {
                $ProfileQS = "&profile_digest=0"
            }
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
        if ($GrokPostTool -and -not $BriefQS) {
            $BriefQS = Get-AiMemoryBriefingQuery -Cwd $Cwd
        }
        # A session-start handoff GET carries the `[briefing]` opt-in on every
        # start, as in the native hook. Kiro keeps its own once-per-session gate.
        if ($Event -eq "session-start" -and -not $BriefingOncePerSession) {
            $BriefQS = Get-AiMemoryBriefingQuery -Cwd $Cwd
        }
        $NoProxy = Get-AiMemoryNoProxy
        try {
            $Response = Invoke-WebRequest @NoProxy `
                -UseBasicParsing `
                -TimeoutSec 2 `
                -Uri "$Server/handoff?agent=$Agent$QS$NativeSessionQS$BriefQS$ProfileQS" `
                -Headers $Headers
            $Status = Get-AiMemoryResponseStatus $Response
            if ($Status -ge 200 -and $Status -lt 300 -and $null -ne $Response -and $Response.Content) {
                if ($GrokPostTool) {
                    $Wrapped = @{
                        hookSpecificOutput = @{
                            hookEventName = "PostToolUse"
                            additionalContext = (Clip-AiMemoryChars ([string]$Response.Content) 10000)
                        }
                    }
                    [Console]::Out.Write(($Wrapped | ConvertTo-Json -Depth 5 -Compress))
                } elseif ($CopilotCliSessionStartOutput) {
                    $Payload = @{ additionalContext = $Response.Content }
                    [Console]::Out.Write(($Payload | ConvertTo-Json -Depth 5 -Compress))
                } elseif ($AntigravityStartOutput) {
                    # A system message stays in the conversation for later
                    # turns; an ephemeralMessage is dropped after the first.
                    $Payload = @{
                        injectSteps = @(@{ systemMessage = @{ systemMessage = $Response.Content } })
                    }
                    [Console]::Out.Write(($Payload | ConvertTo-Json -Depth 5 -Compress))
                } elseif ($SessionStartEnvelope) {
                    $Payload = @{
                        hookSpecificOutput = @{
                            hookEventName = "SessionStart"
                            additionalContext = [string]$Response.Content
                        }
                    }
                    [Console]::Out.Write(($Payload | ConvertTo-Json -Depth 5 -Compress))
                } else {
                    [Console]::Out.Write($Response.Content)
                }
            } elseif ($AntigravityStartOutput -or $GrokPostTool -or $CopilotCliSessionStartOutput -or $SessionStartEnvelope) {
                [Console]::Out.Write("{}")
            }
        } catch {
            if ($AntigravityStartOutput -or $GrokPostTool -or $CopilotCliSessionStartOutput -or $SessionStartEnvelope) {
                [Console]::Out.Write("{}")
            }
        }
        if ($Shown) {
            Set-AiMemoryBriefed -Path $Shown
        }
        # Mark the session as briefed only AFTER the GET completed —
        # success or error (fail-open: with the server down, re-sending the
        # brief-flagged request on every prompt would deliver nothing
        # anyway, and the one lost brief returns on the next session).
        if ($BriefFile) {
            Set-AiMemoryBriefed -Path $BriefFile
        }
        if ($ProfileFile) {
            Set-AiMemoryBriefed -Path $ProfileFile
        }
    } elseif ($AntigravityStartOutput -or $GrokPostTool -or $CopilotCliSessionStartOutput) {
        [Console]::Out.Write("{}")
    }
}

# This file's own path, captured at load time: top-level `$PSCommandPath` is
# the lib's full path whether a hook script dot-sources it or the engine
# runs it directly in drain mode, so `Invoke-AiMemoryKickDrain`'s child can
# be pointed back at exactly this file.
$script:AiMemoryLibFile = $PSCommandPath

# Detached-drain entry mode: `Invoke-AiMemoryKickDrain` re-runs this file as
# `powershell -NoProfile -ExecutionPolicy Bypass -File <lib>
# --ai-memory-drain-spool <max>`. The bespoke token cannot arrive from an
# agent (hook input is stdin JSON, and dot-sourcing passes no arguments),
# and this mode prints nothing and always exits 0, so a redirected detached
# child can never pollute or stall a hook's stdout.
if ($args -contains "--ai-memory-drain-spool") {
    $AiMemoryDrainMax = 64
    foreach ($AiMemoryArg in $args) {
        if ($AiMemoryArg -match '^[0-9]+$') { $AiMemoryDrainMax = [int]$AiMemoryArg }
    }
    $null = Invoke-AiMemoryDrainSpool -Max $AiMemoryDrainMax
    exit 0
}
