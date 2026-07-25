#!/usr/bin/env bash
# Manual, opt-in acceptance test for managed cross-harness workstreams.
# This is intentionally not called by CI: the real-harness phase uses the
# operator's installed CLIs, credentials, model defaults, and native stores.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BIN=${AI_MEMORY_ACCEPTANCE_BIN:-"$ROOT/target/debug/ai-memory"}
KEEP=${AI_MEMORY_ACCEPTANCE_KEEP:-0}
DETERMINISTIC_ONLY=${AI_MEMORY_ACCEPTANCE_DETERMINISTIC_ONLY:-0}
HARNESS_WORDS=${AI_MEMORY_ACCEPTANCE_HARNESSES:-"claude codex opencode pi crush omp kimi"}
TMP=$(mktemp -d "${TMPDIR:-/tmp}/ai-memory-workstream-acceptance.XXXXXX")
DATA="$TMP/data"
REPO="$TMP/repo"
CONFIG="$TMP/config"
LOGS="$TMP/logs"
SERVER_PID=""

cleanup() {
  local code=$?
  if [ -n "$SERVER_PID" ]; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  if [ "$KEEP" = 1 ] || [ "$code" -ne 0 ]; then
    printf 'acceptance artifacts retained at %s\n' "$TMP" >&2
  else
    rm -rf "$TMP"
  fi
}
trap cleanup EXIT INT TERM

for command in cargo curl diff git jq script sqlite3; do
  command -v "$command" >/dev/null 2>&1 || {
    printf 'missing required command: %s\n' "$command" >&2
    exit 1
  }
done

if [ ! -x "$BIN" ] || [ "${AI_MEMORY_ACCEPTANCE_REBUILD:-1}" = 1 ]; then
  (cd "$ROOT" && TAILWIND_SKIP=1 cargo build -p ai-memory-cli)
fi

mkdir -p "$DATA" "$REPO" "$CONFIG" "$LOGS"
git -C "$REPO" init -q
git -C "$REPO" config user.name "ai-memory acceptance"
git -C "$REPO" config user.email "acceptance@localhost"
printf '# Managed workstream acceptance\n' >"$REPO/README.md"
git -C "$REPO" add README.md
git -C "$REPO" commit -qm "acceptance fixture"

TOKEN="managed-acceptance-$(date +%s)-$$"
PORT=${AI_MEMORY_ACCEPTANCE_PORT:-$((52000 + ($$ % 10000)))}
for _ in $(seq 1 50); do
  if ! curl -sS --max-time 0.1 "http://127.0.0.1:$PORT/" >/dev/null 2>&1; then
    break
  fi
  PORT=$((PORT + 1))
done
URL="http://127.0.0.1:$PORT"
export AI_MEMORY_SERVER_URL="$URL"
export AI_MEMORY_AUTH_TOKEN="$TOKEN"
export AI_MEMORY_NO_VERSION_CHECK=1

"$BIN" --data-dir "$DATA" serve \
  --transport http \
  --bind "127.0.0.1:$PORT" \
  --no-watcher >"$LOGS/server.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 100); do
  status=$(curl -sS --max-time 0.2 -o /dev/null -w '%{http_code}' \
    -H "Authorization: Bearer $TOKEN" \
    "$URL/workstream/not-a-uuid/events" 2>/dev/null || true)
  [ "$status" = 400 ] && break
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    printf 'ai-memory server exited during startup\n' >&2
    tail -80 "$LOGS/server.log" >&2
    exit 1
  fi
  sleep 0.1
done
[ "${status:-}" = 400 ] || {
  printf 'ai-memory server did not become ready at %s\n' "$URL" >&2
  tail -80 "$LOGS/server.log" >&2
  exit 1
}

