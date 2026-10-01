. "$PSScriptRoot\..\lib\ai-memory-hook.ps1"
Invoke-AiMemoryHook -Event "session-start" -Agent "cursor" -FetchHandoff -BriefingOncePerSession
exit 0
