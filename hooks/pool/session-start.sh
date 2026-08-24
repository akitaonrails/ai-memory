#!/bin/sh
# Pool (Poolside Agent CLI) SessionStart hook.
# Pool tolerates hook stdout, but model-visible context injection from
# SessionStart stdout is not demonstrated, so this hook captures the event
# only. Do NOT fetch /handoff here: accepting a handoff is destructive and
# an undelivered handoff would be silently lost. Recover a prior session's
# handoff via the MCP `memory_handoff_accept` tool instead.
_lib_dir="$(dirname "$0")"
[ -f "$_lib_dir/_lib.sh" ] || _lib_dir="$_lib_dir/.."
. "$_lib_dir/_lib.sh"

SERVER="${AI_MEMORY_HOOK_URL:-http://127.0.0.1:49374}"
PAYLOAD=$(cat)
CWD=$(ai_memory_extract_cwd "$PAYLOAD")
QS=$(ai_memory_marker_qs "$CWD")

printf '%s' "$PAYLOAD" \
    | ai_memory_post_hook "$SERVER/hook?event=session-start&agent=pool${QS}" >/dev/null 2>&1 || true
exit 0