FAKE="$TMP/fake-harness.sh"
cat >"$FAKE" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
case "${AI_MEMORY_ACCEPTANCE_FAKE_MODE:-argv}" in
  argv)
    printf '%s\n' "$@" >"$AI_MEMORY_ACCEPTANCE_ARGV_LOG"
    ;;
  exit)
    exit "${AI_MEMORY_ACCEPTANCE_EXIT_CODE:-23}"
    ;;
  lease)
    : >"$AI_MEMORY_ACCEPTANCE_STARTED"
    sleep "${AI_MEMORY_ACCEPTANCE_SLEEP:-3}"
    ;;
  crush)
    printf '%s\n' "$@" >"$AI_MEMORY_ACCEPTANCE_ARGV_LOG"
    printf '%s\n' "$CRUSH_GLOBAL_CONFIG" >"$AI_MEMORY_ACCEPTANCE_CRUSH_ENV_LOG"
    cp "$CRUSH_GLOBAL_CONFIG/crush.json" "$AI_MEMORY_ACCEPTANCE_CRUSH_CONFIG_LOG"
    packet=$(jq -r '.options.global_context_paths[-1]' "$CRUSH_GLOBAL_CONFIG/crush.json")
    cp "$packet" "$AI_MEMORY_ACCEPTANCE_CRUSH_PACKET_LOG"
    ;;
  kimi)
    printf '%s\n' "$@" >"$AI_MEMORY_ACCEPTANCE_ARGV_LOG"
    # Honor `--session <id>` (resume); a fresh launch mints its own id
    # because Kimi Code cannot accept a caller-supplied session id.
    session_id=""
    previous_arg=""
    for arg in "$@"; do
      if [ "$previous_arg" = --session ]; then
        session_id=$arg
      fi
      previous_arg=$arg
    done
    if [ -z "$session_id" ]; then
      session_id="session_acceptance-$(date +%s)-$$"
    fi
    # The bucket directory name is intentionally opaque: the real layout
    # hashes the working directory one-way, and discovery must read
    # state.json's workDir instead of parsing the bucket name.
    session_dir="${KIMI_CODE_HOME:?kimi fake mode requires KIMI_CODE_HOME}/sessions/wd_fixture_bucket/$session_id"
    mkdir -p "$session_dir/agents/main"
    wire="$session_dir/agents/main/wire.jsonl"
    if [ ! -f "$session_dir/state.json" ]; then
      printf '{"workDir":"%s"}\n' "$PWD" >"$session_dir/state.json"
      printf '{"type":"metadata","protocol_version":"1","created_at":%s}\n' \
        "$(date +%s)000" >"$wire"
    fi
    sentinel=${AI_MEMORY_ACCEPTANCE_SENTINEL:-AMWS-FAKE-KIMI}
    printf '{"type":"context.append_message","time":%s,"message":{"role":"user","content":[{"type":"text","text":"%s"}],"toolCalls":[]}}\n' \
      "$(date +%s)000" "$sentinel" >>"$wire"
    printf '{"type":"context.append_message","time":%s,"message":{"role":"assistant","content":[{"type":"text","text":"%s reply"}],"toolCalls":[]}}\n' \
      "$(date +%s)000" "$sentinel" >>"$wire"
    ;;
esac
EOF
chmod +x "$FAKE"

printf 'running deterministic wrapper edge checks\n'

# Utility invocations must not discover and import another process's recent
# session merely because it is active in the same checkout.
UTILITY_CODEX_HOME="$CONFIG/utility-codex"
mkdir -p "$UTILITY_CODEX_HOME/sessions/2026/01/01"
printf '%s\n%s\n' \
  "{\"type\":\"session_meta\",\"payload\":{\"id\":\"utility-unrelated\",\"cwd\":\"$REPO\"}}" \
  '{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"must not import"}]}}' \
  >"$UTILITY_CODEX_HOME/sessions/2026/01/01/rollout-utility.jsonl"
(
  cd "$REPO"
  CODEX_HOME="$UTILITY_CODEX_HOME" \
  AI_MEMORY_ACCEPTANCE_FAKE_MODE=argv \
  AI_MEMORY_ACCEPTANCE_ARGV_LOG="$TMP/utility-argv.log" \
    "$BIN" --data-dir "$DATA" run --new edge-utility --executable "$FAKE" \
      codex --version >"$LOGS/edge-utility.log" 2>&1
)
diff -u <(printf '%s\n' --version) "$TMP/utility-argv.log"
# The repository checkpoint is still recorded; the unrelated user message is
# not. A buggy post-exit discovery path reports two imported events here.
grep -q "workstream 'edge-utility' saved 1 new event(s)" "$LOGS/edge-utility.log"

(
  cd "$REPO"
  AI_MEMORY_ACCEPTANCE_FAKE_MODE=argv \
  AI_MEMORY_ACCEPTANCE_ARGV_LOG="$TMP/argv.log" \
    "$BIN" --data-dir "$DATA" run --new edge-argv --executable "$FAKE" \
      codex --yolo -m gpt-5 "prompt words" >"$LOGS/edge-argv.log" 2>&1
)
diff -u \
  <(printf '%s\n' -m gpt-5 "prompt words" --dangerously-bypass-approvals-and-sandbox) \
  "$TMP/argv.log"

set +e
(
  cd "$REPO"
  AI_MEMORY_ACCEPTANCE_FAKE_MODE=exit AI_MEMORY_ACCEPTANCE_EXIT_CODE=23 \
    "$BIN" --data-dir "$DATA" run --new edge-exit --executable "$FAKE" \
      codex >"$LOGS/edge-exit.log" 2>&1
)
exit_code=$?
set -e
[ "$exit_code" -eq 23 ] || {
  printf 'managed child exit code was %s, expected 23\n' "$exit_code" >&2
  exit 1
}

(
  cd "$REPO"
  AI_MEMORY_ACCEPTANCE_FAKE_MODE=lease \
  AI_MEMORY_ACCEPTANCE_SLEEP=7 \
  AI_MEMORY_ACCEPTANCE_STARTED="$TMP/lease-started" \
    "$BIN" --data-dir "$DATA" run --new edge-lease --executable "$FAKE" \
      codex >"$LOGS/edge-lease-owner.log" 2>&1
) &
lease_pid=$!
for _ in $(seq 1 100); do
  [ -f "$TMP/lease-started" ] && break
  sleep 0.05
