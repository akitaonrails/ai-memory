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

# --- marker_qs --------------------------------------------------------
QS=$(ai_memory_marker_qs "$TMP/a/b/c")
assert_eq "marker_qs single key" "&cwd=$(ai_memory_url_encode "$TMP/a/b/c")&workspace=deep" "$QS"

printf 'workspace = "ws1"\nproject = "p1"\nproject_strategy = "repo-root"\n' >"$TMP/a/b/.ai-memory.toml"
QS2=$(ai_memory_marker_qs "$TMP/a/b/c")
assert_eq "closer marker wins" "&cwd=$(ai_memory_url_encode "$TMP/a/b/c")&workspace=ws1&project=p1&project_src=marker&project_strategy=repo-root" "$QS2"

QS3=$(ai_memory_marker_qs "$TMP/nonexistent")
assert_eq "no marker -> cwd only" "&cwd=$(ai_memory_url_encode "$TMP/nonexistent")" "$QS3"

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
        PS_REPO=$($PSH -NoProfile -ExecutionPolicy Bypass -Command \
            ". '$PWD/hooks/lib/ai-memory-hook.ps1'; Get-AiMemoryRepoRootProject -Cwd '$REPO/crates/cli'")
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

# --- summary ----------------------------------------------------------
printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
