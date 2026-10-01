#!/bin/sh
# Devin CLI SessionStart hook.
# 1. Forwards the event JSON to ai-memory.
# 2. Synchronously fetches the pending handoff and injects it through
#    hookSpecificOutput.additionalContext, which Devin consumes as
#    additional session context.
# 3. Delivers the compiled project brief ([briefing]
#    inject_on_session_start) once per session, gated by a marker file
#    keyed on the synthetic session id ai_memory_session_id_qs persists
#    (Devin's payload carries no native session id of its own).
_lib_dir="$(dirname "$0")"
[ -f "$_lib_dir/_lib.sh" ] || _lib_dir="$_lib_dir/.."
. "$_lib_dir/_lib.sh"

SERVER="${AI_MEMORY_HOOK_URL:-http://127.0.0.1:49374}"
PAYLOAD=$(cat)
CWD=$(ai_memory_resolve_cwd "$PAYLOAD")
QS=$(ai_memory_marker_qs "$CWD")
SID_QS=$(ai_memory_session_id_qs devin session-start)

# Once-per-session briefing gate, keyed on the same synthetic session id
# just minted/read above so it matches exactly what SID_QS already names.
BRIEF_QS=$(ai_memory_briefing_qs "$CWD")
BRIEF_FILE=""
if [ -n "$BRIEF_QS" ]; then
    BRIEF_KEY="devin-${SID_QS#&session_id=}"
    BRIEF_FILE=$(ai_memory_briefed_file "$BRIEF_KEY")
    [ -f "$BRIEF_FILE" ] && BRIEF_QS=""
fi

printf '%s' "$PAYLOAD" \
    | ai_memory_post_hook "$SERVER/hook?event=session-start&agent=devin${QS}${SID_QS}" >/dev/null 2>&1 || true

HANDOFF=$(ai_memory_get_handoff "$SERVER/handoff?agent=devin${QS}${SID_QS}${BRIEF_QS}" 2>/dev/null || true)
# Mark an opted-in session as briefed only AFTER the GET completed — success
# or error. Fail-open on purpose: with the server down the flagged request
# delivers nothing anyway, and the one lost brief returns next session.
[ -n "$BRIEF_FILE" ] && ai_memory_mark_briefed "$BRIEF_FILE"
if [ -n "$HANDOFF" ]; then
    printf '{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":%s}}\n' \
        "$(printf '%s' "$HANDOFF" | ai_memory_json_string)"
else
    printf '{}\n'
fi
exit 0