done
[ -f "$TMP/lease-started" ] || {
  printf 'lease owner did not start\n' >&2
  exit 1
}
set +e
(
  cd "$REPO"
  AI_MEMORY_ACCEPTANCE_FAKE_MODE=argv \
  AI_MEMORY_ACCEPTANCE_ARGV_LOG="$TMP/lease-contender-argv.log" \
    "$BIN" --data-dir "$DATA" run --workstream edge-lease --executable "$FAKE" \
      codex >"$LOGS/edge-lease-contender.log" 2>&1
)
lease_code=$?
set -e
[ "$lease_code" -ne 0 ] || {
  printf 'a concurrent managed writer unexpectedly acquired the lease\n' >&2
  exit 1
}
wait "$lease_pid"

# Bare run must fail before contacting the server when this checkout has no
# native session in any auto-detected harness.
mkdir -p "$CONFIG/empty-home"
set +e
(
  cd "$REPO"
  HOME="$CONFIG/empty-home" XDG_DATA_HOME="$CONFIG/empty-home/xdg-data" \
    "$BIN" --data-dir "$DATA" run >"$LOGS/edge-auto-empty.log" 2>&1
)
auto_empty_code=$?
set -e
[ "$auto_empty_code" -ne 0 ] || {
  printf 'bare run unexpectedly started without a checkout-local session\n' >&2
  exit 1
}
# The harness list in this message grows as adapters join the automatic
# pool (Kimi joined after Crush), so match the stable tail instead.
grep -q 'session was found for this directory' \
  "$LOGS/edge-auto-empty.log"

# On a new workstream, bare run automatically adopts the newest local session.
AUTO_HOME="$CONFIG/auto-home"
AUTO_CODEX_HOME="$AUTO_HOME/.codex"
AUTO_CLAUDE_HOME="$AUTO_HOME/.claude"
AUTO_BIN="$AUTO_HOME/bin"
mkdir -p "$AUTO_CODEX_HOME/sessions/2026/01/01" \
  "$AUTO_CLAUDE_HOME/projects/fixture" "$AUTO_BIN"
ln -s "$FAKE" "$AUTO_BIN/codex"
ln -s "$FAKE" "$AUTO_BIN/claude"
printf '{"sessionId":"auto-claude-old","cwd":"%s"}\n' "$REPO" \
  >"$AUTO_CLAUDE_HOME/projects/fixture/auto-claude-old.jsonl"
sleep 1
printf '{"type":"session_meta","payload":{"id":"auto-codex-new","cwd":"%s"}}\n' \
  "$REPO" >"$AUTO_CODEX_HOME/sessions/2026/01/01/rollout-auto.jsonl"
(
  cd "$REPO"
  HOME="$AUTO_HOME" CODEX_HOME="$AUTO_CODEX_HOME" \
  CLAUDE_CONFIG_DIR="$AUTO_CLAUDE_HOME" PATH="$AUTO_BIN:$PATH" \
  AI_MEMORY_ACCEPTANCE_FAKE_MODE=argv \
  AI_MEMORY_ACCEPTANCE_ARGV_LOG="$TMP/auto-newest-argv.log" \
    "$BIN" --data-dir "$DATA" run --workspace edge-auto --project edge-auto --yolo \
      >"$LOGS/edge-auto-newest.log" 2>&1
)
diff -u \
  <(printf '%s\n' resume auto-codex-new --dangerously-bypass-approvals-and-sandbox) \
  "$TMP/auto-newest-argv.log"

# Establish Claude after Codex, then verify bare run follows the managed
# workstream's current Claude link instead of the newer but obsolete Codex file.
(
  cd "$REPO"
  HOME="$AUTO_HOME" CLAUDE_CONFIG_DIR="$AUTO_CLAUDE_HOME" \
  AI_MEMORY_ACCEPTANCE_FAKE_MODE=argv \
  AI_MEMORY_ACCEPTANCE_ARGV_LOG="$TMP/auto-claude-first-argv.log" \
    "$BIN" --data-dir "$DATA" run --workspace edge-auto --project edge-auto \
      --executable "$FAKE" claude >"$LOGS/edge-auto-claude-first.log" 2>&1
)
mapfile -t auto_claude_first <"$TMP/auto-claude-first-argv.log"
[ "${auto_claude_first[0]:-}" = --session-id ] || {
  printf 'Claude did not create a fresh managed session\n' >&2
  exit 1
}
auto_claude_id=${auto_claude_first[1]}
(
  cd "$REPO"
  HOME="$AUTO_HOME" CODEX_HOME="$AUTO_CODEX_HOME" \
  CLAUDE_CONFIG_DIR="$AUTO_CLAUDE_HOME" PATH="$AUTO_BIN:$PATH" \
  AI_MEMORY_ACCEPTANCE_FAKE_MODE=argv \
  AI_MEMORY_ACCEPTANCE_ARGV_LOG="$TMP/auto-managed-precedence-argv.log" \
    "$BIN" --data-dir "$DATA" run --workspace edge-auto --project edge-auto \
      >"$LOGS/edge-auto-managed-precedence.log" 2>&1
)
diff -u <(printf '%s\n' --resume "$auto_claude_id") \
  "$TMP/auto-managed-precedence-argv.log"

