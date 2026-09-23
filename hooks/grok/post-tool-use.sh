#!/bin/sh
# Grok Build CLI post-tool-use hook.
# PostToolUse stdout is the channel Grok shows the model
# (hookSpecificOutput.additionalContext, after the tool result).
# SessionStart and UserPromptSubmit stdout are discarded, so they must
# not accept the handoff. This fetch runs once per session.
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

printf '%s' "$PAYLOAD" \
    | ai_memory_post_hook "$SERVER/hook?event=post-tool-use&agent=grok${QS}" >/dev/null 2>&1 || true

SHOWN_KEY="$SESSION_ID"
if [ -z "$SHOWN_KEY" ]; then
    SHOWN_KEY="grok-post-$(printf '%s' "grok:$CWD" | cksum | awk '{print $1}')"
fi
SHOWN=$(ai_memory_briefed_file "post-$SHOWN_KEY")
if [ -f "$SHOWN" ]; then
    printf '{}\n'
    exit 0
fi

case "$PAYLOAD" in
    *'"subagentType"'*|*'\"subagentType\"'*|*'"parentSessionId"'*)
        printf '{}\n'
        exit 0
        ;;
esac
BRIEF_QS=$(ai_memory_briefing_qs "$CWD")
HANDOFF=$(ai_memory_get_handoff "$SERVER/handoff?agent=grok${QS}${SESSION_QS}${BRIEF_QS}" 2>/dev/null || true)
if [ -n "$HANDOFF" ]; then
    ai_memory_mark_briefed "$SHOWN"
    CTX=$(printf '%s' "$HANDOFF" | ai_memory_json_string)
    printf '{"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":%s}}\n' "$CTX"
    exit 0
fi
printf '{}\n'
exit 0
