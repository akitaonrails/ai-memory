#!/usr/bin/env bash
# End-to-end handoff + recall smoke test for ai-memory.
#
# What this validates:
#
# 1. Two real LLM calls against Gemini free-tier (different model
#    variants per session so any recall must come from ai-memory, not
#    a per-process cache or the same model "remembering" itself).
# 2. ai-memory's /hook ingress — observations land in the store.
# 3. ai-memory's auto-handoff creation at SessionEnd.
# 4. ai-memory's GET /handoff endpoint — the path SessionStart hooks
#    use to surface prior context to the next agent CLI.
# 5. Sanitisation end-to-end — a planted "sk-canary-LEAK_ME_PLEASE_…"
#    secret in session 1's prompt must NOT appear in any persisted
#    state (observations, wiki pages, handoff body, on-disk markdown).
#
# Why direct Gemini REST and not opencode/codex/claude-code:
# - opencode's `run` mode pays a ~3 min bootstrap (mise/node + sqlite
#   migration) per fresh data dir, and didn't reliably exit after the
#   LLM responded in our smoke tests. Not viable for an automated
#   test. The /hook endpoint is the same regardless of which agent
#   CLI POSTs to it, so the test injects hook events directly while
#   driving the LLM with the simplest possible client. The agent-CLI
#   integrations are still documented + shipped (see hooks/) and
#   exercised by the unit tests in ai-memory-hooks.
# - Gemini's free tier has per-user quotas (vs OpenRouter's shared
#   pool which was returning 429s when this test was written) and
#   responds in under 5 s for the prompts here.
#
# Required env: GEMINI_API_KEY. Get one free at
# https://aistudio.google.com/app/apikey (no credit card needed).
#
# Isolation: ai-memory's data dir + the on-the-fly opencode-style
# config live under a tempdir and are removed on exit. Re-runnable on
# any machine that has cargo + curl + jq.

set -euo pipefail

# --------------------------------------------------------------------
# Setup
# --------------------------------------------------------------------

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TEST_DIR="$(mktemp -d -t ai-memory-e2e-XXXXXX)"
LOG_FILE="$TEST_DIR/test.log"
SERVER_PID=""

cleanup() {
    local rc=$?
    if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
        kill "$SERVER_PID" 2>/dev/null || true
        wait "$SERVER_PID" 2>/dev/null || true
    fi
    if [[ $rc -ne 0 ]]; then
        echo
        echo "==== TEST FAILED (rc=$rc) ===="
        echo "Preserved for inspection: $TEST_DIR"
        echo "Log:                      $LOG_FILE"
    else
        rm -rf "$TEST_DIR"
    fi
    exit $rc
}
trap cleanup EXIT INT TERM

step() { printf '\n=== %s ===\n' "$*"; }

require_cmd() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "FATAL: required command '$1' not found in PATH" >&2
        exit 64
    }
}
require_cmd curl
require_cmd jq
require_cmd cargo

if [[ -z "${GEMINI_API_KEY:-}" ]]; then
    echo "FATAL: GEMINI_API_KEY not set." >&2
    echo "       Get one (free) at https://aistudio.google.com/app/apikey" >&2
    exit 64
fi

# Free port for ai-memory.
PORT="$(python3 -c "import socket; s=socket.socket(); s.bind(('127.0.0.1',0)); print(s.getsockname()[1]); s.close()")"
SERVER_URL="http://127.0.0.1:$PORT"
HOOK_SCOPE="workspace=e2e-test&project=blog"

# Two deliberately different Gemini variants. Both are free-tier;
# overridable via env when the defaults rotate out.
MODEL_A="${MODEL_A:-gemini-2.5-flash}"
MODEL_B="${MODEL_B:-gemini-2.5-flash-lite}"

# Isolate ai-memory's data dir; leave $HOME alone so cargo's target
# cache + the user's git config etc. stay accessible.
export AI_MEMORY_DATA_DIR="$TEST_DIR/ai-memory-data"
mkdir -p "$AI_MEMORY_DATA_DIR" "$TEST_DIR/blog"

# Mini blog project — non-coding topic so models don't "know" the
# answer absent the handoff.
cat >"$TEST_DIR/blog/TOPIC.md" <<'EOF'
# Blog draft: Rust Borrow Checker by Example
Plan (session 1):
- Three examples: closure captures + move, &mut aliasing, lifetime elision.
- ~1500 words, friendly tone.
EOF

