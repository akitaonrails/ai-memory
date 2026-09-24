#!/bin/sh
# Smoke tests for hooks/_lib.sh. Run from the repo root:
#
#   sh tests/hooks/test_lib.sh
#
# Exits non-zero on any failure. POSIX shell + sed/awk only, so no extra
# CI setup needed.
set -eu

# shellcheck source=../../hooks/_lib.sh
. "$(dirname "$0")/../../hooks/_lib.sh"

PASS=0
FAIL=0
TMP=$(mktemp -d)
# Pin HOME inside the temp tree so walk-up never leaves the sandbox.
ORIG_HOME=${HOME:-}
HOME="$TMP"
export HOME
unset AI_MEMORY_RUN_ID AI_MEMORY_MANAGED_WORKSTREAM_ID AI_MEMORY_MANAGED_WORKSTREAM_NAME
trap 'rm -rf "$TMP"; HOME=$ORIG_HOME' EXIT

assert_eq() {
    desc="$1"; want="$2"; got="$3"
    if [ "$want" = "$got" ]; then
        PASS=$((PASS+1))
        printf '  ok  %s\n' "$desc"
    else
        FAIL=$((FAIL+1))
        printf '  FAIL %s\n    want=%s\n    got =%s\n' "$desc" "$want" "$got"
    fi
}

# Git Bash reports $PWD and mktemp's directory as MSYS paths (/h/..., /tmp/...).
# powershell.exe resolves neither, so a path handed to it has to be converted
# first. cygpath ships with Git for Windows and exists nowhere else, where the
# path is already native.
host_path() {
    if command -v cygpath >/dev/null 2>&1; then
        cygpath -w "$1"
    else
        printf '%s' "$1"
    fi
}

# --- parse_toml_key ---------------------------------------------------
cat >"$TMP/sample.toml" <<EOF
# Comment line
workspace = "movvia"
project = "pe-portais"
project_strategy = "repo-root"

# Trailing comment
EOF

assert_eq "parse workspace"           "movvia"     "$(ai_memory_parse_toml_key "$TMP/sample.toml" workspace)"
assert_eq "parse project"             "pe-portais" "$(ai_memory_parse_toml_key "$TMP/sample.toml" project)"
assert_eq "parse project_strategy"    "repo-root"  "$(ai_memory_parse_toml_key "$TMP/sample.toml" project_strategy)"
assert_eq "absent key returns empty"  ""           "$(ai_memory_parse_toml_key "$TMP/sample.toml" missing)"
assert_eq "absent file returns empty" ""           "$(ai_memory_parse_toml_key "$TMP/no-such-file.toml" workspace)"

# --- find_marker ------------------------------------------------------
mkdir -p "$TMP/a/b/c/d"
printf 'workspace = "deep"\n' >"$TMP/a/.ai-memory.toml"
assert_eq "walks up to find marker" "$TMP/a/.ai-memory.toml" \
    "$(ai_memory_find_marker "$TMP/a/b/c/d")"
assert_eq "no marker returns empty" "" \
    "$(ai_memory_find_marker "$TMP/nonexistent/path")"

# A checkout outside HOME may inherit a marker inside its own git tree, but
# never one from an unrelated parent. A plain directory outside HOME checks
# only its exact cwd.
OUTSIDE_HOME="$TMP/account-home"
mkdir -p "$OUTSIDE_HOME" "$TMP/outside/repo/src" "$TMP/outside/plain"
printf 'workspace = "wrong"\n' >"$TMP/outside/.ai-memory.toml"
mkdir -p "$TMP/outside/repo/.git"
printf 'workspace = "right"\n' >"$TMP/outside/repo/.ai-memory.toml"
HOME="$OUTSIDE_HOME"
export HOME
assert_eq "outside HOME finds marker within checkout" \
    "$TMP/outside/repo/.ai-memory.toml" \
    "$(ai_memory_find_marker "$TMP/outside/repo/src")"
rm -f "$TMP/outside/repo/.ai-memory.toml"
assert_eq "outside HOME stops at checkout root" "" \
    "$(ai_memory_find_marker "$TMP/outside/repo/src")"
assert_eq "outside HOME plain dir rejects parent marker" "" \
    "$(ai_memory_find_marker "$TMP/outside/plain")"
HOME="$TMP"
export HOME

