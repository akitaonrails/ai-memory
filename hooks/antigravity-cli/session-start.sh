#!/bin/sh
# antigravity-cli PreInvocation hook. Forwards the event JSON to the
# ai-memory server, then injects any pending handoff as an ephemeral
# model-visible message using Antigravity's JSON stdout contract. Also
# delivers the compiled project brief ([briefing] inject_on_session_start)
# once per session, gated by a marker file keyed on the native session id
# when the payload carries one.
_lib_dir="$(dirname "$0")"
[ -f "$_lib_dir/_lib.sh" ] || _lib_dir="$_lib_dir/.."
. "$_lib_dir/_lib.sh"

SERVER="${AI_MEMORY_HOOK_URL:-http://127.0.0.1:49374}"
PAYLOAD=$(cat)
if ! ai_memory_antigravity_is_initial_invocation "$PAYLOAD"; then
    printf '{}\n'
    exit 0
fi
CWD=$(ai_memory_extract_cwd "$PAYLOAD")
QS=$(ai_memory_marker_qs "$CWD")
SESSION_ID=$(ai_memory_extract_session_id "$PAYLOAD")
SESSION_QS=""
if [ -n "$SESSION_ID" ]; then
    SESSION_QS="&session_id=$(ai_memory_url_encode "$SESSION_ID")"
fi

# Once-per-session briefing gate. Marker files are created only when the
# repository opted in. Prefer the native session id when supplied; otherwise
# use a stable hash of agent+cwd.
BRIEF_QS=$(ai_memory_briefing_qs "$CWD")
BRIEF_FILE=""
if [ -n "$BRIEF_QS" ]; then
    BRIEF_KEY="$SESSION_ID"
    if [ -z "$BRIEF_KEY" ]; then
        BRIEF_KEY="antigravity-cli-$(printf '%s' "antigravity-cli:$CWD" | cksum | awk '{print $1}')"
    fi
    BRIEF_FILE=$(ai_memory_briefed_file "$BRIEF_KEY")
    [ -f "$BRIEF_FILE" ] && BRIEF_QS=""
fi

printf '%s' "$PAYLOAD" \
    | ai_memory_post_hook "$SERVER/hook?event=session-start&agent=antigravity-cli${QS}" >/dev/null 2>&1 || true
HANDOFF=$(ai_memory_get_handoff "$SERVER/handoff?agent=antigravity-cli${QS}${SESSION_QS}${BRIEF_QS}" 2>/dev/null || true)
# Mark an opted-in session as briefed only AFTER the GET completed — success
# or error. Fail-open on purpose: with the server down the flagged request
# delivers nothing anyway, and the one lost brief returns next session.
[ -n "$BRIEF_FILE" ] && ai_memory_mark_briefed "$BRIEF_FILE"
if [ -n "$HANDOFF" ]; then
    printf '{"injectSteps":[{"ephemeralMessage":'
    printf '%s' "$HANDOFF" | ai_memory_json_string
    printf '}]}\n'
else
    printf '{}\n'
fi
exit 0
