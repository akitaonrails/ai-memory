. "$PSScriptRoot\..\lib\ai-memory-hook.ps1"
Invoke-AiMemoryHook -Event "pre-tool-use" -Agent "copilot-cli"
exit 0