# --- extract_cwd ------------------------------------------------------
PAYLOAD='{"session_id":"x","cwd":"/home/u/foo","tool":"Read"}'
assert_eq "extract cwd from payload"     "/home/u/foo" "$(ai_memory_extract_cwd "$PAYLOAD")"
assert_eq "extract cwd from empty json"  ""            "$(ai_memory_extract_cwd '{}')"
PAYLOAD_NESTED='{"session_id":"x","cwd":"/home/u/root","tool_input":{"cwd":"/tmp/nested"}}'
assert_eq "extract cwd prefers first match" "/home/u/root" "$(ai_memory_extract_cwd "$PAYLOAD_NESTED")"
PAYLOAD_AGY='{"conversationId":"x","workspacePaths":["/home/u/agy","/tmp/other"]}'
assert_eq "extract cwd from antigravity workspacePaths" "/home/u/agy" "$(ai_memory_extract_cwd "$PAYLOAD_AGY")"
PAYLOAD_WINDOWS='{"session_id":"x","cwd":"C:\\dev\\myproject"}'
assert_eq "extract cwd unescapes Windows JSON path" 'C:\dev\myproject' \
    "$(ai_memory_extract_cwd "$PAYLOAD_WINDOWS")"
# Cursor sends the workspace directory only as `workspace_roots`: its
# sessionStart omits `cwd` and its tool events send `cwd: ""`. Both must
# resolve or every Cursor event is filed under the default scratch project.
PAYLOAD_CURSOR_START='{"session_id":"x","hook_event_name":"sessionStart","cursor_version":"2026.09.02","workspace_roots":["/home/u/cur"]}'
assert_eq "extract cwd from cursor workspace_roots" "/home/u/cur" \
    "$(ai_memory_extract_cwd "$PAYLOAD_CURSOR_START")"
PAYLOAD_CURSOR_TOOL='{"session_id":"x","cwd":"","hook_event_name":"postToolUse","workspace_roots":["/home/u/cur"]}'
assert_eq "extract cwd falls through cursor empty cwd" "/home/u/cur" \
    "$(ai_memory_extract_cwd "$PAYLOAD_CURSOR_TOOL")"
# A large tool result must not make extraction quadratic in the payload
# size: `${payload#*"key"}` pinned a CPU for minutes at a few hundred KB (#870).
BIG=$(awk 'BEGIN { for (i = 0; i < 262144; i++) printf "x" }')
PAYLOAD_CURSOR_BIG="{\"tool_output\":\"$BIG\",\"cwd\":\"\",\"workspace_roots\":[\"/home/u/big\"]}"
assert_eq "extract cwd from a 256 KB cursor tool payload" "/home/u/big" \
    "$(ai_memory_extract_cwd "$PAYLOAD_CURSOR_BIG")"
# Only the first occurrence of a key is read, so a non-string top-level value
# never lets a nested field of the same name take its place.
PAYLOAD_NULL_CWD='{"cwd":null,"tool_input":{"cwd":"/tmp/nested"},"workspace_roots":["/home/u/top"]}'
assert_eq "extract cwd ignores nested cwd after a null one" "/home/u/top" \
    "$(ai_memory_extract_cwd "$PAYLOAD_NULL_CWD")"
PAYLOAD_EMPTY_ROOTS='{"cwd":"","workspace_roots":[],"tool_input":{"workspace_roots":["/tmp/nested"]}}'
assert_eq "extract cwd ignores nested roots after an empty array" "" \
    "$(ai_memory_extract_cwd "$PAYLOAD_EMPTY_ROOTS")"

antigravity_initial() {
    if ai_memory_antigravity_is_initial_invocation "$1"; then
        printf 'yes'
    else
        printf 'no'
    fi
}
assert_eq "antigravity invocation zero is initial" "yes" \
    "$(antigravity_initial '{"invocationNum":0,"conversationId":"agy"}')"
assert_eq "antigravity later invocation is not initial" "no" \
    "$(antigravity_initial '{"invocationNum":3,"conversationId":"agy"}')"
assert_eq "antigravity missing invocation fails closed" "no" \
    "$(antigravity_initial '{"conversationId":"agy"}')"
assert_eq "antigravity quoted invocation fails closed" "no" \
    "$(antigravity_initial '{"invocationNum":"0","conversationId":"agy"}')"
assert_eq "antigravity fractional invocation fails closed" "no" \
    "$(antigravity_initial '{"invocationNum":0.5,"conversationId":"agy"}')"
