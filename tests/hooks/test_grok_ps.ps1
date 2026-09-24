# Behavioral evidence for the Grok PowerShell handoff helpers (pure
# functions only — no HTTP, no process state). Invoked by
# tests/hooks/test_lib.sh when pwsh is available; exits non-zero on any
# failed assertion.

param([Parameter(Mandatory = $true)] [string] $PsLib)

$ErrorActionPreference = "Stop"
. $PsLib

function Assert-True($value, $label) {
    if (-not $value) { throw "grok ps probe failed: $label" }
}

# 1. Child-session gate: every key the native router checks, and the
#    negative cases.
foreach ($key in @("subagentType", "subagent_type", "agent_type", "agent_id", "parentSessionId")) {
    $payload = ConvertFrom-Json ('{"' + $key + '":"planner-a"}')
    Assert-True (Test-AiMemorySubagentPayload $payload) "child key $key detected"
    $blank = ConvertFrom-Json ('{"' + $key + '":"   "}')
    Assert-True (-not (Test-AiMemorySubagentPayload $blank)) "blank $key is not a child session"
}
$parent = ConvertFrom-Json '{"session_id":"g-sess","cwd":"/tmp","tool_name":"read_file"}'
Assert-True (-not (Test-AiMemorySubagentPayload $parent)) "parent payload is not a child session"
Assert-True (-not (Test-AiMemorySubagentPayload $null)) "unparsed payload is not a child session"

# 2. additionalContext clip parity with the native 10 000-char cap.
$long = ("x" * 10100)
$clipped = Clip-AiMemoryChars $long 10000
Assert-True ($clipped.Length -le 10000) "clipped length"
Assert-True ($clipped.EndsWith("`n[truncated]")) "clip marker present"
$short = Clip-AiMemoryChars "short" 10000
Assert-True ($short -eq "short") "short text passes through unchanged"

Write-Output "grok ps behavioral probe: ok checks=15"
exit 0