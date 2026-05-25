# Gemini CLI SessionStart hook (Windows).
$Server = if ($env:AI_MEMORY_HOOK_URL) { $env:AI_MEMORY_HOOK_URL } else { "http://127.0.0.1:49374" }
$Payload = $input | Out-String

$headers = @{ "Content-Type" = "application/json" }
if ($env:AI_MEMORY_AUTH_TOKEN) {
    $headers["Authorization"] = "Bearer $env:AI_MEMORY_AUTH_TOKEN"
}

try {
    Invoke-RestMethod -Uri "$Server/hook?event=session-start&agent=gemini-cli" `
        -Method POST -Headers $headers -Body $Payload `
        -TimeoutSec 1 -ErrorAction SilentlyContinue | Out-Null
} catch {}

try {
    $handoff = Invoke-RestMethod -Uri "$Server/handoff?agent=gemini-cli" `
        -Headers $headers -TimeoutSec 1 -ErrorAction SilentlyContinue
    if ($handoff) { Write-Output $handoff }
} catch {}

exit 0
