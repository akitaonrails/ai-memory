#!/bin/sh
# harness-issue #998 regression: a `[briefing] inject_on_session_start`
# marker must make it onto the actual `/handoff` GET a session-start bundle
# sends, not just onto the `ai_memory_briefing_qs` helper in isolation
# (which `tests/hooks/test_lib.sh` already covers). Nine of eleven bundles
# built their query from `ai_memory_marker_qs` alone and silently dropped the
# two briefing keys; `kiro-cli` was the only one that already called
# `ai_memory_briefing_qs`, so it is the control case below.
#
# Run from the repo root:
#
#   sh tests/hooks/test_session_start_briefing.sh
#
# Exits non-zero on any failure. POSIX shell only, no running server: a
# `curl` stub placed first on PATH logs every invocation's URL instead of
# making a real request, so this needs no network and no mock HTTP server.
set -eu

ROOT_DIR="$(CDPATH='' cd -- "$(dirname "$0")/../.." && pwd)"
PASS=0
FAIL=0
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
# Isolate the once-per-session "briefed" marker state (ai_memory_state_dir)
# from any real ~/.local/share/ai-memory and from one run of this script to
# the next -- otherwise a fixed session id like "sess-claude-code-1" below
# would read as already-briefed on a second run.
AI_MEMORY_DATA_DIR="$TMP/data"
export AI_MEMORY_DATA_DIR

assert_contains() {
    desc="$1"; haystack="$2"; needle="$3"
    case "$haystack" in
        *"$needle"*)
            PASS=$((PASS + 1))
            printf '  ok  %s\n' "$desc"
            ;;
        *)
            FAIL=$((FAIL + 1))
            printf '  FAIL %s\n    needle=%s\n    haystack=%s\n' "$desc" "$needle" "$haystack"
            ;;
    esac
}

assert_not_contains() {
    desc="$1"; haystack="$2"; needle="$3"
    case "$haystack" in
        *"$needle"*)
            FAIL=$((FAIL + 1))
            printf '  FAIL %s (should not contain %s)\n    haystack=%s\n' "$desc" "$needle" "$haystack"
            ;;
        *)
            PASS=$((PASS + 1))
            printf '  ok  %s\n' "$desc"
            ;;
    esac
}

# A fake `curl` ahead of the real one on PATH: logs the full invocation (one
# line per call) and exits 0 printing nothing, which `ai_memory_get_handoff`
# and `ai_memory_post_hook` both treat as a valid, empty response.
STUB_BIN="$TMP/bin"
CURL_LOG="$TMP/curl.log"
mkdir -p "$STUB_BIN"
: >"$CURL_LOG"
cat >"$STUB_BIN/curl" <<EOF
#!/bin/sh
printf '%s\n' "\$*" >>"$CURL_LOG"
exit 0
EOF
chmod +x "$STUB_BIN/curl"
PATH="$STUB_BIN:$PATH"
export PATH

run_bundle() {
    bundle="$1"; cwd="$2"; session_id="$3"
    qcwd=$(printf '%s' "$cwd" | sed 's/\\/\\\\/g; s/"/\\"/g; s/^/"/; s/$/"/')
    qsid=$(printf '%s' "$session_id" | sed 's/\\/\\\\/g; s/"/\\"/g; s/^/"/; s/$/"/')
    extra=""
    # Antigravity's PreInvocation hook only treats invocationNum=0 as a
    # session start; anything else (including a missing field) is a no-op by
    # design (ai_memory_antigravity_is_initial_invocation).
    [ "$bundle" = "antigravity-cli" ] && extra=',"invocationNum":0'
    payload=$(printf '{"cwd":%s,"session_id":%s%s}' "$qcwd" "$qsid" "$extra")
    printf '%s' "$payload" | sh "$ROOT_DIR/hooks/$bundle/session-start.sh" >/dev/null 2>&1 || true
}

last_handoff_request() {
    # The handoff GET is the invocation naming "/handoff" (the capture POST
    # names "/hook" instead); the briefing keys, if present, ride on it.
    grep '/handoff' "$CURL_LOG" | tail -n 1
}

for bundle in antigravity-cli claude-code codex command-code cursor devin gemini-cli opencode kiro-cli; do
    REPO="$TMP/repo-$bundle"
    mkdir -p "$REPO"
    cat >"$REPO/.ai-memory.toml" <<EOF
workspace = "default"
project = "briefing-probe-$bundle"

[briefing]
inject_on_session_start = true
max_chars = 4000
EOF
    : >"$CURL_LOG"
    run_bundle "$bundle" "$REPO" "sess-$bundle-1"
    req=$(last_handoff_request)
    assert_contains  "$bundle: first session-start fetch carries briefing=" "$req" "briefing="
    assert_contains  "$bundle: first session-start fetch carries briefing_budget=" "$req" "briefing_budget=4000"

    # Once-per-session: a second session-start with the SAME session id must
    # not re-request the brief (the gate `ai_memory_briefed_file` writes).
    # Devin has no native session id of its own: `ai_memory_session_id_qs`
    # mints a fresh synthetic one on every "session-start" event by design
    # (it only reads the persisted file for a *different* event name), so
    # there is no real "repeat session-start, same session" case to assert
    # for it -- every session-start IS a new session for devin.
    if [ "$bundle" != "devin" ]; then
        : >"$CURL_LOG"
        run_bundle "$bundle" "$REPO" "sess-$bundle-1"
        req=$(last_handoff_request)
        assert_not_contains "$bundle: repeat session-start (same session id) omits briefing=" "$req" "briefing="
    fi

    # A genuinely new session id gets the brief again.
    : >"$CURL_LOG"
    run_bundle "$bundle" "$REPO" "sess-$bundle-2"
    req=$(last_handoff_request)
    assert_contains "$bundle: a new session id gets the brief again" "$req" "briefing="
done

# Agents whose SessionStart does not inject a handoff
# (`AgentKind::session_start_injects_handoff` == false) must not fetch one at
# all, briefing included: grok, kimi-code and pool are not in the loop above
# on purpose. Confirm the negative directly for one of them.
for bundle in grok kimi-code pool; do
    REPO="$TMP/repo-$bundle"
    mkdir -p "$REPO"
    cat >"$REPO/.ai-memory.toml" <<EOF
workspace = "default"
project = "briefing-probe-$bundle"

[briefing]
inject_on_session_start = true
max_chars = 4000
EOF
    : >"$CURL_LOG"
    run_bundle "$bundle" "$REPO" "sess-$bundle-1"
    req=$(last_handoff_request)
    assert_eq_empty() {
        if [ -z "$2" ]; then
            PASS=$((PASS + 1)); printf '  ok  %s\n' "$1"
        else
            FAIL=$((FAIL + 1)); printf '  FAIL %s\n    got=%s\n' "$1" "$2"
        fi
    }
    assert_eq_empty "$bundle: never fetches a handoff from session-start, briefing or not" "$req"
done

printf '\n%d passed, %d failed\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