# A handled failure after lease acquisition must release immediately. A
# malformed Crush config fails after context fetch; the next run below would
# hit a stale 409 if cancellation were missing.
BAD_CRUSH_CONFIG="$CONFIG/bad-crush"
mkdir -p "$BAD_CRUSH_CONFIG"
printf '{not-json\n' >"$BAD_CRUSH_CONFIG/crush.json"
set +e
(
  cd "$REPO"
  HOME="$AUTO_HOME" CRUSH_GLOBAL_CONFIG="$BAD_CRUSH_CONFIG" \
    AI_MEMORY_ACCEPTANCE_FAKE_MODE=crush \
    "$BIN" --data-dir "$DATA" run --workspace edge-auto --project edge-auto \
      --executable "$FAKE" crush >"$LOGS/edge-crush-invalid-config.log" 2>&1
)
bad_crush_code=$?
set -e
[ "$bad_crush_code" -ne 0 ] || {
  printf 'malformed Crush config unexpectedly succeeded\n' >&2
  exit 1
}
grep -q 'parsing Crush config' "$LOGS/edge-crush-invalid-config.log"

# Crush has no SessionStart hook. Verify the launcher fetches the packet into a
# temporary supported global-context config, then removes it after exit.
(
  cd "$REPO"
  HOME="$AUTO_HOME" \
  AI_MEMORY_ACCEPTANCE_FAKE_MODE=crush \
  AI_MEMORY_ACCEPTANCE_ARGV_LOG="$TMP/crush-context-argv.log" \
  AI_MEMORY_ACCEPTANCE_CRUSH_ENV_LOG="$TMP/crush-context-env.log" \
  AI_MEMORY_ACCEPTANCE_CRUSH_CONFIG_LOG="$TMP/crush-context-config.json" \
  AI_MEMORY_ACCEPTANCE_CRUSH_PACKET_LOG="$TMP/crush-context-packet.md" \
    "$BIN" --data-dir "$DATA" run --workspace edge-auto --project edge-auto \
      --executable "$FAKE" --yolo crush >"$LOGS/edge-crush-context.log" 2>&1
)
diff -u <(printf '%s\n' --yolo) "$TMP/crush-context-argv.log"
grep -q 'ai-memory managed workstream' "$TMP/crush-context-packet.md"
crush_context_dir=$(cat "$TMP/crush-context-env.log")
[ ! -e "$crush_context_dir" ] || {
  printf 'temporary Crush context directory was not removed\n' >&2
  exit 1
}

# Kimi fake-mode fixture: the fake kimi honors `--session <id>`, writes the
# native store layout ($KIMI_CODE_HOME/sessions/<bucket>/<id>/state.json plus
# agents/main/wire.jsonl), and appends the round sentinel as
# context.append_message records. A fresh launch injects no selector because
# Kimi Code cannot mint a caller-supplied session id; the wrapper links the
# session post-exit by exact-checkout discovery through state.json.
KIMI_FAKE_HOME="$CONFIG/kimi-fake"
mkdir -p "$KIMI_FAKE_HOME"
(
  cd "$REPO"
  KIMI_CODE_HOME="$KIMI_FAKE_HOME" \
  AI_MEMORY_ACCEPTANCE_FAKE_MODE=kimi \
  AI_MEMORY_ACCEPTANCE_ARGV_LOG="$TMP/kimi-first-argv.log" \
  AI_MEMORY_ACCEPTANCE_SENTINEL="AMWS-FAKE-KIMI-ONE" \
    "$BIN" --data-dir "$DATA" run --new edge-kimi --executable "$FAKE" \
      kimi >"$LOGS/edge-kimi-first.log" 2>&1
)
# The fake echoes one line per argv element, so a zero-argument fresh launch
# leaves a single empty line in the log; assert no real argument arrived
# rather than asserting the file is byte-empty.
if grep -q . "$TMP/kimi-first-argv.log"; then
  printf 'fresh kimi launch unexpectedly received a session selector\n' >&2
  cat "$TMP/kimi-first-argv.log" >&2
  exit 1
fi
kimi_session_dir=$(find "$KIMI_FAKE_HOME/sessions" -mindepth 2 -maxdepth 2 -type d -print -quit)
[ -n "$kimi_session_dir" ] || {
  printf 'fake kimi did not create a native session store\n' >&2
  exit 1
}
kimi_session_id=$(basename "$kimi_session_dir")
kimi_ws_hex=$(sqlite3 "$DATA/db/memory.sqlite" \
  "SELECT lower(hex(id)) FROM workstreams WHERE name = 'edge-kimi' ORDER BY selected_at DESC LIMIT 1;")
[ "${#kimi_ws_hex}" -eq 32 ] || {
  printf 'could not resolve the edge-kimi workstream id\n' >&2
  exit 1
}
kimi_ws_id="${kimi_ws_hex:0:8}-${kimi_ws_hex:8:4}-${kimi_ws_hex:12:4}-${kimi_ws_hex:16:4}-${kimi_ws_hex:20:12}"
kimi_first_hits=$("$BIN" --data-dir "$DATA" workstream-search \
  --workstream-id "$kimi_ws_id" --limit 100 --json "AMWS-FAKE-KIMI-ONE")
