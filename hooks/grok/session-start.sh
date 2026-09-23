#!/bin/sh
# Grok Build CLI SessionStart hook.
# Grok ignores SessionStart stdout, so this hook captures the event only.
# Do NOT fetch /handoff here: accepting a handoff is destructive and Grok
# would discard the returned context. UserPromptSubmit cannot deliver it
# either (allowing-hook stdout is discarded).
_lib_dir="$(dirname "$0")"
[ -f "$_lib_dir/_lib.sh" ] || _lib_dir="$_lib_dir/.."
. "$_lib_dir/_lib.sh"

SERVER="${AI_MEMORY_HOOK_URL:-http://127.0.0.1:49374}"
PAYLOAD=$(cat)
CWD=$(ai_memory_extract_cwd "$PAYLOAD")
QS=$(ai_memory_marker_qs "$CWD")

printf '%s' "$PAYLOAD" \
    | ai_memory_post_hook "$SERVER/hook?event=session-start&agent=grok${QS}" >/dev/null 2>&1 || true
printf '{}\n'
exit 0