# Canary that MUST be redacted end-to-end.
CANARY_KEY="sk-canary-LEAK_ME_PLEASE_e2e_smoketest_xxxxxxxxxxxx"

# Generated bearer token for this test run. The test starts ai-memory
# with AI_MEMORY_AUTH_TOKEN set, then exercises both the unauth (must
# 401) and auth (must 200) paths.
AUTH_TOKEN="testtoken-$(date +%s)-$(printf '%08x' $RANDOM$RANDOM)"
AUTH_HEADER="Authorization: Bearer $AUTH_TOKEN"

# --------------------------------------------------------------------
# Gemini helper
# --------------------------------------------------------------------

# Usage: gemini_call <model> <prompt-string>
# Echoes the model's text response to stdout. thinkingBudget=0
# disables reasoning tokens so maxOutputTokens is spent on the actual
# answer (gemini-2.5-flash is a reasoning model by default).
gemini_call() {
    local model="$1" prompt="$2"
    local body
    body="$(jq -n --arg p "$prompt" '{
        contents: [{ parts: [{ text: $p }] }],
        generationConfig: {
            maxOutputTokens: 800,
            thinkingConfig: { thinkingBudget: 0 }
        }
    }')"
    curl -sS --max-time 90 \
        "https://generativelanguage.googleapis.com/v1beta/models/$model:generateContent?key=$GEMINI_API_KEY" \
        -H "Content-Type: application/json" \
        -d "$body" \
        | jq -r '.candidates[0].content.parts[0].text // (.error.message | tostring | "GEMINI_ERROR: " + .)'
}

# --------------------------------------------------------------------
# Build + start ai-memory
# --------------------------------------------------------------------

step "Building ai-memory release binary"
cd "$REPO_ROOT"
cargo build --release --bin ai-memory --quiet 2>&1 | tee -a "$LOG_FILE"
AI_MEMORY="$REPO_ROOT/target/release/ai-memory"

step "Initialising ai-memory data dir at $AI_MEMORY_DATA_DIR"
"$AI_MEMORY" init >>"$LOG_FILE" 2>&1

step "Starting ai-memory server on $SERVER_URL (auth-required)"
AI_MEMORY_AUTH_TOKEN="$AUTH_TOKEN" \
"$AI_MEMORY" serve \
    --transport http --bind "127.0.0.1:$PORT" \
    --workspace e2e-test --project blog \
    >>"$LOG_FILE" 2>&1 &
SERVER_PID=$!
sleep 2
if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "FATAL: ai-memory server died on startup" >&2
    tail -30 "$LOG_FILE" >&2
    exit 1
fi
# Verify it's actually listening before continuing. /mcp without auth
# returns 401, which is fine — it proves the server is up AND that
# the auth layer is wired in. (Without auth we'd see 405 GET-not-allowed.)
for _ in 1 2 3 4 5; do
    code=$(curl -s -o /dev/null -w "%{http_code}" --max-time 2 "$SERVER_URL/mcp" 2>/dev/null || echo 000)
    [[ "$code" == "401" || "$code" == "405" || "$code" == "200" ]] && break
    sleep 1
done

# Auth-path assertions — the server must reject unauthenticated calls
# and accept authenticated ones. We check this BEFORE the main flow
# so a misconfigured server fails fast with a clear signal.
step "Auth probe: GET /handoff without token must 401"
unauth_code=$(curl -s -o /dev/null -w "%{http_code}" --max-time 3 \
    "$SERVER_URL/handoff?agent=open-code" 2>/dev/null || echo 000)
if [[ "$unauth_code" != "401" ]]; then
    echo "FAIL: expected 401 without auth, got $unauth_code" >&2
    exit 1
fi
echo "  PASS: unauth → 401"

step "Auth probe: GET /handoff with wrong token must 401"
wrong_code=$(curl -s -o /dev/null -w "%{http_code}" --max-time 3 \
    -H "Authorization: Bearer not-the-right-one" \
    "$SERVER_URL/handoff?agent=open-code" 2>/dev/null || echo 000)
if [[ "$wrong_code" != "401" ]]; then
    echo "FAIL: expected 401 with wrong token, got $wrong_code" >&2
    exit 1
fi
echo "  PASS: wrong-token → 401"

step "Auth probe: GET /handoff with right token must NOT be 401"
right_code=$(curl -s -o /dev/null -w "%{http_code}" --max-time 3 \
    -H "$AUTH_HEADER" \
    "$SERVER_URL/handoff?agent=open-code" 2>/dev/null || echo 000)