assert_eq "extract antigravity conversation id" "agy" \
    "$(ai_memory_extract_session_id '{"conversationId":"agy"}')"
PAYLOAD_AGY_BIG="{\"prompt\":\"$BIG\",\"invocationNum\":0,\"conversationId\":\"agy\"}"
assert_eq "antigravity initial invocation after a 256 KB field" "yes" \
    "$(antigravity_initial "$PAYLOAD_AGY_BIG")"
assert_eq "extract session id after a 256 KB field" "agy" \
    "$(ai_memory_extract_session_id "$PAYLOAD_AGY_BIG")"

FAKE_CURL_BIN="$TMP/fake-curl-bin"
FAKE_CURL_LOG="$TMP/fake-curl.log"
mkdir -p "$FAKE_CURL_BIN"
cat >"$FAKE_CURL_BIN/curl" <<'EOF'
#!/bin/sh
printf '%s\n' "$*" >>"$AI_MEMORY_CURL_LOG"
case "$*" in
    *'/handoff?'*) printf 'next-session-context' ;;
esac
EOF
chmod +x "$FAKE_CURL_BIN/curl"

rm -f "$FAKE_CURL_LOG"
AGY_LATER_OUTPUT=$(printf '%s' '{"invocationNum":2,"conversationId":"agy","workspacePaths":["/work"]}' \
    | PATH="$FAKE_CURL_BIN:$PATH" AI_MEMORY_CURL_LOG="$FAKE_CURL_LOG" \
        AI_MEMORY_HOOK_URL='http://memory.test' sh hooks/antigravity-cli/session-start.sh)
assert_eq "antigravity later invocation returns empty hook output" "{}" "$AGY_LATER_OUTPUT"
assert_eq "antigravity later invocation makes no HTTP request" "0" \
    "$([ -f "$FAKE_CURL_LOG" ] && wc -l <"$FAKE_CURL_LOG" | tr -d ' ' || printf '0')"

rm -f "$FAKE_CURL_LOG"
AGY_INITIAL_OUTPUT=$(printf '%s' '{"invocationNum":0,"conversationId":"agy","workspacePaths":["/work"]}' \
    | PATH="$FAKE_CURL_BIN:$PATH" AI_MEMORY_CURL_LOG="$FAKE_CURL_LOG" \
        AI_MEMORY_HOOK_URL='http://memory.test' sh hooks/antigravity-cli/session-start.sh)
assert_eq "antigravity initial invocation injects handoff" \
    '{"injectSteps":[{"ephemeralMessage":"next-session-context"}]}' "$AGY_INITIAL_OUTPUT"
assert_eq "antigravity initial invocation posts then fetches" "2" \
    "$(wc -l <"$FAKE_CURL_LOG" | tr -d ' ')"
assert_eq "antigravity handoff fetch carries conversation id" "yes" \
    "$(grep -q '/handoff?.*session_id=agy' "$FAKE_CURL_LOG" && printf 'yes' || printf 'no')"

PS_ANTIGRAVITY_STATIC=$(grep -q 'function Test-AiMemoryAntigravityInitialInvocation' hooks/lib/ai-memory-hook.ps1 \
    && grep -q '\$AntigravityPreInvocationOutput -and -not' hooks/lib/ai-memory-hook.ps1 \
    && printf 'ok' || printf 'missing')
assert_eq "powershell antigravity hook has invocation guard" "ok" "$PS_ANTIGRAVITY_STATIC"

PS_UTF8_STATIC=$(grep -Fq '$BodyBytes = [Text.Encoding]::UTF8.GetBytes($Payload)' hooks/lib/ai-memory-hook.ps1 \
    && grep -Fq 'application/json; charset=utf-8' hooks/lib/ai-memory-hook.ps1 \
    && grep -Fq -- '-Body $BodyBytes' hooks/lib/ai-memory-hook.ps1 \
    && printf 'ok' || printf 'missing')
assert_eq "powershell hook posts explicit UTF-8 JSON bytes" "ok" "$PS_UTF8_STATIC"
PS_HOME_STATIC=$(grep -Fq '$userHome = if ($env:HOME)' hooks/lib/ai-memory-hook.ps1 \
    && ! grep -Eq '\$home[[:space:]]*=' hooks/lib/ai-memory-hook.ps1 \
    && printf 'ok' || printf 'missing')
assert_eq "powershell marker helper avoids read-only HOME" "ok" "$PS_HOME_STATIC"

