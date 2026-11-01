# Behavioral evidence for the PowerShell bundle's offline spool: the
# entry writer's on-disk contract (file name, SpoolEntry fields, no BOM,
# token handling) and a refused-connection drain pass that keeps the
# backlog, plus a stubbed mixed queue proving native OIDC entries stay queued.
# Pure functions plus a connection-refused loopback POST — no live server.
# Invoked by tests/hooks/test_lib.sh when pwsh is available;
# exits non-zero on any failed assertion.

param([Parameter(Mandatory = $true)] [string] $PsLib)

$ErrorActionPreference = "Stop"
. $PsLib

function Assert-True($value, $label) {
    if (-not $value) { throw "spool ps probe failed: $label" }
}

$root = Join-Path ([IO.Path]::GetTempPath()) ("am-spool-" + [Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $root | Out-Null
$env:AI_MEMORY_DATA_DIR = (Join-Path $root "data")
# Port 1 is reserved and never listens: connection refused, immediately.
$RefusedUrl = "http://127.0.0.1:1/hook?event=stop&agent=codex&ingest_key=ps0000000000000000"

# 1. The lib captured its own path so the detached drain child can be
#    pointed back at this exact file.
Assert-True ($script:AiMemoryLibFile -and ((Split-Path $script:AiMemoryLibFile -Leaf) -eq "ai-memory-hook.ps1")) "lib path captured"

# 2. An undelivered event lands in <data>/hook-spool with the shared name
#    shape and entry fields.
Write-AiMemorySpoolEvent -Url $RefusedUrl -Body '{"e":"outage"}'
$dir = Get-AiMemorySpoolDir
$files = @(Get-ChildItem -LiteralPath $dir -Filter "*.json" -File)
Assert-True ($files.Count -eq 1) "one entry written"
Assert-True ($files[0].Name -match '^[0-9]{13}-[0-9]+-[0-9a-f]{16}\.json$') "entry name is <ms>-<pid>-<seq>"
$bytes = [IO.File]::ReadAllBytes($files[0].FullName)
Assert-True ($bytes.Length -gt 1 -and $bytes[0] -ne 0xEF) "entry has no UTF-8 BOM"
$entry = [Text.Encoding]::UTF8.GetString($bytes) | ConvertFrom-Json
Assert-True ($entry.url -eq $RefusedUrl) "url round-trips"
Assert-True ($entry.body -eq '{"e":"outage"}') "body round-trips"
Assert-True ($entry.created_ms -ge 1000000000000) "created_ms is unix milliseconds"
Assert-True ($entry.auth_mode -eq "none") "no token means auth_mode none"
Assert-True ($null -eq $entry.token) "no token field when auth_mode is none"
Assert-True ($entry.attempts -eq 0) "attempts starts at 0"

# 3. The auth token rides the entry when set.
$env:AI_MEMORY_AUTH_TOKEN = "probe-bearer"
Write-AiMemorySpoolEvent -Url $RefusedUrl -Body '{"e":"auth"}'
$tokenEntry = Get-ChildItem -LiteralPath $dir -Filter "*.json" -File | Sort-Object Name | Select-Object -Last 1
$tokenParsed = [IO.File]::ReadAllText($tokenEntry.FullName) | ConvertFrom-Json
Assert-True ($tokenParsed.auth_mode -eq "static") "token entry is static auth"
Assert-True ($tokenParsed.token -eq "probe-bearer") "token round-trips"
Remove-Item Env:\AI_MEMORY_AUTH_TOKEN -ErrorAction SilentlyContinue

# 4. Two entries in the same process get distinct, ordered names.
$names = @(Get-ChildItem -LiteralPath $dir -Filter "*.json" -File | Sort-Object Name | ForEach-Object { $_.Name })
Assert-True ($names.Count -eq 2 -and $names[0] -cne $names[1]) "entry names are unique per write"

# 5. A drain pass against a refused connection keeps every entry (the
#    000/5xx "stop the pass, keep the remainder" semantics).
Invoke-AiMemoryDrainSpool -Max 8
$kept = @(Get-ChildItem -LiteralPath $dir -Filter "*.json" -File)
Assert-True ($kept.Count -eq 2) "refused connection keeps the backlog"

# 6. A missing spool directory is a no-op, not an error.
Remove-Item -Recurse -Force (Join-Path $root "data")
Invoke-AiMemoryDrainSpool -Max 8

# 7. Native OIDC entries require auth.json resolution. Even a server accepting
#    an anonymous request must not receive them from the script drain; eligible
#    entries behind them still deliver. A body mentioning auth_mode is data.
New-Item -ItemType Directory -Force -Path $dir | Out-Null
$oidcPath = Join-Path $dir "0000000000000-1-0000000000000000.json"
$oidcJson = [ordered]@{
    url = "http://127.0.0.1:49374/hook?event=oidc"
    body = '{"e":"native-oidc"}'
    created_ms = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    auth_mode = "oidc"
    attempts = 0
} | ConvertTo-Json -Compress
[IO.File]::WriteAllText($oidcPath, $oidcJson, [Text.UTF8Encoding]::new($false))
$env:AI_MEMORY_AUTH_TOKEN = "probe-bearer"
Write-AiMemorySpoolEvent -Url "http://127.0.0.1:49374/hook?event=static" -Body '{"auth_mode":"oidc"}'
Remove-Item Env:\AI_MEMORY_AUTH_TOKEN -ErrorAction SilentlyContinue
Write-AiMemorySpoolEvent -Url "http://127.0.0.1:49374/hook?event=none" -Body '{"auth_mode":"oidc"}'
$legacyJson = [ordered]@{
    url = "http://127.0.0.1:49374/hook?event=legacy"
    body = '{"auth_mode":"oidc"}'
    created_ms = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    attempts = 0
} | ConvertTo-Json -Compress
[IO.File]::WriteAllText((Join-Path $dir "9999999999999-1-0000000000000000.json"), $legacyJson, [Text.UTF8Encoding]::new($false))
$script:OidcRequests = [Collections.Generic.List[object]]::new()
function Invoke-WebRequest {
    param([switch] $UseBasicParsing, [switch] $NoProxy, [int] $TimeoutSec,
        [int] $MaximumRedirection, [string] $Method, [string] $Uri, $Headers,
        [string] $ContentType, $Body)
    $script:OidcRequests.Add([pscustomobject]@{ Url = $Uri; Authorization = $Headers["Authorization"] })
}
Invoke-AiMemoryDrainSpool -Max 8
Assert-True ((Test-Path $oidcPath) -and [IO.File]::ReadAllText($oidcPath) -ceq $oidcJson) "OIDC entry stays untouched"
Assert-True (@($script:OidcRequests | Where-Object { $_.Url -match 'event=oidc$' }).Count -eq 0) "OIDC entry is never posted"
Assert-True (@($script:OidcRequests | Where-Object { $_.Url -match 'event=static$' -and $_.Authorization -ceq 'Bearer probe-bearer' }).Count -eq 1) "static control delivers with its bearer"
foreach ($control in @("none", "legacy")) {
    Assert-True (@($script:OidcRequests | Where-Object { $_.Url -match "event=$control`$" -and -not $_.Authorization }).Count -eq 1) "$control control delivers without a bearer"
}
Assert-True ($script:OidcRequests.Count -eq 3) "only eligible entries are posted"
$kept = @(Get-ChildItem -LiteralPath $dir -Filter "*.json" -File)
Assert-True ($kept.Count -eq 1 -and $kept[0].FullName -eq $oidcPath) "only the OIDC entry stays queued"

# 8. Every request splats MaximumRedirection 0, matching the native hook.
$splat = Get-AiMemoryNoProxy
Assert-True ($splat.ContainsKey('MaximumRedirection') -and $splat.MaximumRedirection -eq 0) "splat refuses redirects"
Assert-True ((Get-AiMemoryResponseStatus $null) -eq 200) "null response defaults to 200"
Assert-True ((Get-AiMemoryResponseStatus ([pscustomobject]@{ Content = 'x' })) -eq 200) "missing StatusCode defaults to 200"
Assert-True ((Get-AiMemoryResponseStatus ([pscustomobject]@{ StatusCode = 302 })) -eq 302) "302 is classified"
Assert-True ((Get-AiMemoryResponseStatus ([pscustomobject]@{ StatusCode = 202 })) -eq 202) "202 is classified"

# 9. A 307 from the drain must not contact the Location origin and must keep
# the spooled event (the native hook_capture bite: the redirect target
# receives nothing). Remove the IWR mock so this pass hits the real cmdlet.
Remove-Item -Force -ErrorAction SilentlyContinue function:\Invoke-WebRequest
Remove-Item -Recurse -Force (Join-Path $root "data") -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path (Get-AiMemorySpoolDir) | Out-Null

$target = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
$source = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
$target.Start()
$source.Start()
$targetPort = ([System.Net.IPEndPoint]$target.LocalEndpoint).Port
$sourcePort = ([System.Net.IPEndPoint]$source.LocalEndpoint).Port
$targetUrl = "http://127.0.0.1:$targetPort/login"
$contacted = [System.Threading.ManualResetEventSlim]::new($false)
$targetRs = [runspacefactory]::CreateRunspace()
$sourceRs = [runspacefactory]::CreateRunspace()
$targetRs.Open()
$sourceRs.Open()
$targetPs = [powershell]::Create()
$sourcePs = [powershell]::Create()
$targetPs.Runspace = $targetRs
$sourcePs.Runspace = $sourceRs
[void]$targetPs.AddScript({
    param($Listener, $Flag)
    try {
        $client = $Listener.AcceptTcpClient()
        $Flag.Set()
        $stream = $client.GetStream()
        $buf = New-Object byte[] 1024
        $null = $stream.Read($buf, 0, $buf.Length)
        $resp = [Text.Encoding]::ASCII.GetBytes("HTTP/1.1 200 OK`r`nContent-Length: 5`r`nConnection: close`r`n`r`nlogin")
        $stream.Write($resp, 0, $resp.Length)
        $stream.Close()
        $client.Close()
    } catch {
    }
}).AddArgument($target).AddArgument($contacted)
[void]$sourcePs.AddScript({
    param($Listener, $Location)
    try {
        $client = $Listener.AcceptTcpClient()
        $stream = $client.GetStream()
        $buf = New-Object byte[] 8192
        $null = $stream.Read($buf, 0, $buf.Length)
        $resp = [Text.Encoding]::ASCII.GetBytes("HTTP/1.1 307 Temporary Redirect`r`nLocation: $Location`r`nContent-Length: 0`r`nConnection: close`r`n`r`n")
        $stream.Write($resp, 0, $resp.Length)
        $stream.Close()
        $client.Close()
    } catch {
    }
}).AddArgument($source).AddArgument($targetUrl)
$targetHandle = $targetPs.BeginInvoke()
$sourceHandle = $sourcePs.BeginInvoke()
try {
    Write-AiMemorySpoolEvent -Url "http://127.0.0.1:$sourcePort/hook?event=stop&agent=codex" -Body '{"e":"redirect"}'
    Invoke-AiMemoryDrainSpool -Max 8
    Assert-True (-not $contacted.Wait(200)) "drain redirect target is not contacted"
    $kept = @(Get-ChildItem -LiteralPath (Get-AiMemorySpoolDir) -Filter "*.json" -File)
    Assert-True ($kept.Count -eq 1) "307 keeps the spool entry"

    # The same splat on GET must not follow a 302 to the login origin.
    $getSource = [System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback, 0)
    $getSource.Start()
    $getPort = ([System.Net.IPEndPoint]$getSource.LocalEndpoint).Port
    $getRs = [runspacefactory]::CreateRunspace()
    $getRs.Open()
    $getPs = [powershell]::Create()
    $getPs.Runspace = $getRs
    [void]$getPs.AddScript({
        param($Listener, $Location)
        try {
            $client = $Listener.AcceptTcpClient()
            $stream = $client.GetStream()
            $buf = New-Object byte[] 8192
            $null = $stream.Read($buf, 0, $buf.Length)
            $resp = [Text.Encoding]::ASCII.GetBytes("HTTP/1.1 302 Found`r`nLocation: $Location`r`nContent-Length: 10`r`nConnection: close`r`n`r`nLOGIN-HTML")
            $stream.Write($resp, 0, $resp.Length)
            $stream.Close()
            $client.Close()
        } catch {
        }
    }).AddArgument($getSource).AddArgument($targetUrl)
    $getHandle = $getPs.BeginInvoke()
    $injected = $null
    try {
        $NoProxy = Get-AiMemoryNoProxy
        try {
            $Response = Invoke-WebRequest @NoProxy -UseBasicParsing -TimeoutSec 2 -Uri "http://127.0.0.1:$getPort/handoff"
            if ((Get-AiMemoryResponseStatus $Response) -ge 200 -and (Get-AiMemoryResponseStatus $Response) -lt 300) {
                $injected = [string]$Response.Content
            }
        } catch {
        }
        Assert-True (-not $contacted.Wait(200)) "GET redirect target is not contacted"
        Assert-True ($null -eq $injected) "302 body is not injected"
    } finally {
        try { $getSource.Stop() } catch { }
        try { $getPs.EndInvoke($getHandle) } catch { }
        $getPs.Dispose()
        $getRs.Close()
    }
} finally {
    try { $source.Stop() } catch { }
    try { $target.Stop() } catch { }
    try { $sourcePs.EndInvoke($sourceHandle) } catch { }
    try { $targetPs.EndInvoke($targetHandle) } catch { }
    $sourcePs.Dispose()
    $targetPs.Dispose()
    $sourceRs.Close()
    $targetRs.Close()
    $contacted.Dispose()
}

Remove-Item -Recurse -Force $root -ErrorAction SilentlyContinue
Write-Output "spool ps behavioral probe: ok checks=31"
exit 0
