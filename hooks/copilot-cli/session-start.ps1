. "$PSScriptRoot\..\lib\ai-memory-hook.ps1"
Invoke-AiMemoryHook -Event "session-start" -Agent "copilot-cli" -FetchHandoff -CopilotCliSessionStartOutput
exit 0