# --- json_string -------------------------------------------------------
JSON_INPUT='quoted "thing" \ path
next line'
assert_eq "json_string escapes text" '"quoted \"thing\" \\ path\nnext line"' \
    "$(printf '%s' "$JSON_INPUT" | ai_memory_json_string)"

# A raw control byte (JSON forbids U+0000..U+001F inside a string) must become
# a \u00XX escape, not reach stdout bare -- a replayed ANSI-coloured tool
# result otherwise made the whole SessionStart packet invalid JSON (#732).
CTRL_OUT="$(printf 'a\033[0mb' | ai_memory_json_string)"
case "$CTRL_OUT" in
    *'\u001b'*) CTRL_ESCAPED=yes ;;
    *) CTRL_ESCAPED=no ;;
esac
assert_eq "json_string escapes a control byte as a \u escape" "yes" "$CTRL_ESCAPED"

# --- marker_qs --------------------------------------------------------
QS=$(ai_memory_marker_qs "$TMP/a/b/c")
assert_eq "marker_qs single key" "&cwd=$(ai_memory_url_encode "$TMP/a/b/c")&workspace=deep" "$QS"

printf 'workspace = "ws1"\nproject = "p1"\nproject_strategy = "repo-root"\n' >"$TMP/a/b/.ai-memory.toml"
QS2=$(ai_memory_marker_qs "$TMP/a/b/c")
assert_eq "closer marker wins" "&cwd=$(ai_memory_url_encode "$TMP/a/b/c")&workspace=ws1&project=p1&project_src=marker&project_strategy=repo-root" "$QS2"

QS3=$(ai_memory_marker_qs "$TMP/nonexistent")
assert_eq "no marker -> cwd only" "&cwd=$(ai_memory_url_encode "$TMP/nonexistent")" "$QS3"

# --- capture-only marker transparency (#668) ---------------------------
# A nested marker whose only content is [capture] must not shadow an outer
# marker's workspace/project: ai_memory_marker_qs skips it and forwards the
# OUTER marker's fields, while ai_memory_find_marker (used for [capture]
# itself) still resolves the INNER (nearest) marker.
mkdir -p "$TMP/scope/inner"
printf 'workspace = "acme"\nproject = "infra"\n' >"$TMP/scope/.ai-memory.toml"
printf '[capture]\nignore_paths = ["secret/**"]\n' >"$TMP/scope/inner/.ai-memory.toml"

assert_eq "capture-only marker: find_marker still resolves nearest" \
    "$TMP/scope/inner/.ai-memory.toml" \
    "$(ai_memory_find_marker "$TMP/scope/inner")"
assert_eq "capture-only marker: find_settings_marker skips it for the outer" \
    "$TMP/scope/.ai-memory.toml" \
    "$(ai_memory_find_settings_marker "$TMP/scope/inner")"
assert_eq "capture-only marker: marker_qs forwards the OUTER scope" \
    "&cwd=$(ai_memory_url_encode "$TMP/scope/inner")&workspace=acme&project=infra&project_src=marker" \
    "$(ai_memory_marker_qs "$TMP/scope/inner")"

# A marker declaring [briefing] but no workspace/project is NOT capture-only
# (it declares a forwarded setting), so it stays a resolution boundary: the
# outer marker's scope must not leak through it.
printf '[briefing]\ninject_on_session_start = true\n' >"$TMP/scope/inner/.ai-memory.toml"
assert_eq "briefing-only marker is a settings boundary, not transparent" \
    "" "$(ai_memory_parse_toml_key "$(ai_memory_find_settings_marker "$TMP/scope/inner")" workspace)"
assert_eq "marker_qs stops at the briefing-only boundary" \
    "&cwd=$(ai_memory_url_encode "$TMP/scope/inner")" \
    "$(ai_memory_marker_qs "$TMP/scope/inner")"

# A capture-only marker with no scope-declaring ancestor: still transparent,
# and resolution falls back exactly as it does with no marker at all.
mkdir -p "$TMP/no-outer-scope/inner"
printf '[capture]\nignore_paths = ["a/**"]\n' >"$TMP/no-outer-scope/inner/.ai-memory.toml"
assert_eq "capture-only marker with no ancestor scope: settings walk finds none" \
    "" "$(ai_memory_find_settings_marker "$TMP/no-outer-scope/inner")"