jq -e --arg id "$kimi_session_id" \
  '[.[] | select(.agent == "kimi-code" and .role == "assistant" and (.content | contains("AMWS-FAKE-KIMI-ONE")) and .native_session_id == $id)] | length == 1' \
  <<<"$kimi_first_hits" >/dev/null || {
  printf 'kimi wire.jsonl sentinel was not imported from the discovered session\n' >&2
  tail -80 "$LOGS/edge-kimi-first.log" >&2
  exit 1
}
(
  cd "$REPO"
  KIMI_CODE_HOME="$KIMI_FAKE_HOME" \
  AI_MEMORY_ACCEPTANCE_FAKE_MODE=kimi \
  AI_MEMORY_ACCEPTANCE_ARGV_LOG="$TMP/kimi-second-argv.log" \
  AI_MEMORY_ACCEPTANCE_SENTINEL="AMWS-FAKE-KIMI-TWO" \
    "$BIN" --data-dir "$DATA" run --workstream edge-kimi --executable "$FAKE" \
      kimi-cli >"$LOGS/edge-kimi-second.log" 2>&1
)
diff -u <(printf '%s\n' --session "$kimi_session_id") "$TMP/kimi-second-argv.log"
kimi_second_hits=$("$BIN" --data-dir "$DATA" workstream-search \
  --workstream-id "$kimi_ws_id" --limit 100 --json "AMWS-FAKE-KIMI")
jq -e \
  '([.[] | select(.role == "assistant" and (.content | contains("AMWS-FAKE-KIMI-ONE")))] | length == 1)
   and ([.[] | select(.role == "assistant" and (.content | contains("AMWS-FAKE-KIMI-TWO")))] | length == 1)' \
  <<<"$kimi_second_hits" >/dev/null || {
  printf 'kimi incremental import duplicated or missed a round sentinel\n' >&2
  tail -80 "$LOGS/edge-kimi-second.log" >&2
  exit 1
}

# A blank first launch remains eligible for one-time native-session adoption.
# Use a pseudo-terminal because redirected/scripted launches deliberately skip
# the chooser.
ADOPTION_CODEX_HOME="$CONFIG/adoption-codex"
mkdir -p "$ADOPTION_CODEX_HOME/sessions/2026/01/01"
(
  cd "$REPO"
  CODEX_HOME="$ADOPTION_CODEX_HOME" \
  AI_MEMORY_ACCEPTANCE_FAKE_MODE=argv \
  AI_MEMORY_ACCEPTANCE_ARGV_LOG="$TMP/adoption-blank-argv.log" \
    "$BIN" --data-dir "$DATA" run --new edge-adopt --executable "$FAKE" \
      codex >"$LOGS/edge-adoption-blank.log" 2>&1
)
printf '{"type":"session_meta","payload":{"id":"adoption-codex-id","cwd":"%s"}}\n' \
  "$REPO" >"$ADOPTION_CODEX_HOME/sessions/2026/01/01/rollout-adoption.jsonl"

ADOPTION_RUNNER="$TMP/adoption-runner.sh"
cat >"$ADOPTION_RUNNER" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
cd "$AI_MEMORY_ACCEPTANCE_REPO"
exec "$AI_MEMORY_ACCEPTANCE_BIN" --data-dir "$AI_MEMORY_ACCEPTANCE_DATA" \
  run --workstream edge-adopt --executable "$AI_MEMORY_ACCEPTANCE_FAKE" "$@"
EOF
chmod +x "$ADOPTION_RUNNER"

printf '\n' | env \
  AI_MEMORY_ACCEPTANCE_REPO="$REPO" \
  AI_MEMORY_ACCEPTANCE_BIN="$BIN" \
  AI_MEMORY_ACCEPTANCE_DATA="$DATA" \
  AI_MEMORY_ACCEPTANCE_FAKE="$FAKE" \
  AI_MEMORY_ACCEPTANCE_FAKE_MODE=argv \
  AI_MEMORY_ACCEPTANCE_ARGV_LOG="$TMP/adoption-codex-argv.log" \
  CODEX_HOME="$ADOPTION_CODEX_HOME" \
  script -qefc "$ADOPTION_RUNNER codex" /dev/null \
    >"$LOGS/edge-adoption-codex.log" 2>&1
diff -u <(printf '%s\n' resume adoption-codex-id) "$TMP/adoption-codex-argv.log"

# Once Codex establishes the workstream, Claude must start clean even when an
# obsolete checkout-local Claude session exists.
ADOPTION_CLAUDE_HOME="$CONFIG/adoption-claude"
mkdir -p "$ADOPTION_CLAUDE_HOME/projects/fixture"
printf '{"sessionId":"obsolete-claude-id","cwd":"%s"}\n' "$REPO" \
  >"$ADOPTION_CLAUDE_HOME/projects/fixture/obsolete-claude-id.jsonl"
printf '\n' | env \
  AI_MEMORY_ACCEPTANCE_REPO="$REPO" \
  AI_MEMORY_ACCEPTANCE_BIN="$BIN" \
  AI_MEMORY_ACCEPTANCE_DATA="$DATA" \
  AI_MEMORY_ACCEPTANCE_FAKE="$FAKE" \
  AI_MEMORY_ACCEPTANCE_FAKE_MODE=argv \
  AI_MEMORY_ACCEPTANCE_ARGV_LOG="$TMP/adoption-claude-argv.log" \
  CLAUDE_CONFIG_DIR="$ADOPTION_CLAUDE_HOME" \
  script -qefc "$ADOPTION_RUNNER claude" /dev/null \
    >"$LOGS/edge-adoption-claude.log" 2>&1
