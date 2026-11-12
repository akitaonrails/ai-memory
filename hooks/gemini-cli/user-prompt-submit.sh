#!/bin/sh
# gemini-cli user-prompt hook.
# Forwards the event JSON to the ai-memory server, fire-and-forget.
_lib_dir="$(dirname "$0")"
[ -f "$_lib_dir/_lib.sh" ] || _lib_dir="$_lib_dir/.."
. "$_lib_dir/_lib.sh"

SERVER="${AI_MEMORY_HOOK_URL:-http://127.0.0.1:49374}"
PAYLOAD=$(cat)
# Gemini CLI appends the contents of every @-referenced file to the prompt it
# hands BeforeAgent. The script fallback cannot cut them out of the JSON, so a
# prompt carrying them is dropped rather than posted or spooled with them; the
# native `ai-memory hook` keeps the user's words and drops only the files.
case "$PAYLOAD" in
    *'--- Content from referenced files ---'*)
        exit 0
        ;;
esac
CWD=$(ai_memory_extract_cwd "$PAYLOAD")
QS=$(ai_memory_marker_qs "$CWD")

printf '%s' "$PAYLOAD" \
    | ai_memory_post_hook "$SERVER/hook?event=user-prompt&agent=gemini-cli${QS}" >/dev/null 2>&1 || true
exit 0