assert_eq "capture-only marker with no ancestor scope: marker_qs is cwd-only" \
    "&cwd=$(ai_memory_url_encode "$TMP/no-outer-scope/inner")" \
    "$(ai_memory_marker_qs "$TMP/no-outer-scope/inner")"

# --- repo-root strategy: host-side resolution -------------------------
# Outside any git repo the helper stays silent (caller keeps basename(cwd)).
assert_eq "repo_root_project on non-git path is empty" "" \
    "$(ai_memory_repo_root_project "$TMP/nonexistent")"

if command -v git >/dev/null 2>&1; then
    REPO="$TMP/repos/acme-api"
    mkdir -p "$REPO"
    git init -q "$REPO"
    git -C "$REPO" -c user.email=t@example.com -c user.name=t \
        commit -q --no-gpg-sign --allow-empty -m init

    # A subdirectory of the main checkout collapses to the repo basename
    # (not the subdir name) when the marker selects repo-root and pins no
    # explicit project.
    mkdir -p "$REPO/crates/cli"
    printf 'workspace = "oss"\nproject_strategy = "repo-root"\n' >"$REPO/.ai-memory.toml"
    QSR=$(ai_memory_marker_qs "$REPO/crates/cli")
    assert_eq "repo-root: subdir resolves to repo basename" \
        "&cwd=$(ai_memory_url_encode "$REPO/crates/cli")&workspace=oss&project=acme-api&project_src=repo-root&project_strategy=repo-root" \
        "$QSR"

    rm -f "$REPO/.ai-memory.toml"
    AI_MEMORY_PROJECT_STRATEGY=repo-root
    export AI_MEMORY_PROJECT_STRATEGY
    QSE=$(ai_memory_marker_qs "$REPO/crates/cli")
    assert_eq "repo-root env: no marker resolves to repo basename" \
        "&cwd=$(ai_memory_url_encode "$REPO/crates/cli")&project=acme-api&project_src=repo-root&project_strategy=repo-root" \
        "$QSE"

    printf 'workspace = "oss"\nproject = "pinned"\nproject_strategy = "basename"\n' \
        >"$REPO/.ai-memory.toml"
    QSO=$(ai_memory_marker_qs "$REPO/crates/cli")
    assert_eq "marker project strategy overrides env default" \
        "&cwd=$(ai_memory_url_encode "$REPO/crates/cli")&workspace=oss&project=pinned&project_src=marker&project_strategy=basename" \
        "$QSO"
    unset AI_MEMORY_PROJECT_STRATEGY

    printf 'workspace = "oss"\nproject_strategy = "repo-root"\n' >"$REPO/.ai-memory.toml"

    # A linked worktree whose directory lives OUTSIDE the main repo tree
    # (a common layout: tools that keep worktrees in a separate directory)
    # has no .ai-memory.toml ancestor of its own, yet still collapses to the
    # MAIN repo basename via the commondir pointer. The strategy comes from a
    # marker placed above the worktrees directory.
    WT="$TMP/worktrees/acme-api/wt-feature"
    mkdir -p "$TMP/worktrees/acme-api"
    printf 'workspace = "oss"\nproject_strategy = "repo-root"\n' >"$TMP/worktrees/.ai-memory.toml"
    if git -C "$REPO" worktree add -q "$WT" >/dev/null 2>&1; then
        QSW=$(ai_memory_marker_qs "$WT")
        assert_eq "repo-root: out-of-tree worktree collapses to main repo" \
            "&cwd=$(ai_memory_url_encode "$WT")&workspace=oss&project=acme-api&project_src=repo-root&project_strategy=repo-root" \
            "$QSW"
    fi

    # An explicit project pin always wins over repo-root resolution.
    printf 'workspace = "oss"\nproject = "pinned"\nproject_strategy = "repo-root"\n' \
        >"$REPO/.ai-memory.toml"
    QSP=$(ai_memory_marker_qs "$REPO/crates/cli")
    assert_eq "explicit project pin beats repo-root" \
        "&cwd=$(ai_memory_url_encode "$REPO/crates/cli")&workspace=oss&project=pinned&project_src=marker&project_strategy=repo-root" \
        "$QSP"

    PSH=""
    if command -v pwsh >/dev/null 2>&1; then
        PSH=$(command -v pwsh)
    elif command -v powershell >/dev/null 2>&1; then
        PSH=$(command -v powershell)
    fi
    if [ -n "$PSH" ]; then
        PS_LIB=$(host_path "$PWD/hooks/lib/ai-memory-hook.ps1")
        PS_CWD=$(host_path "$REPO/crates/cli")
        PS_REPO=$("$PSH" -NoProfile -ExecutionPolicy Bypass -Command \
            ". '$PS_LIB'; Get-AiMemoryRepoRootProject -Cwd '$PS_CWD'")
        assert_eq "powershell repo-root helper resolves repo basename" "acme-api" "$PS_REPO"
    else
        PS_STATIC=$(grep -q 'function Get-AiMemoryRepoRootProject' hooks/lib/ai-memory-hook.ps1 \
            && grep -q -- '--git-common-dir' hooks/lib/ai-memory-hook.ps1 \
            && grep -q 'Get-AiMemoryRepoRootProject -Cwd' hooks/lib/ai-memory-hook.ps1 \
            && printf 'ok' || printf 'missing')
        assert_eq "powershell repo-root helper has static parity" "ok" "$PS_STATIC"
    fi
