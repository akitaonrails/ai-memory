# Codex user-prompt hook (Windows).
$Server = if ($env:AI_MEMORY_HOOK_URL) { $env:AI_MEMORY_HOOK_URL } else { "http://127.0.0.1:49374" }
$Payload = $input | Out-String
$headers = @{ "Content-Type" = "application/json" }
if ($env:AI_MEMORY_AUTH_TOKEN) { $headers["Authorization"] = "Bearer $env:AI_MEMORY_AUTH_TOKEN" }
try { Invoke-RestMethod -Uri "$Server/hook?event=user-prompt&agent=codex" -Method POST -Headers $headers -Body $Payload -TimeoutSec 1 -ErrorAction SilentlyContinue | Out-Null } catch {}
exit 0
