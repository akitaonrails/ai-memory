#!/bin/sh
# GitHub Copilot CLI subagent-start hook — forwards the event so the server can seed
# the subagent's session id for drop_subagent_captures (so the whole nested
# session, incl. the unmarked tail, is dropped). Mirrors stop.sh.
_lib_dir="$(dirname "$0")"
[ -f "$_lib_dir/_lib.sh" ] || _lib_dir="$_lib_dir/.."
. "$_lib_dir/_lib.sh"

SERVER="${AI_MEMORY_HOOK_URL:-http://127.0.0.1:49374}"
PAYLOAD=$(cat)
CWD=$(ai_memory_extract_cwd "$PAYLOAD")
QS=$(ai_memory_marker_qs "$CWD")

printf '%s' "$PAYLOAD" \
    | ai_memory_post_hook "$SERVER/hook?event=subagent-start&agent=copilot-cli${QS}" >/dev/null 2>&1 || true
printf '{}\n'
exit 0