fi

# --- url_encode -------------------------------------------------------
assert_eq "url_encode passes safe slug"   "movvia" "$(ai_memory_url_encode "movvia")"
assert_eq "url_encode escapes ampersand"  "a%26b"  "$(ai_memory_url_encode "a&b")"
assert_eq "url_encode escapes equals"     "a%3Db"  "$(ai_memory_url_encode "a=b")"
assert_eq "url_encode escapes plus"       "a%2Bb"  "$(ai_memory_url_encode "a+b")"
assert_eq "url_encode escapes Windows cwd" "C%3A%5Cdev%5Cmyproject" \
    "$(ai_memory_url_encode 'C:\dev\myproject')"
assert_eq "url_encode encodes UTF-8 per byte" "r%C3%A9po" "$(ai_memory_url_encode 'répo')"

# --- offline spool ----------------------------------------------------
# The spool dir follows the data dir, which the harness pins inside $TMP.
AI_MEMORY_DATA_DIR="$TMP/spool-data"
export AI_MEMORY_DATA_DIR

MS=$(ai_memory_now_ms)
assert_eq "now_ms is 13 digits" "13" "$(printf '%s' "$MS" | wc -c | tr -d ' ')"
case "$MS" in
    *[!0-9]*) assert_eq "now_ms is all digits" "digits" "$MS" ;;
    *) assert_eq "now_ms is all digits" "digits" "digits" ;;
esac

# A body carrying every escape ai_memory_json_string emits must survive the
# write/read round trip byte for byte, or a drained event is corrupted.
SPOOL_BODY='{"t":"quote \" backslash \\ newline
tab\ttail"}'
ai_memory_spool_event "http://127.0.0.1:1/hook?event=stop&agent=cursor" "$SPOOL_BODY"
SPOOL_FILE=$(ls "$TMP/spool-data/hook-spool/"*.json 2>/dev/null | head -n 1)
assert_eq "spool_event writes one entry" "1" \
    "$(ls "$TMP/spool-data/hook-spool/"*.json 2>/dev/null | wc -l | tr -d ' ')"
assert_eq "spooled body round-trips" "$SPOOL_BODY" "$(ai_memory_json_field body "$SPOOL_FILE")"
case "$(ai_memory_json_field url "$SPOOL_FILE")" in
    *ingest_key=sh*) SPOOL_HAS_INGEST_KEY=yes ;;
    *) SPOOL_HAS_INGEST_KEY=no ;;
esac
assert_eq "spool_event mints an ingest_key" "yes" "$SPOOL_HAS_INGEST_KEY"
assert_eq "spool entry is 0600" "600" \
    "$(ls -l "$SPOOL_FILE" | cut -c2-10 | tr 'rwx-' '4210' | awk '{print substr($0,1,3)+0 substr($0,4,3)+0 substr($0,7,3)+0}' >/dev/null 2>&1; \
       if [ -r "$SPOOL_FILE" ] && [ ! -x "$SPOOL_FILE" ]; then printf '600'; else printf 'other'; fi)"
assert_eq "spool filename is <ms>-<pid>-<seq>.json" "ok" \
    "$(basename "$SPOOL_FILE" | grep -Eq '^[0-9]{13}-[0-9]+-[0-9a-f]{16}\.json$' && printf ok || printf bad)"

