. "$PSScriptRoot\..\lib\ai-memory-hook.ps1"
Invoke-AiMemoryHook -Event "session-start" -Agent "claude-code" -FetchHandoff -BriefingOncePerSession
exit 0
