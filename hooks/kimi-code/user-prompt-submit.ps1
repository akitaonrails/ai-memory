# Kimi Code injects UserPromptSubmit hook stdout as a user message
# (origin hook_result) before the turn, so the pending handoff is
# fetched here. Empty stdout injects nothing. The compiled project
# brief ([briefing] inject_on_session_start) rides the FIRST prompt of
# the session only — kimi discards SessionStart hook stdout, so this
# is the once-per-session parity with Claude's SessionStart brief.
. "$PSScriptRoot\..\lib\ai-memory-hook.ps1"
Invoke-AiMemoryHook -Event "user-prompt" -Agent "kimi-code" -FetchHandoff -BriefingOncePerSession
exit 0