# A `\uXXXX` escape means a richer serializer wrote the entry (the native
# binary). The shell reader declines it rather than mangling the payload, so
# `ai-memory hook-drain` still delivers it.
printf '%s' '{"url":"http://x/y","body":"{\"a\":\"\u0007\"}","created_ms":1,"auth_mode":"none","attempts":0}' \
    >"$TMP/spool-data/hook-spool/foreign.json"
ai_memory_json_field body "$TMP/spool-data/hook-spool/foreign.json" >/dev/null 2>&1 \
    && FOREIGN=read || FOREIGN=declined
assert_eq "json_field declines a \\u escape" "declined" "$FOREIGN"
rm -f "$TMP/spool-data/hook-spool/foreign.json"

# A timeout can hide a successful server write. The initial attempt and the
# spooled replay must carry the same key so the server can reject the replay.
rm -f "$TMP/spool-data/hook-spool/"*.json
CURL_ATTEMPT_FILE="$TMP/curl-attempt-url"
curl() {
    while [ "$#" -gt 0 ]; do
        case "$1" in
            http://* | https://*) printf '%s' "$1" >"$CURL_ATTEMPT_FILE" ;;
        esac
        shift
    done
    cat >/dev/null
    return 28
}
printf '%s' '{"e":"ambiguous"}' \
    | ai_memory_post_hook "http://127.0.0.1:49374/hook?event=stop&agent=cursor" >/dev/null 2>&1
unset -f curl
SPOOL_FILE=$(ls "$TMP/spool-data/hook-spool/"*.json 2>/dev/null | head -n 1)
ATTEMPT_URL=$(cat "$CURL_ATTEMPT_FILE")
SPOOL_URL=$(ai_memory_json_field url "$SPOOL_FILE")
case "$ATTEMPT_URL" in
    *ingest_key=sh*) ATTEMPT_HAS_INGEST_KEY=yes ;;
    *) ATTEMPT_HAS_INGEST_KEY=no ;;
esac
assert_eq "post_hook keys the initial delivery" "yes" "$ATTEMPT_HAS_INGEST_KEY"
assert_eq "post_hook preserves the key after an ambiguous delivery" \
    "$ATTEMPT_URL" "$SPOOL_URL"

# An unreachable server must leave the event on disk instead of dropping it.
rm -f "$TMP/spool-data/hook-spool/"*.json
printf '%s' '{"e":"unreachable"}' \
    | ai_memory_post_hook "http://127.0.0.1:1/hook?event=post-tool-use&agent=cursor" >/dev/null 2>&1
assert_eq "post_hook spools an undelivered event" "1" \
    "$(ls "$TMP/spool-data/hook-spool/"*.json 2>/dev/null | wc -l | tr -d ' ')"

unset AI_MEMORY_DATA_DIR

# --- external capture ownership (AI_MEMORY_CAPTURE_OWNER) -------------
# A wrapper, extension or managed launcher that already produces this
# session's capture events announces itself with AI_MEMORY_CAPTURE_OWNER.
# The bundle must then stop PRODUCING events (no POST, no spool entry, no
# piggyback drain) while still DELIVERING: the synchronous handoff GET keeps
# working, a backlog already on disk stays put, and an explicit drain still
# ships it. Every case below runs the real `ai_memory_post_hook` /
# `ai_memory_get_handoff` / `ai_memory_drain_spool` path with curl stubbed,
# so what is asserted is what the shipped functions do.

# Stub curl: log method + URL, answer 200 to a POST and a body to a GET.
# Only `--data-binary @-` reads the body from stdin. Consuming it for every
# POST would block on an inherited terminal stdin the moment a caller passes
# `@file` or an inline body, which the real curl never touches.
curl() {
    _tm=GET
    _tu=""
    _tdata=""
    while [ "$#" -gt 0 ]; do
        case "$1" in
            -X) shift; [ "$#" -gt 0 ] || break; _tm="$1" ;;
            --data-binary) shift; [ "$#" -gt 0 ] || break; _tdata="$1" ;;
            --data-binary=*) _tdata="${1#--data-binary=}" ;;
            http://* | https://*) _tu="$1" ;;
        esac
        shift
    done
    if [ "$_tdata" = "@-" ]; then
        cat >/dev/null
    fi
    if [ "$_tm" = POST ]; then
        printf 'POST %s\n' "$_tu" >>"$CURL_LOG"
        printf '200'
    else
        printf 'GET %s\n' "$_tu" >>"$CURL_LOG"
        printf 'HANDOFF BODY'
    fi
    return 0
}

