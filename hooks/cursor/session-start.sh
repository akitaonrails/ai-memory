#!/bin/sh
# cursor SessionStart hook.
# 1. Forwards the event JSON to the ai-memory server (fire-and-forget).
# 2. Synchronously fetches any pending cross-agent handoff and prints
#    it to stdout — agent CLIs prepend session-start hook stdout to
#    the next session, so the resuming agent sees prior context with
#    no human in the loop.
# 3. Delivers the compiled project brief ([briefing]
#    inject_on_session_start) once per session, gated by a marker file
#    keyed on cursor's native session id when the payload carries one.
#
_lib_dir="$(dirname "$0")"
[ -f "$_lib_dir/_lib.sh" ] || _lib_dir="$_lib_dir/.."
. "$_lib_dir/_lib.sh"

SERVER="${AI_MEMORY_HOOK_URL:-http://127.0.0.1:49374}"
PAYLOAD=$(cat)
CWD=$(ai_memory_extract_cwd "$PAYLOAD")
QS=$(ai_memory_marker_qs "$CWD")
SESSION_ID=$(ai_memory_extract_session_id "$PAYLOAD")
SESSION_QS=""
[ -n "$SESSION_ID" ] && SESSION_QS="&session_id=$(ai_memory_url_encode "$SESSION_ID")"

# Once-per-session briefing gate. Marker files are created only when the
# repository opted in. Prefer cursor's native session id when supplied;
# otherwise use a stable hash of agent+cwd.
BRIEF_QS=$(ai_memory_briefing_qs "$CWD")
BRIEF_FILE=""
if [ -n "$BRIEF_QS" ]; then
    BRIEF_KEY="$SESSION_ID"
    if [ -z "$BRIEF_KEY" ]; then
        BRIEF_KEY="cursor-$(printf '%s' "cursor:$CWD" | cksum | awk '{print $1}')"
    fi
    BRIEF_FILE=$(ai_memory_briefed_file "$BRIEF_KEY")
    [ -f "$BRIEF_FILE" ] && BRIEF_QS=""
fi

printf '%s' "$PAYLOAD" \
    | ai_memory_post_hook "$SERVER/hook?event=session-start&agent=cursor${QS}" >/dev/null 2>&1 || true
HANDOFF=$(ai_memory_get_handoff "$SERVER/handoff?agent=cursor${QS}${SESSION_QS}${BRIEF_QS}" 2>/dev/null || true)
# Mark an opted-in session as briefed only AFTER the GET completed — success
# or error. Fail-open on purpose: with the server down the flagged request
# delivers nothing anyway, and the one lost brief returns next session.
[ -n "$BRIEF_FILE" ] && ai_memory_mark_briefed "$BRIEF_FILE"
[ -n "$HANDOFF" ] && printf '%s\n' "$HANDOFF"
exit 0
