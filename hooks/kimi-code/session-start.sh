#!/bin/sh
# kimi-code SessionStart hook.
# Forwards the event JSON to the ai-memory server (fire-and-forget).
#
# No handoff fetch here: Kimi Code fires SessionStart but DISCARDS the
# hook's stdout/result (kimi-code v0.28.1,
# packages/agent-core/src/session/index.ts), so a handoff consumed here
# would be lost. The handoff is delivered by user-prompt-submit.sh —
# Kimi Code injects UserPromptSubmit stdout as a user message before
# the turn.
#
_lib_dir="$(dirname "$0")"
[ -f "$_lib_dir/_lib.sh" ] || _lib_dir="$_lib_dir/.."
. "$_lib_dir/_lib.sh"

SERVER="${AI_MEMORY_HOOK_URL:-http://127.0.0.1:49374}"
PAYLOAD=$(cat)
CWD=$(ai_memory_extract_cwd "$PAYLOAD")
QS=$(ai_memory_marker_qs "$CWD")

printf '%s' "$PAYLOAD" \
    | ai_memory_post_hook "$SERVER/hook?event=session-start&agent=kimi-code${QS}" >/dev/null 2>&1 || true
exit 0
