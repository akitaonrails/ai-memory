# Kimi Code discards SessionStart hook stdout (v0.28.1,
# packages/agent-core/src/session/index.ts), so the handoff is fetched
# by user-prompt-submit.ps1 instead. This hook only captures the event.
. "$PSScriptRoot\..\lib\ai-memory-hook.ps1"
Invoke-AiMemoryHook -Event "session-start" -Agent "kimi-code"
exit 0