mapfile -t adoption_claude_argv <"$TMP/adoption-claude-argv.log"
[ "${adoption_claude_argv[0]:-}" = --session-id ] || {
  printf 'established workstream did not create a fresh Claude session\n' >&2
  exit 1
}
[ "${adoption_claude_argv[1]:-}" != obsolete-claude-id ] || {
  printf 'established workstream adopted an obsolete Claude session\n' >&2
  exit 1
}

if [ "$DETERMINISTIC_ONLY" = 1 ]; then
  printf 'deterministic managed-workstream acceptance passed\n'
  exit 0
fi

read -r -a requested_harnesses <<<"$HARNESS_WORDS"
harnesses=()
for requested_harness in "${requested_harnesses[@]}"; do
  case "$requested_harness" in
    kimi-code | kimi-cli) harness=kimi ;;
    *) harness=$requested_harness ;;
  esac
  if command -v "$harness" >/dev/null 2>&1; then
    harnesses+=("$harness")
  else
    printf 'skipping unavailable harness: %s\n' "$requested_harness" >&2
  fi
done
[ "${#harnesses[@]}" -ge 2 ] || {
  printf 'real acceptance needs at least two installed harnesses\n' >&2
  exit 1
}

CLAUDE_CONFIG_HOME="$CONFIG/claude"
CLAUDE_SETTINGS="$CLAUDE_CONFIG_HOME/settings.json"
CODEX_ACCEPTANCE_HOME="$CONFIG/codex-home"
CODEX_HOOKS="$CODEX_ACCEPTANCE_HOME/.codex/hooks.json"
OPENCODE_CONFIG_HOME="$CONFIG/opencode-xdg"
OPENCODE_PLUGIN="$OPENCODE_CONFIG_HOME/opencode/plugins/ai-memory.ts"
OPENCODE_DATA_HOME="$CONFIG/opencode-xdg-data"
PI_EXTENSION="$CONFIG/pi/ai-memory.ts"
OMP_EXTENSION="$CONFIG/omp/ai-memory.ts"
OMP_AGENT_DIR="$CONFIG/omp/agent"
CRUSH_DATA_DIR="$CONFIG/crush/data"
KIMI_ACCEPTANCE_HOME="$CONFIG/kimi-home"
mkdir -p "$(dirname "$CLAUDE_SETTINGS")" "$(dirname "$CODEX_HOOKS")" \
  "$(dirname "$OPENCODE_PLUGIN")" "$(dirname "$PI_EXTENSION")" \
  "$(dirname "$OMP_EXTENSION")" "$OMP_AGENT_DIR" "$OPENCODE_DATA_HOME/opencode" \
  "$CRUSH_DATA_DIR" "$KIMI_ACCEPTANCE_HOME"

# Redirect native transcript stores into the fixture while reusing only the
# minimum authentication material required for real model calls.
if [ -f "$HOME/.claude/.credentials.json" ]; then
  cp "$HOME/.claude/.credentials.json" "$CLAUDE_CONFIG_HOME/.credentials.json"
fi
if [ -f "$HOME/.local/share/opencode/auth.json" ]; then
  cp "$HOME/.local/share/opencode/auth.json" "$OPENCODE_DATA_HOME/opencode/auth.json"
fi

# Codex only discovers hooks below its home. Use a temporary home so the
# acceptance config cannot modify or depend on the operator's trusted hooks.
if [ -f "$HOME/.codex/auth.json" ]; then
  cp "$HOME/.codex/auth.json" "$CODEX_ACCEPTANCE_HOME/.codex/auth.json"
fi

# OMP's installed release drops explicit extension paths when
# --no-extensions is set. Isolate discovery with a temporary agent directory
# and copy only settings plus consistent credential/model database backups.
for database in agent.db models.db; do
  if [ -f "$HOME/.omp/agent/$database" ]; then
    sqlite3 "$HOME/.omp/agent/$database" ".backup '$OMP_AGENT_DIR/$database'"
  fi
done
for config_name in auth.json config.yml models-store.json settings.json; do
  if [ -f "$HOME/.omp/agent/$config_name" ]; then
    cp "$HOME/.omp/agent/$config_name" "$OMP_AGENT_DIR/$config_name"
  fi
done

# Preserve OpenCode's provider/model preferences while loading only the
# acceptance plugin from the isolated XDG config root.
for config_name in opencode.json opencode.jsonc tui.json; do
  if [ -f "$HOME/.config/opencode/$config_name" ]; then
    cp "$HOME/.config/opencode/$config_name" \
      "$OPENCODE_CONFIG_HOME/opencode/$config_name"
  fi
done

# Kimi Code keeps providers/model and hooks in one config.toml under
# $KIMI_CODE_HOME. Seed the isolated home with the operator's provider
# settings and minimum login state; install-hooks merges its [[hooks]]
# entries without rewriting the rest of the file. Native sessions, indexes,
# logs, and telemetry remain isolated.
if [ -f "$HOME/.kimi-code/config.toml" ]; then
  cp "$HOME/.kimi-code/config.toml" "$KIMI_ACCEPTANCE_HOME/config.toml"