if [[ "$right_code" == "401" ]]; then
    echo "FAIL: right token also rejected, got $right_code" >&2
    exit 1
fi
echo "  PASS: right-token → $right_code (not 401)"

# --------------------------------------------------------------------
# Session 1 — model A. Plant the plan + the canary.
# --------------------------------------------------------------------

SESSION_ID_1="$(uuidgen 2>/dev/null || cat /proc/sys/kernel/random/uuid)"

step "Session 1 ($MODEL_A): session-start hook"
echo "{\"session_id\":\"$SESSION_ID_1\",\"cwd\":\"$TEST_DIR/blog\",\"model\":\"$MODEL_A\"}" \
    | curl -sS --max-time 3 \
        -X POST "$SERVER_URL/hook?event=session-start&agent=open-code&$HOOK_SCOPE" \
        -H "Content-Type: application/json" \
        -H "$AUTH_HEADER" \
        --data-binary @- >>"$LOG_FILE" 2>&1

step "Session 1: invoke model A with the plan + canary"
S1_PROMPT="I'm planning a blog post titled 'Rust Borrow Checker by Example'.
Three concrete examples — (1) closure captures and the move keyword,
(2) double-mutable references and the &mut aliasing rule,
(3) lifetime elision pitfalls at function boundaries.
Length: about 1500 words. Tone: friendly and conversational.

For the ops dashboard I'm also storing this admin token:
$CANARY_KEY (do not share).

Acknowledge the plan in one short paragraph; no changes."
S1_RESPONSE="$(gemini_call "$MODEL_A" "$S1_PROMPT")"
echo "$S1_RESPONSE" >"$TEST_DIR/session1_response.txt"
echo "--- model A response (first 30 lines) ---"
echo "$S1_RESPONSE" | head -30
echo "--- end model A response ---"

# Tell ai-memory what the user prompt was so a meaningful handoff
# is built at SessionEnd.
step "Session 1: forward user-prompt + session-end hooks"
echo "{\"session_id\":\"$SESSION_ID_1\",\"prompt\":$(jq -Rs <<<"$S1_PROMPT")}" \
    | curl -sS --max-time 3 \
        -X POST "$SERVER_URL/hook?event=user-prompt&agent=open-code&$HOOK_SCOPE" \
        -H "Content-Type: application/json" \
        -H "$AUTH_HEADER" \
        --data-binary @- >>"$LOG_FILE" 2>&1

# Synthetic "tool use" observation so the handoff's "next steps"
# section has tool names to surface.
for tool in "Read" "Edit" "Write"; do
    echo "{\"session_id\":\"$SESSION_ID_1\",\"tool\":\"$tool\"}" \
        | curl -sS --max-time 2 \
            -X POST "$SERVER_URL/hook?event=post-tool-use&agent=open-code&$HOOK_SCOPE" \
            -H "Content-Type: application/json" \
            -H "$AUTH_HEADER" \
            --data-binary @- >>"$LOG_FILE" 2>&1
done

echo "{\"session_id\":\"$SESSION_ID_1\",\"cwd\":\"$TEST_DIR/blog\"}" \
    | curl -sS --max-time 10 \
        -X POST "$SERVER_URL/hook?event=session-end&agent=open-code&$HOOK_SCOPE" \
        -H "Content-Type: application/json" \
        -H "$AUTH_HEADER" \
        --data-binary @- >>"$LOG_FILE" 2>&1

# Server work is async (writer actor + auto-commit). Give it a beat.
sleep 2

# --------------------------------------------------------------------
# Probe: handoff exists; canary already scrubbed at this point
# --------------------------------------------------------------------

step "Probe: fetch handoff via GET /handoff (authed)"
HANDOFF_MD="$(curl -sS --max-time 5 -H "$AUTH_HEADER" "$SERVER_URL/handoff?agent=open-code")"
echo "$HANDOFF_MD" >"$TEST_DIR/handoff.md"
echo "--- handoff body ---"
echo "$HANDOFF_MD"
echo "--- end handoff body ---"
[[ -n "$HANDOFF_MD" ]] || { echo "FAIL: /handoff returned empty" >&2; exit 1; }

# --------------------------------------------------------------------
# Session 2 — model B. The handoff is the only bridge.
# --------------------------------------------------------------------

SESSION_ID_2="$(uuidgen 2>/dev/null || cat /proc/sys/kernel/random/uuid)"

