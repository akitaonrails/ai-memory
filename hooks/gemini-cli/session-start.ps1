. "$PSScriptRoot\..\lib\ai-memory-hook.ps1"
Invoke-AiMemoryHook -Event "session-start" -Agent "gemini-cli" -FetchHandoff -BriefingOncePerSession
exit 0