fi
for relative in credentials/kimi-code.json oauth/kimi-code device_id; do
  if [ -f "$HOME/.kimi-code/$relative" ]; then
    mkdir -p "$KIMI_ACCEPTANCE_HOME/$(dirname "$relative")"
    cp "$HOME/.kimi-code/$relative" "$KIMI_ACCEPTANCE_HOME/$relative"
  fi
done

install_hook() {
  local agent=$1
  local target=$2
  local -a command=(
    "$BIN" --data-dir "$DATA" install-hooks --apply
    --agent "$agent" --server-url "$URL" --auth-token "$TOKEN"
    --config-file "$target"
  )
  case "$agent" in
    claude-code | codex | kimi-code)
      command+=(--hooks-dir "$ROOT/hooks")
      ;;
  esac
  XDG_DATA_HOME="$TMP/xdg-data" "${command[@]}" \
    >"$LOGS/install-$agent.log" 2>&1
}

install_hook claude-code "$CLAUDE_SETTINGS"
install_hook codex "$CODEX_HOOKS"
install_hook opencode "$OPENCODE_PLUGIN"
install_hook pi "$PI_EXTENSION"
install_hook omp "$OMP_EXTENSION"
install_hook kimi-code "$KIMI_ACCEPTANCE_HOME/config.toml"

uuid_from_hex() {
  local hex=$1
  printf '%s-%s-%s-%s-%s\n' \
    "${hex:0:8}" "${hex:8:4}" "${hex:12:4}" "${hex:16:4}" "${hex:20:12}"
}

workstream_id() {
  local name=$1
  local hex
  hex=$(sqlite3 "$DATA/db/memory.sqlite" \
    "SELECT lower(hex(id)) FROM workstreams WHERE name = '$name' ORDER BY selected_at DESC LIMIT 1;")
  [ "${#hex}" -eq 32 ] || return 1
  uuid_from_hex "$hex"
}

agent_wire_name() {
  case "$1" in
    claude) printf 'claude-code\n' ;;
    opencode) printf 'open-code\n' ;;
    kimi) printf 'kimi-code\n' ;;
    *) printf '%s\n' "$1" ;;
  esac
}

uppercase() {
  printf '%s' "$1" | tr '[:lower:]' '[:upper:]'
}