step "Session 2 ($MODEL_B): session-start hook"
echo "{\"session_id\":\"$SESSION_ID_2\",\"cwd\":\"$TEST_DIR/blog\",\"model\":\"$MODEL_B\"}" \
    | curl -sS --max-time 3 \
        -X POST "$SERVER_URL/hook?event=session-start&agent=open-code&$HOOK_SCOPE" \
        -H "Content-Type: application/json" \
        -H "$AUTH_HEADER" \
        --data-binary @- >>"$LOG_FILE" 2>&1

step "Session 2: invoke model B, with the handoff prepended"
S2_PROMPT="$(cat <<EOF
$HANDOFF_MD

---

The user has resumed work on the blog draft. Using only the handoff
above, summarise what was planned in the previous session: the title,
the three example topics, the length target, and the tone. Be
concrete — name each example. Then ask whether they want to start
drafting example 1.
EOF
)"
S2_RESPONSE="$(gemini_call "$MODEL_B" "$S2_PROMPT")"
echo "$S2_RESPONSE" >"$TEST_DIR/session2_response.txt"
echo "--- model B response ---"
echo "$S2_RESPONSE"
echo "--- end model B response ---"

# --------------------------------------------------------------------
# Assertions
# --------------------------------------------------------------------

PASS=0
FAIL=0
check_contains() {
    local label="$1" source="$2"
    shift 2
    local found=0
    for needle in "$@"; do
        if echo "$source" | grep -iqF "$needle"; then
            found=1
            break
        fi
    done
    if [[ $found -eq 1 ]]; then
        echo "  PASS: $label"
        PASS=$((PASS+1))
    else
        echo "  FAIL: $label (tried: $*)"
        FAIL=$((FAIL+1))
    fi
}

step "Recall assertions: model B output reflects session 1's plan"
check_contains "title (borrow checker)"          "$S2_RESPONSE" "borrow checker" "borrowck" "borrow-checker"
check_contains "example 1 (closure / move)"      "$S2_RESPONSE" "closure" "move"
check_contains "example 2 (mut aliasing)"        "$S2_RESPONSE" "mutable" "&mut" "aliasing"
check_contains "example 3 (lifetime elision)"    "$S2_RESPONSE" "lifetime" "elision"
check_contains "length target"                   "$S2_RESPONSE" "1500" "1,500" "fifteen hundred"
check_contains "tone descriptor"                 "$S2_RESPONSE" "friendly" "conversational" "casual"

step "Sanitisation assertions: canary NEVER in persisted state"
LEAKED=0
for path in "$AI_MEMORY_DATA_DIR/wiki" "$AI_MEMORY_DATA_DIR/db" "$TEST_DIR/handoff.md"; do
    if [[ -e "$path" ]] && grep -rqF "LEAK_ME_PLEASE" "$path" 2>/dev/null; then
        echo "  FAIL: canary 'LEAK_ME_PLEASE' found under $path:"
        grep -rnF "LEAK_ME_PLEASE" "$path" 2>/dev/null | head -5
        LEAKED=1
    fi
done
if [[ $LEAKED -eq 0 ]]; then
    echo "  PASS: canary scrubbed from wiki, db, and handoff body"
    PASS=$((PASS+1))
else
    FAIL=$((FAIL+1))
fi

# Also assert the canary text is in the model-B response only if the
# handoff didn't accidentally re-introduce it (it shouldn't, since
# build_auto_handoff extracts from already-scrubbed observations).
if echo "$S2_RESPONSE" | grep -qF "LEAK_ME_PLEASE"; then
    echo "  FAIL: canary surfaced into model B's context via handoff"
    FAIL=$((FAIL+1))
else
    echo "  PASS: canary not in model B's input context"
    PASS=$((PASS+1))
fi

step "Handoff content assertion"
if echo "$HANDOFF_MD" | grep -iqE "borrow|blog|rust|borrowck"; then
    echo "  PASS: handoff body references the session-1 topic"
    PASS=$((PASS+1))
else
    echo "  FAIL: handoff body looks unrelated to the session-1 topic"
    FAIL=$((FAIL+1))
fi

# --------------------------------------------------------------------
# Summary
# --------------------------------------------------------------------

echo
echo "================================="
echo "  PASS: $PASS"
echo "  FAIL: $FAIL"
echo "================================="
[[ $FAIL -gt 0 ]] && exit 1
exit 0
