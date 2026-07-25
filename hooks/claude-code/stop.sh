#!/bin/sh
# Claude Code stop hook.
# Forwards the event JSON to the ai-memory server, fire-and-forget.
# Walks up from the payload's cwd for a .ai-memory.toml marker file;
# if found, appends marker query params to the URL so the server
# applies the declared workspace/project/strategy instead of
# bucketing by basename(cwd) under the default workspace.
# At runtime (after `install-hooks --apply`) `_lib.sh` is staged
# alongside this script. From the source tree it lives one dir up.
_lib_dir="$(dirname "$0")"
[ -f "$_lib_dir/_lib.sh" ] || _lib_dir="$_lib_dir/.."
. "$_lib_dir/_lib.sh"

SERVER="${AI_MEMORY_HOOK_URL:-http://127.0.0.1:49374}"
PAYLOAD=$(cat)
# Assistant/Stop capture (#196) is native-only: the script fallback cannot
# sanitize the assistant message, so if the raw payload still carries the field
# we drop the whole Stop rather than POST it verbatim. A literal substring check
# (POSIX `case`, no bash-isms per _lib.sh) — conservative on purpose: any Stop
# mentioning the key is dropped. Move to a native install to capture it safely.
case "$PAYLOAD" in
    *'"last_assistant_message"'*)
        printf '{}\n'
        exit 0
        ;;
esac
CWD=$(ai_memory_extract_cwd "$PAYLOAD")
QS=$(ai_memory_marker_qs "$CWD")

printf '%s' "$PAYLOAD" \
    | ai_memory_post_hook "$SERVER/hook?event=stop&agent=claude-code${QS}" >/dev/null 2>&1 || true
# Acknowledge to Claude Code with an empty JSON object. This hook only
# POSTs (fire-and-forget) and injects no context; without "{" on stdout
# Claude Code treats the (empty) output as plain text and logs
# "Hook output does not start with {, treating as plain text".
# Emitting {} keeps the debug log clean.
printf '{}\n'
exit 0