run_harness() {
  local harness=$1
  local current=$2
  local previous=$3
  local first_run=$4
  local log="$LOGS/real-$harness-$current.log"
  local prompt
  local expected_agent
  local -a wrapper_args native_args
  expected_agent=$(agent_wire_name "$harness")
  if [ -z "$previous" ]; then
    prompt="Do not use tools. Reply with exactly: $current"
  else
    prompt="Do not use tools. In the newest assistant event from the injected ai-memory managed-workstream context, find the final whitespace-delimited token beginning with AMWS-. Reply with exactly that token, one space, then $current."
  fi
  if [ "$first_run" = 1 ]; then
    wrapper_args=(--new "$WORKSTREAM_NAME")
  else
    wrapper_args=(--workstream "$WORKSTREAM_NAME")
  fi
  case "$harness" in
    claude)
      native_args=(-p --settings "$CLAUDE_SETTINGS" --model "${AI_MEMORY_ACCEPTANCE_CLAUDE_MODEL:-haiku}" --permission-mode plan "$prompt")
      ;;
    codex)
      native_args=(exec -c 'sandbox_mode="read-only"' --dangerously-bypass-hook-trust --json "$prompt")
      if [ -n "${AI_MEMORY_ACCEPTANCE_CODEX_MODEL:-}" ]; then
        native_args=(exec -c 'sandbox_mode="read-only"' --dangerously-bypass-hook-trust --json --model "$AI_MEMORY_ACCEPTANCE_CODEX_MODEL" "$prompt")
      fi
      ;;
    opencode)
      native_args=(run --format json --auto "$prompt")
      [ -z "${AI_MEMORY_ACCEPTANCE_OPENCODE_MODEL:-}" ] || native_args=(run --format json --auto --model "$AI_MEMORY_ACCEPTANCE_OPENCODE_MODEL" "$prompt")
      ;;
    pi)
      native_args=(-p --no-tools --no-extensions --extension "$PI_EXTENSION" --session-dir "$CONFIG/pi/sessions" "$prompt")
      [ -z "${AI_MEMORY_ACCEPTANCE_PI_MODEL:-}" ] || native_args=(-p --no-tools --no-extensions --extension "$PI_EXTENSION" --session-dir "$CONFIG/pi/sessions" --model "$AI_MEMORY_ACCEPTANCE_PI_MODEL" "$prompt")
      ;;
    crush)
      native_args=(run --quiet --data-dir "$CRUSH_DATA_DIR" "$prompt")
      [ -z "${AI_MEMORY_ACCEPTANCE_CRUSH_MODEL:-}" ] || native_args=(run --quiet --data-dir "$CRUSH_DATA_DIR" --model "$AI_MEMORY_ACCEPTANCE_CRUSH_MODEL" "$prompt")
      ;;
    omp)
      native_args=(-p --no-tools --extension "$OMP_EXTENSION" --session-dir "$CONFIG/omp/sessions" "$prompt")
      [ -z "${AI_MEMORY_ACCEPTANCE_OMP_MODEL:-}" ] || native_args=(-p --no-tools --extension "$OMP_EXTENSION" --session-dir "$CONFIG/omp/sessions" --model "$AI_MEMORY_ACCEPTANCE_OMP_MODEL" "$prompt")
      ;;
    kimi)
      native_args=(-p "$prompt")
      [ -z "${AI_MEMORY_ACCEPTANCE_KIMI_MODEL:-}" ] || native_args=(-p -m "$AI_MEMORY_ACCEPTANCE_KIMI_MODEL" "$prompt")
      ;;
    *)
      printf 'unsupported acceptance harness: %s\n' "$harness" >&2
      return 1
      ;;
  esac

  printf 'running real harness: %s\n' "$harness" >&2
  if [ "$harness" = claude ]; then
    (cd "$REPO" && CLAUDE_CONFIG_DIR="$CLAUDE_CONFIG_HOME" \
      "$BIN" --data-dir "$DATA" run "${wrapper_args[@]}" "$harness" "${native_args[@]}") \
      >"$log" 2>&1
  elif [ "$harness" = codex ]; then
    (cd "$REPO" && HOME="$CODEX_ACCEPTANCE_HOME" \
      CODEX_HOME="$CODEX_ACCEPTANCE_HOME/.codex" \
      "$BIN" --data-dir "$DATA" run "${wrapper_args[@]}" "$harness" "${native_args[@]}") \
      >"$log" 2>&1
  elif [ "$harness" = opencode ]; then
    (cd "$REPO" && XDG_CONFIG_HOME="$OPENCODE_CONFIG_HOME" \
      XDG_DATA_HOME="$OPENCODE_DATA_HOME" \
      "$BIN" --data-dir "$DATA" run "${wrapper_args[@]}" "$harness" "${native_args[@]}") \
      >"$log" 2>&1
  elif [ "$harness" = omp ]; then
    (cd "$REPO" && PI_CODING_AGENT_DIR="$OMP_AGENT_DIR" \
      "$BIN" --data-dir "$DATA" run "${wrapper_args[@]}" "$harness" "${native_args[@]}") \
      >"$log" 2>&1
  elif [ "$harness" = kimi ]; then
    (cd "$REPO" && KIMI_CODE_HOME="$KIMI_ACCEPTANCE_HOME" \
      "$BIN" --data-dir "$DATA" run "${wrapper_args[@]}" "$harness" "${native_args[@]}") \
      >"$log" 2>&1
  else
    (cd "$REPO" && "$BIN" --data-dir "$DATA" run \
      "${wrapper_args[@]}" "$harness" "${native_args[@]}") >"$log" 2>&1
  fi

  local id results event native_id
  id=$(workstream_id "$WORKSTREAM_NAME")
  results=$("$BIN" --data-dir "$DATA" workstream-search \
    --workstream-id "$id" --limit 100 --json "$current")
  event=$(jq -c --arg agent "$expected_agent" --arg current "$current" \
    '[.[] | select(.agent == $agent and .role == "assistant" and (.content | contains($current)))] | last // empty' \
    <<<"$results")
  [ -n "$event" ] || {
    printf '%s did not persist an assistant event containing %s\n' "$harness" "$current" >&2
    tail -120 "$log" >&2
    return 1
  }
  if [ -n "$previous" ] && ! jq -e --arg previous "$previous" \
    '.content | contains($previous)' <<<"$event" >/dev/null; then
    printf '%s did not demonstrate receipt of prior sentinel %s\n' "$harness" "$previous" >&2
    jq -r '.content' <<<"$event" >&2
    return 1
  fi
  native_id=$(jq -r '.native_session_id' <<<"$event")
  printf '%s\n' "$native_id"
}

WORKSTREAM_NAME="native-acceptance-$(date +%s)-$$"
RUN_TAG="$(date +%s)-$$"
previous=""
first_harness=${harnesses[0]}
first_native=""
index=0
for harness in "${harnesses[@]}"; do
  current="AMWS-$RUN_TAG-$(uppercase "$harness")"
  first_run=0
  [ "$index" -ne 0 ] || first_run=1
  native_id=$(run_harness "$harness" "$current" "$previous" "$first_run")
  if [ "$index" -eq 0 ]; then
    first_native=$native_id
  fi
  previous=$current
  index=$((index + 1))
done

return_sentinel="AMWS-$RUN_TAG-$(uppercase "$first_harness")-RETURN"
returned_native=$(run_harness "$first_harness" "$return_sentinel" "$previous" 0)
[ "$returned_native" = "$first_native" ] || {
  printf '%s resumed native session %s, expected %s\n' \
    "$first_harness" "$returned_native" "$first_native" >&2
  exit 1
}

printf 'real managed-workstream acceptance passed: %s\n' "${harnesses[*]}"
printf 'returned to %s native session %s\n' "$first_harness" "$first_native"
printf 'native harness session stores and resume paths were exercised\n'
