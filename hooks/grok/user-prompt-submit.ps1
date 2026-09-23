# Do not fetch /handoff. Grok discards allowing UserPromptSubmit stdout,
# and GET /handoff would accept the handoff anyway.
. "$PSScriptRoot\..\lib\ai-memory-hook.ps1"
Invoke-AiMemoryHook -Event "user-prompt" -Agent "grok"
exit 0