# Each case gets its own data dir, so the spool and the curl log start empty
# without anything having to be cleared between cases.
owner_case() {
    AI_MEMORY_DATA_DIR="$TMP/owner-$1"
    export AI_MEMORY_DATA_DIR
    mkdir -p "$AI_MEMORY_DATA_DIR"
    CURL_LOG="$AI_MEMORY_DATA_DIR/curl.log"
    touch "$CURL_LOG"
}
owner_requests() { awk 'END { print NR + 0 }' "$CURL_LOG"; }
owner_posts()    { awk '/^POST /{ n++ } END { print n + 0 }' "$CURL_LOG"; }
owner_spooled()  { ls "$AI_MEMORY_DATA_DIR/hook-spool/"*.json 2>/dev/null | wc -l | tr -d ' '; }

# 1. Owner set: an event produced now is dropped whole, and the backlog that
#    was already queued is neither extended nor drained behind it.
owner_case suppressed
ai_memory_spool_event "http://127.0.0.1:1/hook?event=stop&agent=cursor" '{"e":"backlog"}'
BACKLOG=$(owner_spooled)
assert_eq "owner: backlog is queued before the gate" "1" "$BACKLOG"
AI_MEMORY_CAPTURE_OWNER="external-runner"
export AI_MEMORY_CAPTURE_OWNER
printf '%s' '{"e":"owned"}' \
    | ai_memory_post_hook "http://127.0.0.1:49374/hook?event=post-tool-use&agent=cursor" >/dev/null 2>&1
assert_eq "owner: post_hook sends nothing" "0" "$(owner_requests)"
assert_eq "owner: post_hook spools nothing and drains nothing" "$BACKLOG" "$(owner_spooled)"

# 2. Delivery is untouched: the handoff GET still runs and still returns.
HANDOFF=$(set +e; ai_memory_get_handoff "http://127.0.0.1:49374/handoff?agent=cursor")
assert_eq "owner: handoff GET still delivered" "HANDOFF BODY" "$HANDOFF"
assert_eq "owner: the GET is the only request" "1" "$(owner_requests)"
assert_eq "owner: still no POST"               "0" "$(owner_posts)"

# 3. Standalone control: with no owner the same call POSTs as before.
unset AI_MEMORY_CAPTURE_OWNER
owner_case control
printf '%s' '{"e":"control"}' \
    | ai_memory_post_hook "http://127.0.0.1:49374/hook?event=post-tool-use&agent=cursor" >/dev/null 2>&1
assert_eq "no owner: post_hook POSTs" "1" "$(owner_posts)"

# 4. An exported-but-blank variable must not disable capture: empty and
#    whitespace-only keep the default behaviour.
AI_MEMORY_CAPTURE_OWNER=""
export AI_MEMORY_CAPTURE_OWNER
owner_case empty
printf '%s' '{"e":"empty"}' \
    | ai_memory_post_hook "http://127.0.0.1:49374/hook?event=post-tool-use&agent=cursor" >/dev/null 2>&1
assert_eq "empty owner: post_hook POSTs" "1" "$(owner_posts)"

AI_MEMORY_CAPTURE_OWNER=$(printf ' \t ')
export AI_MEMORY_CAPTURE_OWNER
owner_case whitespace
printf '%s' '{"e":"blank"}' \
    | ai_memory_post_hook "http://127.0.0.1:49374/hook?event=post-tool-use&agent=cursor" >/dev/null 2>&1
assert_eq "whitespace owner: post_hook POSTs" "1" "$(owner_posts)"

# 5. An EXPLICIT drain is delivery, not production: it must still ship a
#    queued event while the owner is set, and retire it on a 2xx.
unset AI_MEMORY_CAPTURE_OWNER
owner_case drain
ai_memory_spool_event "http://127.0.0.1:49374/hook?event=stop&agent=cursor" '{"e":"pending"}'
AI_MEMORY_CAPTURE_OWNER="external-runner"
export AI_MEMORY_CAPTURE_OWNER
ai_memory_drain_spool 64 >/dev/null 2>&1
assert_eq "owner: explicit drain still delivers the backlog" "1" "$(owner_posts)"
assert_eq "owner: delivered entry is retired"                "0" "$(owner_spooled)"

unset -f curl
unset AI_MEMORY_CAPTURE_OWNER
unset AI_MEMORY_DATA_DIR

# --- summary ----------------------------------------------------------
printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
