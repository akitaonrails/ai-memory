# Pool (Poolside Agent CLI) session-start hook.
# Capture only — Pool's SessionStart stdout injection is not demonstrated,
# so no -FetchHandoff: accepting a handoff is destructive and the context
# could be silently lost. Recover handoffs via MCP `memory_handoff_accept`.
. "$PSScriptRoot\..\lib\ai-memory-hook.ps1"
Invoke-AiMemoryHook -Event "session-start" -Agent "pool"
exit 0
