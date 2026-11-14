#!/bin/sh
# Devin CLI post-compaction hook.
# Triggers *after* compaction with a summary field (Devin-specific).
_lib_dir="$(dirname "$0")"
[ -f "$_lib_dir/_lib.sh" ] || _lib_dir="$_lib_dir/.."
. "$_lib_dir/_lib.sh"

SERVER="${AI_MEMORY_HOOK_URL:-http://127.0.0.1:49374}"
PAYLOAD=$(cat)
CWD=$(ai_memory_resolve_cwd "$PAYLOAD")
QS=$(ai_memory_marker_qs "$CWD")
SID_QS=$(ai_memory_session_id_qs devin post-compaction "$PAYLOAD")

printf '%s' "$PAYLOAD" \
    | ai_memory_post_hook "$SERVER/hook?event=post-compaction&agent=devin${QS}${SID_QS}" >/dev/null 2>&1 || true
printf '{}\n'
exit 0
