#!/bin/sh
# Claude Code SessionStart hook.
# 1. Forwards the event JSON to the ai-memory server (fire-and-forget).
# 2. Synchronously fetches the pending cross-agent handoff and prints
#    it to stdout — Claude Code prepends `session-start` stdout to the
#    next session, so the resuming agent sees prior context with no
#    human in the loop.
# 3. Delivers the compiled project brief ([briefing]
#    inject_on_session_start) once per session, gated by a marker file
#    keyed on Claude Code's native session id when the payload carries one.
#
# Walks up from the payload's cwd for a .ai-memory.toml marker file
# and appends cwd plus marker query params to both URLs — so a session
# resuming under basename or marker-declared routing doesn't query the
# wrong bucket and miss its own handoff.
# At runtime (after `install-hooks --apply`) `_lib.sh` is staged
# alongside this script. From the source tree it lives one dir up.
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
# repository opted in. Prefer Claude Code's native session id when supplied;
# otherwise use a stable hash of agent+cwd.
BRIEF_QS=$(ai_memory_briefing_qs "$CWD")
BRIEF_FILE=""
if [ -n "$BRIEF_QS" ]; then
    BRIEF_KEY="$SESSION_ID"
    if [ -z "$BRIEF_KEY" ]; then
        BRIEF_KEY="claude-code-$(printf '%s' "claude-code:$CWD" | cksum | awk '{print $1}')"
    fi
    BRIEF_FILE=$(ai_memory_briefed_file "$BRIEF_KEY")
    [ -f "$BRIEF_FILE" ] && BRIEF_QS=""
fi

printf '%s' "$PAYLOAD" \
    | ai_memory_post_hook "$SERVER/hook?event=session-start&agent=claude-code${QS}" >/dev/null 2>&1 || true

# Claude Code prepends a SessionStart hook's stdout to the resuming
# session as context. Emit it as structured JSON
# (hookSpecificOutput.additionalContext) instead of raw text: bare text
# does not start with "{", so Claude Code logs every session start as
# "Hook output does not start with {, treating as plain text". JSON
# injects the same handoff with a clean debug log; no handoff -> "{}".
HANDOFF=$(ai_memory_get_handoff "$SERVER/handoff?agent=claude-code${QS}${SESSION_QS}${BRIEF_QS}" 2>/dev/null || true)
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
