#!/bin/sh
# GitHub Copilot CLI SessionStart hook.
# 1. Forwards the event JSON to the ai-memory server (fire-and-forget).
# 2. Synchronously fetches the pending cross-agent handoff and prints
#    it to stdout — GitHub Copilot CLI prepends `session-start` stdout to the
#    next session, so the resuming agent sees prior context with no
#    human in the loop.
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
# The `[briefing]` opt-in rides the handoff GET, as in the native hook.
BRIEF_QS=$(ai_memory_briefing_qs "$CWD")
SESSION_ID=$(ai_memory_extract_session_id "$PAYLOAD")
SESSION_QS=""
[ -n "$SESSION_ID" ] && SESSION_QS="&session_id=$(ai_memory_url_encode "$SESSION_ID")"

printf '%s' "$PAYLOAD" \
    | ai_memory_post_hook "$SERVER/hook?event=session-start&agent=copilot-cli${QS}" >/dev/null 2>&1 || true

# Copilot CLI accepts SessionStart context only in its top-level
# additionalContext envelope. The hook protocol maps the PascalCase event
# name to snake_case fields before this script receives it; no handoff -> {}.
HANDOFF=$(ai_memory_get_handoff "$SERVER/handoff?agent=copilot-cli${QS}${SESSION_QS}${BRIEF_QS}" 2>/dev/null || true)
if [ -n "$HANDOFF" ]; then
    printf '{"additionalContext":%s}\n' \
        "$(printf '%s' "$HANDOFF" | ai_memory_json_string)"
else
    printf '{}\n'
fi
exit 0
