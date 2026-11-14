#!/bin/sh
# Devin CLI SessionStart hook.
# 1. Forwards the event JSON to ai-memory.
# 2. Synchronously fetches the pending handoff and injects it through
#    hookSpecificOutput.additionalContext, which Devin consumes as
#    additional session context.
_lib_dir="$(dirname "$0")"
[ -f "$_lib_dir/_lib.sh" ] || _lib_dir="$_lib_dir/.."
. "$_lib_dir/_lib.sh"

SERVER="${AI_MEMORY_HOOK_URL:-http://127.0.0.1:49374}"
PAYLOAD=$(cat)
CWD=$(ai_memory_resolve_cwd "$PAYLOAD")
QS=$(ai_memory_marker_qs "$CWD")
# The `[briefing]` opt-in rides the handoff GET, as in the native hook.
BRIEF_QS=$(ai_memory_briefing_qs "$CWD")
SID_QS=$(ai_memory_session_id_qs devin session-start "$PAYLOAD")

printf '%s' "$PAYLOAD" \
    | ai_memory_post_hook "$SERVER/hook?event=session-start&agent=devin${QS}${SID_QS}" >/dev/null 2>&1 || true

if [ -z "$SID_QS" ] && _am_native_sid=$(ai_memory_payload_session_id "$PAYLOAD"); then
    SID_QS="&session_id=$(ai_memory_url_encode "$_am_native_sid")"
fi
HANDOFF=$(ai_memory_get_start_handoff "$PAYLOAD" "$SERVER/handoff?agent=devin${QS}${SID_QS}${BRIEF_QS}" 2>/dev/null || true)
if [ -n "$HANDOFF" ]; then
    printf '{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":%s}}\n' \
        "$(printf '%s' "$HANDOFF" | ai_memory_json_string)"
else
    printf '{}\n'
fi
exit 0
