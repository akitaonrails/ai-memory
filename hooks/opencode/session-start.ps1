. "$PSScriptRoot\..\lib\ai-memory-hook.ps1"
Invoke-AiMemoryHook -Event "session-start" -Agent "open-code" -FetchHandoff -BriefingOncePerSession
exit 0
