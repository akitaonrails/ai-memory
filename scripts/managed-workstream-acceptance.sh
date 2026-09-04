#!/usr/bin/env bash
# Manual, opt-in acceptance test for managed cross-harness workstreams.
# This is intentionally not called by CI: the real-harness phase uses the
# operator's installed CLIs, credentials, model defaults, and native stores.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BIN=${AI_MEMORY_ACCEPTANCE_BIN:-"$ROOT/target/debug/ai-memory"}
KEEP=${AI_MEMORY_ACCEPTANCE_KEEP:-0}
DETERMINISTIC_ONLY=${AI_MEMORY_ACCEPTANCE_DETERMINISTIC_ONLY:-0}
HARNESS_WORDS=${AI_MEMORY_ACCEPTANCE_HARNESSES:-"claude codex opencode pi crush omp kimi command-code kiro grok antigravity"}
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
git -C "$REPO" commit -q --no-gpg-sign -m "acceptance fixture"

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

workstream_hex() {
  local name=$1
  sqlite3 "$DATA/db/memory.sqlite" \
    "SELECT lower(hex(id)) FROM workstreams WHERE name = '$name' ORDER BY selected_at DESC LIMIT 1;"
}

latest_workstream_sequence() {
  local hex=$1
  sqlite3 "$DATA/db/memory.sqlite" \
    "SELECT COALESCE(MAX(sequence), 0) FROM workstream_events WHERE workstream_id = x'$hex';"
}

current_delivery_cursor() {
  local hex=$1
  local agent=$2
  sqlite3 "$DATA/db/memory.sqlite" \
    "SELECT COALESCE((
       SELECT delivery_cursor FROM workstream_native_sessions
        WHERE workstream_id = x'$hex'
          AND agent_kind = '$agent'
          AND is_current = 1
     ), 0);"
}

agent_observation_count() {
  local agent=$1
  sqlite3 "$DATA/db/memory.sqlite" \
    "SELECT COUNT(*)
       FROM observations AS o
       JOIN sessions AS s ON s.id = o.session_id
      WHERE s.agent_kind = '$agent';"
}

# Assert the deterministic boundaries of one real or fake managed leg:
# the harness produced new portable evidence, and any assigned context delta
# was acknowledged by its hook/launcher delivery path. Adapters with readable
# transcripts persist an assistant event. Antigravity deliberately does not
# decode private trajectory protobuf, so its startup hook must instead persist
# an observation and link the exact native conversation. Model recall is not a
# protocol oracle: large hook results may be file-backed, and whether a model
# chooses to read that file must not decide acceptance.
assert_managed_leg() {
  local hex=$1
  local expected_agent=$2
  local before_sequence=$3
  local before_delivery=$4
  local expect_context=$5
  local log=$6
  local before_observations=${7:-0}
  local native_id delivery sync_after sync_through context_delivered after_observations

  if [ "$expected_agent" = antigravity-cli ]; then
    native_id=$(sqlite3 "$DATA/db/memory.sqlite" \
      "SELECT native_session_id FROM workstream_native_sessions
        WHERE workstream_id = x'$hex'
          AND agent_kind = '$expected_agent'
          AND is_current = 1;")
    [ -n "$native_id" ] || {
      printf '%s did not link a native conversation\n' "$expected_agent" >&2
      tail -120 "$log" >&2
      return 1
    }
    after_observations=$(agent_observation_count "$expected_agent")
    [ "$after_observations" -gt "$before_observations" ] || {
      printf '%s did not persist a startup-hook observation (before: %s, after: %s)\n' \
        "$expected_agent" "$before_observations" "$after_observations" >&2
      tail -120 "$log" >&2
      return 1
    }
  else
    native_id=$(sqlite3 "$DATA/db/memory.sqlite" \
      "SELECT native_session_id FROM workstream_events
         WHERE workstream_id = x'$hex'
           AND sequence > $before_sequence
           AND agent_kind = '$expected_agent'
           AND role = 'assistant'
         ORDER BY sequence DESC LIMIT 1;")
    [ -n "$native_id" ] || {
      printf '%s did not persist a new assistant event after ledger sequence %s\n' \
        "$expected_agent" "$before_sequence" >&2
      tail -120 "$log" >&2
      return 1
    }
  fi

  if [ "$expect_context" = 1 ]; then
    if [ "$before_sequence" -le "$before_delivery" ]; then
      printf '%s had no managed context delta to assign (ledger: %s, cursor: %s)\n' \
        "$expected_agent" "$before_sequence" "$before_delivery" >&2
      tail -120 "$log" >&2
      return 1
    fi
    delivery=$(sqlite3 -separator '|' "$DATA/db/memory.sqlite" \
      "SELECT sync_after, sync_through, context_delivered
         FROM managed_runs
        WHERE workstream_id = x'$hex' AND agent_kind = '$expected_agent'
        ORDER BY started_at DESC LIMIT 1;")
    IFS='|' read -r sync_after sync_through context_delivered <<<"$delivery"
    if [ -z "$delivery" ] || [ "${sync_through:-0}" -ne "$before_sequence" ] ||
      [ "${context_delivered:-0}" -ne 1 ]; then
      printf '%s did not acknowledge its assigned managed context delta (state: %s)\n' \
        "$expected_agent" "${delivery:-missing}" >&2
      tail -120 "$log" >&2
      return 1
    fi
  fi

  printf '%s\n' "$native_id"
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
  kiro)
    printf '%s\n' "$@" >"$AI_MEMORY_ACCEPTANCE_ARGV_LOG"
    # Honor `--resume-id <id>` (resume); a fresh launch mints its own id
    # because Kiro CLI session ids are server-assigned UUIDs.
    session_id=""
    previous_arg=""
    for arg in "$@"; do
      if [ "$previous_arg" = --resume-id ]; then
        session_id=$arg
      fi
      previous_arg=$arg
    done
    if [ -z "$session_id" ]; then
      if command -v uuidgen >/dev/null 2>&1; then
        session_id=$(uuidgen | tr '[:upper:]' '[:lower:]')
      elif [ -r /proc/sys/kernel/random/uuid ]; then
        session_id=$(cat /proc/sys/kernel/random/uuid)
      else
        printf 'kiro fake mode needs uuidgen or /proc/sys/kernel/random/uuid\n' >&2
        exit 1
      fi
    fi
    # Real layout: flat store, one `<uuid>.json` metadata + `<uuid>.jsonl`
    # event-stream pair per session; discovery must read the metadata cwd.
    store="${KIRO_HOME:?kiro fake mode requires KIRO_HOME}/sessions/cli"
    mkdir -p "$store"
    stream="$store/$session_id.jsonl"
    if [ ! -f "$store/$session_id.json" ]; then
      printf '{"session_id":"%s","cwd":"%s","created_at":"2026-08-01T10:00:00Z","updated_at":"2026-08-01T10:00:00Z","title":"acceptance","session_state":{"version":"v1","conversation_metadata":{}}}\n' \
        "$session_id" "$PWD" >"$store/$session_id.json"
      : >"$stream"
    fi
    sentinel=${AI_MEMORY_ACCEPTANCE_SENTINEL:-AMWS-FAKE-KIRO}
    printf '{"version":"v1","kind":"Prompt","data":{"message_id":"m-%s-u","content":[{"kind":"text","data":"%s"}],"meta":{"timestamp":%s}}}\n' \
      "$(date +%s)" "$sentinel" "$(date +%s)000" >>"$stream"
    printf '{"version":"v1","kind":"AssistantMessage","data":{"message_id":"m-%s-a","content":[{"kind":"text","data":"%s reply"}],"meta":{"timestamp":%s}}}\n' \
      "$(date +%s)" "$sentinel" "$(date +%s)000" >>"$stream"
    ;;
  kiro-v3)
    printf '%s\n' "$@" >"$AI_MEMORY_ACCEPTANCE_ARGV_LOG"
    session_id=""
    previous_arg=""
    for arg in "$@"; do
      if [ "$previous_arg" = --resume-id ]; then
        session_id=$arg
      fi
      previous_arg=$arg
    done
    if [ -z "$session_id" ]; then
      if command -v uuidgen >/dev/null 2>&1; then
        session_id="sess_$(uuidgen | tr '[:upper:]' '[:lower:]')"
      elif [ -r /proc/sys/kernel/random/uuid ]; then
        session_id="sess_$(cat /proc/sys/kernel/random/uuid)"
      else
        printf 'kiro v3 fake mode needs uuidgen or /proc/sys/kernel/random/uuid\n' >&2
        exit 1
      fi
    fi
    session_dir="${KIRO_HOME:?kiro v3 fake mode requires KIRO_HOME}/sessions/checkout_fixture/$session_id"
    mkdir -p "$session_dir"
    stream="$session_dir/messages.jsonl"
    if [ ! -f "$session_dir/session.json" ]; then
      printf '{"schemaVersion":"1.0.0","dataModelVersion":1,"id":"%s","workspacePaths":["%s"],"createdAt":"2026-08-06T10:00:00Z","lastModifiedAt":"2026-08-06T10:00:00Z","agentMode":"vibe","status":"idle"}\n' \
        "$session_id" "$PWD" >"$session_dir/session.json"
      : >"$stream"
    fi
    sentinel=${AI_MEMORY_ACCEPTANCE_SENTINEL:-AMWS-FAKE-KIRO-V3}
    printf '{"id":"u-%s","timestamp":"2026-08-06T10:00:00Z","payload":{"type":"user","content":"%s"}}\n' \
      "$(date +%s)" "$sentinel" >>"$stream"
    printf '{"id":"a-%s","timestamp":"2026-08-06T10:00:01Z","payload":{"type":"assistant","operationType":"Say","content":"%s reply"}}\n' \
      "$(date +%s)" "$sentinel" >>"$stream"
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
    # state.json's current cwd field instead of parsing the bucket name.
    session_dir="${KIMI_CODE_HOME:?kimi fake mode requires KIMI_CODE_HOME}/sessions/wd_fixture_bucket/$session_id"
    mkdir -p "$session_dir/agents/main"
    wire="$session_dir/agents/main/wire.jsonl"
    if [ ! -f "$session_dir/state.json" ]; then
      printf '{"id":"%s","version":2,"cwd":"%s"}\n' \
        "$session_id" "$PWD" >"$session_dir/state.json"
      printf '{"type":"metadata","protocol_version":"1","created_at":%s}\n' \
        "$(date +%s)000" >"$wire"
    fi
    sentinel=${AI_MEMORY_ACCEPTANCE_SENTINEL:-AMWS-FAKE-KIMI}
    printf '{"type":"context.append_message","time":%s,"message":{"role":"user","content":[{"type":"text","text":"%s"}],"toolCalls":[]}}\n' \
      "$(date +%s)000" "$sentinel" >>"$wire"
    printf '{"type":"context.append_message","time":%s,"message":{"role":"assistant","content":[{"type":"text","text":"%s reply"}],"toolCalls":[]}}\n' \
      "$(date +%s)000" "$sentinel" >>"$wire"
    ;;
  command-code)
    printf '%s\n' "$@" >"$AI_MEMORY_ACCEPTANCE_ARGV_LOG"
    session_id=""
    previous_arg=""
    for arg in "$@"; do
      if [ "$previous_arg" = --session ]; then
        session_id=$arg
      fi
      previous_arg=$arg
    done
    if [ -z "$session_id" ]; then
      if command -v uuidgen >/dev/null 2>&1; then
        session_id=$(uuidgen | tr '[:upper:]' '[:lower:]')
      elif [ -r /proc/sys/kernel/random/uuid ]; then
        session_id=$(cat /proc/sys/kernel/random/uuid)
      else
        printf 'command-code fake mode needs uuidgen or /proc/sys/kernel/random/uuid\n' >&2
        exit 1
      fi
    fi
    store="$HOME/.commandcode/projects/opaque_fixture_slug"
    mkdir -p "$store"
    stream="$store/$session_id.jsonl"
    if [ ! -f "$stream" ]; then
      printf '{"type":"session","version":3,"id":"%s","timestamp":"2026-08-07T17:00:00Z","cwd":"%s"}\n' \
        "$session_id" "$PWD" >"$stream"
    fi
    sentinel=${AI_MEMORY_ACCEPTANCE_SENTINEL:-AMWS-FAKE-COMMAND-CODE}
    now=$(date +%s)
    printf '{"type":"message","id":"u-%s","parentId":null,"timestamp":"2026-08-07T17:00:01Z","message":{"role":"user","content":[{"type":"text","text":"%s"}],"meta":{"source":"user"}}}\n' \
      "$now" "$sentinel" >>"$stream"
    printf '{"type":"message","id":"a-%s","parentId":"u-%s","timestamp":"2026-08-07T17:00:02Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"private reasoning","signature":"private signature"},{"type":"text","text":"%s reply"}],"meta":{"source":"model"}}}\n' \
      "$now" "$now" "$sentinel" >>"$stream"
    ;;
  grok)
    printf '%s\n' "$@" >"$AI_MEMORY_ACCEPTANCE_ARGV_LOG"
    # Honor the wrapper-owned `--session-id <id>` (fresh) and `--resume <id>`
    # (returning) selectors the way the real CLI does.
    session_id=""
    previous_arg=""
    for arg in "$@"; do
      case "$previous_arg" in
        --session-id | --resume) session_id=$arg ;;
      esac
      previous_arg=$arg
    done
    [ -n "$session_id" ] || session_id="019f-fake-$(date +%s)-$$"
    # The bucket directory name is a URL-encoded cwd in the real layout, but
    # discovery must read summary.json's info.cwd instead of parsing it.
    session_dir="${GROK_HOME:?grok fake mode requires GROK_HOME}/sessions/%2Ffixture%2Fbucket/$session_id"
    mkdir -p "$session_dir"
    chat="$session_dir/chat_history.jsonl"
    if [ ! -f "$session_dir/summary.json" ]; then
      printf '{"info":{"id":"%s","cwd":"%s"}}\n' "$session_id" "$PWD" >"$session_dir/summary.json"
      printf '{"type":"system","content":"fake grok system prompt"}\n' >"$chat"
    fi
    sentinel=${AI_MEMORY_ACCEPTANCE_SENTINEL:-AMWS-FAKE-GROK}
    printf '{"type":"user","content":[{"type":"text","text":"%s"}]}\n' "$sentinel" >>"$chat"
    printf '{"type":"assistant","content":"%s reply"}\n' "$sentinel" >>"$chat"
    ;;
  antigravity)
    printf '%s\n' "$@" >"$AI_MEMORY_ACCEPTANCE_ARGV_LOG"
    session_id=""
    previous_arg=""
    for arg in "$@"; do
      if [ "$previous_arg" = --conversation ]; then
        session_id=$arg
      fi
      previous_arg=$arg
    done
    if [ -z "$session_id" ]; then
      session_id=${AI_MEMORY_ACCEPTANCE_ANTIGRAVITY_SESSION_ID:?antigravity fake mode requires a fresh session id}
    fi
    conversations="$HOME/.gemini/antigravity-cli/conversations"
    mkdir -p "$conversations"
    database="$conversations/$session_id.db"
    if [ ! -f "$database" ]; then
      uri="file://$PWD"
      uri_hex=$(printf '%s' "$uri" | od -An -tx1 | tr -d ' \n')
      printf -v uri_length '%02x' "${#uri}"
      nested="0a${uri_length}${uri_hex}"
      printf -v nested_length '%02x' "$((2 + ${#uri}))"
      metadata="0a${nested_length}${nested}"
      sqlite3 "$database" \
        "CREATE TABLE trajectory_metadata_blob (id text DEFAULT 'main', data blob, PRIMARY KEY (id));
         INSERT INTO trajectory_metadata_blob (id, data) VALUES ('main', x'$metadata');
         CREATE TABLE trajectory_blob (data blob);
         INSERT INTO trajectory_blob (data) VALUES (x'414d57532d505249564154452d5452414a4543544f5259');"
    fi
    payload=$(jq -nc --arg id "$session_id" --arg cwd "$PWD" \
      '{invocationNum: 0, conversationId: $id, workspacePaths: [$cwd]}')
    printf '%s' "$payload" | \
      AI_MEMORY_HOOK_URL="${AI_MEMORY_SERVER_URL:?}" \
      "$AI_MEMORY_ACCEPTANCE_ANTIGRAVITY_HOOK" \
      >"$AI_MEMORY_ACCEPTANCE_ANTIGRAVITY_HOOK_LOG"
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
AUTO_COMMAND_CODE_HOME="$AUTO_HOME/.commandcode"
AUTO_BIN="$AUTO_HOME/bin"
mkdir -p "$AUTO_CODEX_HOME/sessions/2026/01/01" \
  "$AUTO_CLAUDE_HOME/projects/fixture" \
  "$AUTO_COMMAND_CODE_HOME/projects/opaque_fixture_slug" "$AUTO_BIN"
ln -s "$FAKE" "$AUTO_BIN/codex"
ln -s "$FAKE" "$AUTO_BIN/claude"
ln -s "$FAKE" "$AUTO_BIN/command-code"
printf '{"sessionId":"auto-claude-old","cwd":"%s"}\n' "$REPO" \
  >"$AUTO_CLAUDE_HOME/projects/fixture/auto-claude-old.jsonl"
sleep 1
printf '{"type":"session_meta","payload":{"id":"auto-codex-new","cwd":"%s"}}\n' \
  "$REPO" >"$AUTO_CODEX_HOME/sessions/2026/01/01/rollout-auto.jsonl"
sleep 1
AUTO_COMMAND_CODE_ID="7c1d5698-204a-4c0f-ae9c-43db7fc4e41d"
printf '{"type":"session","version":3,"id":"%s","timestamp":"2026-08-07T17:00:00Z","cwd":"%s"}\n' \
  "$AUTO_COMMAND_CODE_ID" "$REPO" \
  >"$AUTO_COMMAND_CODE_HOME/projects/opaque_fixture_slug/$AUTO_COMMAND_CODE_ID.jsonl"
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
  <(printf '%s\n' --session "$AUTO_COMMAND_CODE_ID" --yolo) \
  "$TMP/auto-newest-argv.log"

# Establish Claude after Command Code, then verify bare run follows the managed
# workstream's current Claude link instead of newer but obsolete native files.
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
printf '{"sessionId":"%s","cwd":"%s"}\n' "$auto_claude_id" "$REPO" \
  >"$AUTO_CLAUDE_HOME/projects/fixture/$auto_claude_id.jsonl"
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

# Deleting the linked native store must heal the established workstream instead
# of feeding the dead id back to Kimi forever.
rm -rf "$kimi_session_dir"
(
  cd "$REPO"
  KIMI_CODE_HOME="$KIMI_FAKE_HOME" \
  AI_MEMORY_ACCEPTANCE_FAKE_MODE=kimi \
  AI_MEMORY_ACCEPTANCE_ARGV_LOG="$TMP/kimi-orphan-argv.log" \
  AI_MEMORY_ACCEPTANCE_SENTINEL="AMWS-FAKE-KIMI-RECOVERED" \
    "$BIN" --data-dir "$DATA" run --workstream edge-kimi --executable "$FAKE" \
      kimi >"$LOGS/edge-kimi-orphan.log" 2>&1
)
if grep -q . "$TMP/kimi-orphan-argv.log"; then
  printf 'orphaned kimi session was not replaced by a fresh launch\n' >&2
  cat "$TMP/kimi-orphan-argv.log" >&2
  exit 1
fi
grep -q "linked kimi session .* is missing from its native store" \
  "$LOGS/edge-kimi-orphan.log"
kimi_recovered_dir=$(find "$KIMI_FAKE_HOME/sessions" -mindepth 2 -maxdepth 2 -type d -print -quit)
[ -n "$kimi_recovered_dir" ] || {
  printf 'orphan recovery did not create a replacement kimi session\n' >&2
  exit 1
}
kimi_recovered_id=$(basename "$kimi_recovered_dir")
[ "$kimi_recovered_id" != "$kimi_session_id" ] || {
  printf 'orphan recovery reused the deleted kimi session id\n' >&2
  exit 1
}
kimi_current_id=$(sqlite3 "$DATA/db/memory.sqlite" \
  "SELECT native_session_id FROM workstream_native_sessions WHERE workstream_id = x'${kimi_ws_hex}' AND agent_kind = 'kimi-code' AND is_current = 1;")
[ "$kimi_current_id" = "$kimi_recovered_id" ] || {
  printf 'orphan recovery did not repoint the kimi workstream\n' >&2
  exit 1
}

# Command Code v3 fixture: the fake creates an exact UUID transcript under an
# opaque project slug, then accepts the same `--session <uuid>` selector on the
# returning leg. HOME isolates the documented user store from real sessions.
COMMAND_CODE_FAKE_HOME="$CONFIG/command-code-fake"
mkdir -p "$COMMAND_CODE_FAKE_HOME"
(
  cd "$REPO"
  HOME="$COMMAND_CODE_FAKE_HOME" \
  AI_MEMORY_ACCEPTANCE_FAKE_MODE=command-code \
  AI_MEMORY_ACCEPTANCE_ARGV_LOG="$TMP/command-code-first-argv.log" \
  AI_MEMORY_ACCEPTANCE_SENTINEL="AMWS-FAKE-COMMAND-CODE-ONE" \
    "$BIN" --data-dir "$DATA" run --new edge-command-code --executable "$FAKE" \
      --yolo command-code >"$LOGS/edge-command-code-first.log" 2>&1
)
diff -u <(printf '%s\n' --yolo) "$TMP/command-code-first-argv.log"
command_code_session_file=$(find "$COMMAND_CODE_FAKE_HOME/.commandcode/projects" \
  -mindepth 2 -maxdepth 2 -name '*.jsonl' ! -name '*.checkpoints.jsonl' -print -quit)
[ -n "$command_code_session_file" ] || {
  printf 'fake command-code did not create a native session transcript\n' >&2
  exit 1
}
command_code_session_id=$(basename "$command_code_session_file" .jsonl)
command_code_ws_hex=$(sqlite3 "$DATA/db/memory.sqlite" \
  "SELECT lower(hex(id)) FROM workstreams WHERE name = 'edge-command-code' ORDER BY selected_at DESC LIMIT 1;")
[ "${#command_code_ws_hex}" -eq 32 ] || {
  printf 'could not resolve the edge-command-code workstream id\n' >&2
  exit 1
}
command_code_ws_id="${command_code_ws_hex:0:8}-${command_code_ws_hex:8:4}-${command_code_ws_hex:12:4}-${command_code_ws_hex:16:4}-${command_code_ws_hex:20:12}"
command_code_first_hits=$("$BIN" --data-dir "$DATA" workstream-search \
  --workstream-id "$command_code_ws_id" --limit 100 --json "AMWS-FAKE-COMMAND-CODE-ONE")
jq -e --arg id "$command_code_session_id" \
  '[.[] | select(.agent == "command-code" and .role == "assistant" and (.content | contains("AMWS-FAKE-COMMAND-CODE-ONE")) and .native_session_id == $id)] | length == 1' \
  <<<"$command_code_first_hits" >/dev/null || {
  printf 'command-code transcript sentinel was not imported\n' >&2
  tail -80 "$LOGS/edge-command-code-first.log" >&2
  exit 1
}
(
  cd "$REPO"
  HOME="$COMMAND_CODE_FAKE_HOME" \
  AI_MEMORY_ACCEPTANCE_FAKE_MODE=command-code \
  AI_MEMORY_ACCEPTANCE_ARGV_LOG="$TMP/command-code-second-argv.log" \
  AI_MEMORY_ACCEPTANCE_SENTINEL="AMWS-FAKE-COMMAND-CODE-TWO" \
    "$BIN" --data-dir "$DATA" run --workstream edge-command-code --executable "$FAKE" \
      cmdc >"$LOGS/edge-command-code-second.log" 2>&1
)
diff -u <(printf '%s\n' --session "$command_code_session_id") \
  "$TMP/command-code-second-argv.log"
command_code_second_hits=$("$BIN" --data-dir "$DATA" workstream-search \
  --workstream-id "$command_code_ws_id" --limit 100 --json "AMWS-FAKE-COMMAND-CODE")
jq -e \
  '([.[] | select(.role == "assistant" and (.content | contains("AMWS-FAKE-COMMAND-CODE-ONE")))] | length == 1)
   and ([.[] | select(.role == "assistant" and (.content | contains("AMWS-FAKE-COMMAND-CODE-TWO")))] | length == 1)' \
  <<<"$command_code_second_hits" >/dev/null || {
  printf 'command-code incremental import duplicated or missed a sentinel\n' >&2
  tail -80 "$LOGS/edge-command-code-second.log" >&2
  exit 1
}

# Kiro fake-mode fixtures cover both incompatible native stores. The fake owns
# each fresh id, writes the sanitized schema observed in live acceptance, and
# honors the exact engine-specific resume selected by the wrapper.
KIRO_FAKE_HOME="$CONFIG/kiro-fake"
mkdir -p "$KIRO_FAKE_HOME"
(
  cd "$REPO"
  KIRO_HOME="$KIRO_FAKE_HOME" \
  AI_MEMORY_ACCEPTANCE_FAKE_MODE=kiro \
  AI_MEMORY_ACCEPTANCE_ARGV_LOG="$TMP/kiro-first-argv.log" \
  AI_MEMORY_ACCEPTANCE_SENTINEL="AMWS-FAKE-KIRO-ONE" \
    "$BIN" --data-dir "$DATA" run --new edge-kiro --executable "$FAKE" \
      --yolo kiro >"$LOGS/edge-kiro-first.log" 2>&1
)
diff -u <(printf '%s\n' --trust-all-tools) "$TMP/kiro-first-argv.log"
kiro_session_file=$(find "$KIRO_FAKE_HOME/sessions/cli" -maxdepth 1 -name '*.jsonl' -print -quit)
[ -n "$kiro_session_file" ] || {
  printf 'fake kiro did not create a native session store\n' >&2
  exit 1
}
kiro_session_id=$(basename "$kiro_session_file" .jsonl)
kiro_ws_hex=$(sqlite3 "$DATA/db/memory.sqlite" \
  "SELECT lower(hex(id)) FROM workstreams WHERE name = 'edge-kiro' ORDER BY selected_at DESC LIMIT 1;")
[ "${#kiro_ws_hex}" -eq 32 ] || {
  printf 'could not resolve the edge-kiro workstream id\n' >&2
  exit 1
}
kiro_ws_id="${kiro_ws_hex:0:8}-${kiro_ws_hex:8:4}-${kiro_ws_hex:12:4}-${kiro_ws_hex:16:4}-${kiro_ws_hex:20:12}"
kiro_first_hits=$("$BIN" --data-dir "$DATA" workstream-search \
  --workstream-id "$kiro_ws_id" --limit 100 --json "AMWS-FAKE-KIRO-ONE")
jq -e --arg id "$kiro_session_id" \
  '[.[] | select(.agent == "kiro-cli" and .role == "assistant" and (.content | contains("AMWS-FAKE-KIRO-ONE")) and .native_session_id == $id)] | length == 1' \
  <<<"$kiro_first_hits" >/dev/null || {
  printf 'kiro event-stream sentinel was not imported from the discovered session\n' >&2
  tail -80 "$LOGS/edge-kiro-first.log" >&2
  exit 1
}
(
  cd "$REPO"
  KIRO_HOME="$KIRO_FAKE_HOME" \
  AI_MEMORY_ACCEPTANCE_FAKE_MODE=kiro \
  AI_MEMORY_ACCEPTANCE_ARGV_LOG="$TMP/kiro-second-argv.log" \
  AI_MEMORY_ACCEPTANCE_SENTINEL="AMWS-FAKE-KIRO-TWO" \
    "$BIN" --data-dir "$DATA" run --workstream edge-kiro --executable "$FAKE" \
      kiro-cli >"$LOGS/edge-kiro-second.log" 2>&1
)
diff -u <(printf '%s\n' --resume-id "$kiro_session_id") "$TMP/kiro-second-argv.log"
kiro_second_hits=$("$BIN" --data-dir "$DATA" workstream-search \
  --workstream-id "$kiro_ws_id" --limit 100 --json "AMWS-FAKE-KIRO")
jq -e \
  '([.[] | select(.role == "assistant" and (.content | contains("AMWS-FAKE-KIRO-ONE")))] | length == 1)
   and ([.[] | select(.role == "assistant" and (.content | contains("AMWS-FAKE-KIRO-TWO")))] | length == 1)' \
  <<<"$kiro_second_hits" >/dev/null || {
  printf 'kiro incremental import duplicated or missed a round sentinel\n' >&2
  tail -80 "$LOGS/edge-kiro-second.log" >&2
  exit 1
}
(
  cd "$REPO"
  KIRO_HOME="$KIRO_FAKE_HOME" \
  AI_MEMORY_ACCEPTANCE_FAKE_MODE=kiro-v3 \
  AI_MEMORY_ACCEPTANCE_ARGV_LOG="$TMP/kiro-v3-first-argv.log" \
  AI_MEMORY_ACCEPTANCE_SENTINEL="AMWS-FAKE-KIRO-V3-ONE" \
    "$BIN" --data-dir "$DATA" run --new edge-kiro-v3 --executable "$FAKE" \
      --yolo kiro --v3 >"$LOGS/edge-kiro-v3-first.log" 2>&1
)
diff -u <(printf '%s\n' --v3) "$TMP/kiro-v3-first-argv.log"
kiro_v3_session_dir=$(find "$KIRO_FAKE_HOME/sessions" -mindepth 2 -maxdepth 2 \
  -type d -name 'sess_*' -print -quit)
[ -n "$kiro_v3_session_dir" ] || {
  printf 'fake kiro v3 did not create a native session store\n' >&2
  exit 1
}
kiro_v3_session_id=$(basename "$kiro_v3_session_dir")
kiro_v3_ws_hex=$(sqlite3 "$DATA/db/memory.sqlite" \
  "SELECT lower(hex(id)) FROM workstreams WHERE name = 'edge-kiro-v3' ORDER BY selected_at DESC LIMIT 1;")
kiro_v3_ws_id="${kiro_v3_ws_hex:0:8}-${kiro_v3_ws_hex:8:4}-${kiro_v3_ws_hex:12:4}-${kiro_v3_ws_hex:16:4}-${kiro_v3_ws_hex:20:12}"
kiro_v3_first_hits=$("$BIN" --data-dir "$DATA" workstream-search \
  --workstream-id "$kiro_v3_ws_id" --limit 100 --json "AMWS-FAKE-KIRO-V3-ONE")
jq -e --arg id "$kiro_v3_session_id" \
  '[.[] | select(.agent == "kiro-cli" and .role == "assistant" and (.content | contains("AMWS-FAKE-KIRO-V3-ONE")) and .native_session_id == $id)] | length == 1' \
  <<<"$kiro_v3_first_hits" >/dev/null || {
  printf 'kiro v3 sentinel was not imported from the discovered session\n' >&2
  tail -80 "$LOGS/edge-kiro-v3-first.log" >&2
  exit 1
}
(
  cd "$REPO"
  KIRO_HOME="$KIRO_FAKE_HOME" \
  AI_MEMORY_ACCEPTANCE_FAKE_MODE=kiro-v3 \
  AI_MEMORY_ACCEPTANCE_ARGV_LOG="$TMP/kiro-v3-second-argv.log" \
  AI_MEMORY_ACCEPTANCE_SENTINEL="AMWS-FAKE-KIRO-V3-TWO" \
    "$BIN" --data-dir "$DATA" run --workstream edge-kiro-v3 --executable "$FAKE" \
      kiro >"$LOGS/edge-kiro-v3-second.log" 2>&1
)
diff -u <(printf '%s\n' --v3 --resume-id "$kiro_v3_session_id") \
  "$TMP/kiro-v3-second-argv.log"
kiro_v3_second_hits=$("$BIN" --data-dir "$DATA" workstream-search \
  --workstream-id "$kiro_v3_ws_id" --limit 100 --json "AMWS-FAKE-KIRO-V3")
jq -e \
  '([.[] | select(.role == "assistant" and (.content | contains("AMWS-FAKE-KIRO-V3-ONE")))] | length == 1)
   and ([.[] | select(.role == "assistant" and (.content | contains("AMWS-FAKE-KIRO-V3-TWO")))] | length == 1)' \
  <<<"$kiro_v3_second_hits" >/dev/null || {
  printf 'kiro v3 incremental import duplicated or missed a round sentinel\n' >&2
  tail -80 "$LOGS/edge-kiro-v3-second.log" >&2
  exit 1
}

# Antigravity fake-mode fixture: `agy` owns the conversation id and SQLite
# database, while its real PreInvocation hook links that id and accepts any
# pending workstream context. The private trajectory table is never decoded or
# copied into the ledger.
ANTIGRAVITY_FAKE_HOME="$CONFIG/antigravity-fake"
ANTIGRAVITY_FAKE_ID="a0d5ac62-2501-4780-b783-76d159c56cb3"
ANTIGRAVITY_HOOK="$ROOT/hooks/antigravity-cli/session-start.sh"
mkdir -p "$ANTIGRAVITY_FAKE_HOME"
antigravity_first_observations=$(agent_observation_count antigravity-cli)
(
  cd "$REPO"
  HOME="$ANTIGRAVITY_FAKE_HOME" \
  AI_MEMORY_ACCEPTANCE_FAKE_MODE=antigravity \
  AI_MEMORY_ACCEPTANCE_ARGV_LOG="$TMP/antigravity-first-argv.log" \
  AI_MEMORY_ACCEPTANCE_ANTIGRAVITY_SESSION_ID="$ANTIGRAVITY_FAKE_ID" \
  AI_MEMORY_ACCEPTANCE_ANTIGRAVITY_HOOK="$ANTIGRAVITY_HOOK" \
  AI_MEMORY_ACCEPTANCE_ANTIGRAVITY_HOOK_LOG="$TMP/antigravity-first-hook.json" \
    "$BIN" --data-dir "$DATA" run --new edge-antigravity --executable "$FAKE" \
      --yolo antigravity >"$LOGS/edge-antigravity-first.log" 2>&1
)
diff -u <(printf '%s\n' --dangerously-skip-permissions) \
  "$TMP/antigravity-first-argv.log"
antigravity_ws_hex=$(workstream_hex edge-antigravity)
[ "${#antigravity_ws_hex}" -eq 32 ] || {
  printf 'could not resolve the edge-antigravity workstream id\n' >&2
  exit 1
}
antigravity_native=$(assert_managed_leg "$antigravity_ws_hex" antigravity-cli 0 0 0 \
  "$LOGS/edge-antigravity-first.log" "$antigravity_first_observations")
[ "$antigravity_native" = "$ANTIGRAVITY_FAKE_ID" ] || {
  printf 'Antigravity linked native conversation %s, expected %s\n' \
    "$antigravity_native" "$ANTIGRAVITY_FAKE_ID" >&2
  exit 1
}
antigravity_ws_id="${antigravity_ws_hex:0:8}-${antigravity_ws_hex:8:4}-${antigravity_ws_hex:12:4}-${antigravity_ws_hex:16:4}-${antigravity_ws_hex:20:12}"
private_hits=$("$BIN" --data-dir "$DATA" workstream-search \
  --workstream-id "$antigravity_ws_id" --limit 100 --json "PRIVATE")
jq -e 'length == 0' <<<"$private_hits" >/dev/null || {
  printf 'Antigravity private trajectory payload leaked into the workstream ledger\n' >&2
  exit 1
}

antigravity_second_before=$(latest_workstream_sequence "$antigravity_ws_hex")
antigravity_second_delivery=$(current_delivery_cursor "$antigravity_ws_hex" antigravity-cli)
antigravity_second_observations=$(agent_observation_count antigravity-cli)
(
  cd "$REPO"
  HOME="$ANTIGRAVITY_FAKE_HOME" \
  AI_MEMORY_ACCEPTANCE_FAKE_MODE=antigravity \
  AI_MEMORY_ACCEPTANCE_ARGV_LOG="$TMP/antigravity-second-argv.log" \
  AI_MEMORY_ACCEPTANCE_ANTIGRAVITY_SESSION_ID="$ANTIGRAVITY_FAKE_ID" \
  AI_MEMORY_ACCEPTANCE_ANTIGRAVITY_HOOK="$ANTIGRAVITY_HOOK" \
  AI_MEMORY_ACCEPTANCE_ANTIGRAVITY_HOOK_LOG="$TMP/antigravity-second-hook.json" \
    "$BIN" --data-dir "$DATA" run --workstream edge-antigravity \
      --executable "$FAKE" agy >"$LOGS/edge-antigravity-second.log" 2>&1
)
diff -u <(printf '%s\n' --conversation "$ANTIGRAVITY_FAKE_ID") \
  "$TMP/antigravity-second-argv.log"
returned_antigravity=$(assert_managed_leg "$antigravity_ws_hex" antigravity-cli \
  "$antigravity_second_before" "$antigravity_second_delivery" 0 \
  "$LOGS/edge-antigravity-second.log" "$antigravity_second_observations")
[ "$returned_antigravity" = "$ANTIGRAVITY_FAKE_ID" ] || {
  printf 'returning Antigravity launch did not resume %s\n' "$ANTIGRAVITY_FAKE_ID" >&2
  exit 1
}

# A fresh Antigravity conversation joining Kimi's established workstream must
# receive the prior visible ledger through the real hook's injectSteps output.
ANTIGRAVITY_CROSS_HOME="$CONFIG/antigravity-cross"
ANTIGRAVITY_CROSS_ID="9576275f-7c4e-4709-b372-22d1ad2a0af8"
mkdir -p "$ANTIGRAVITY_CROSS_HOME"
antigravity_cross_before=$(latest_workstream_sequence "$kimi_ws_hex")
antigravity_cross_delivery=$(current_delivery_cursor "$kimi_ws_hex" antigravity-cli)
antigravity_cross_observations=$(agent_observation_count antigravity-cli)
(
  cd "$REPO"
  HOME="$ANTIGRAVITY_CROSS_HOME" \
  AI_MEMORY_ACCEPTANCE_FAKE_MODE=antigravity \
  AI_MEMORY_ACCEPTANCE_ARGV_LOG="$TMP/antigravity-cross-argv.log" \
  AI_MEMORY_ACCEPTANCE_ANTIGRAVITY_SESSION_ID="$ANTIGRAVITY_CROSS_ID" \
  AI_MEMORY_ACCEPTANCE_ANTIGRAVITY_HOOK="$ANTIGRAVITY_HOOK" \
  AI_MEMORY_ACCEPTANCE_ANTIGRAVITY_HOOK_LOG="$TMP/antigravity-cross-hook.json" \
    "$BIN" --data-dir "$DATA" run --workstream edge-kimi --executable "$FAKE" \
      antigravity-cli >"$LOGS/edge-antigravity-cross.log" 2>&1
)
if grep -q . "$TMP/antigravity-cross-argv.log"; then
  printf 'fresh Antigravity launch unexpectedly received a session selector\n' >&2
  cat "$TMP/antigravity-cross-argv.log" >&2
  exit 1
fi
jq -e '.injectSteps[0].ephemeralMessage | contains("AMWS-FAKE-KIMI")' \
  "$TMP/antigravity-cross-hook.json" >/dev/null || {
  printf 'Antigravity hook did not receive the prior Kimi workstream history\n' >&2
  cat "$TMP/antigravity-cross-hook.json" >&2
  exit 1
}
assert_managed_leg "$kimi_ws_hex" antigravity-cli "$antigravity_cross_before" \
  "$antigravity_cross_delivery" 1 "$LOGS/edge-antigravity-cross.log" \
  "$antigravity_cross_observations" >/dev/null

# Grok fake-mode fixture: the fake grok honors the wrapper's `--session-id`
# (fresh) and `--resume <id>` (returning) selectors, writes the native store
# layout ($GROK_HOME/sessions/<bucket>/<id>/summary.json plus
# chat_history.jsonl), and appends the round sentinel. The returning launch
# must also receive the undelivered workstream packet through `--rules`.
GROK_FAKE_HOME="$CONFIG/grok-fake"
mkdir -p "$GROK_FAKE_HOME"
(
  cd "$REPO"
  GROK_HOME="$GROK_FAKE_HOME" \
  AI_MEMORY_ACCEPTANCE_FAKE_MODE=grok \
  AI_MEMORY_ACCEPTANCE_ARGV_LOG="$TMP/grok-first-argv.log" \
  AI_MEMORY_ACCEPTANCE_SENTINEL="AMWS-FAKE-GROK-ONE" \
    "$BIN" --data-dir "$DATA" run --new edge-grok --executable "$FAKE" \
      grok >"$LOGS/edge-grok-first.log" 2>&1
)
grep -qx -- '--session-id' "$TMP/grok-first-argv.log" || {
  printf 'fresh grok launch did not receive a wrapper session id\n' >&2
  cat "$TMP/grok-first-argv.log" >&2
  exit 1
}
if grep -qx -- '--rules' "$TMP/grok-first-argv.log"; then
  printf 'fresh grok launch on an empty workstream unexpectedly received --rules\n' >&2
  exit 1
fi
grok_session_id=$(basename "$(find "$GROK_FAKE_HOME/sessions" -mindepth 2 -maxdepth 2 -type d -print -quit)")
[ -n "$grok_session_id" ] || {
  printf 'fake grok did not create a native session store\n' >&2
  exit 1
}
grok_ws_hex=$(sqlite3 "$DATA/db/memory.sqlite" \
  "SELECT lower(hex(id)) FROM workstreams WHERE name = 'edge-grok' ORDER BY selected_at DESC LIMIT 1;")
[ "${#grok_ws_hex}" -eq 32 ] || {
  printf 'could not resolve the edge-grok workstream id\n' >&2
  exit 1
}
grok_ws_id="${grok_ws_hex:0:8}-${grok_ws_hex:8:4}-${grok_ws_hex:12:4}-${grok_ws_hex:16:4}-${grok_ws_hex:20:12}"
grok_first_hits=$("$BIN" --data-dir "$DATA" workstream-search \
  --workstream-id "$grok_ws_id" --limit 100 --json "AMWS-FAKE-GROK-ONE")
jq -e --arg id "$grok_session_id" \
  '[.[] | select(.agent == "grok" and .role == "assistant" and (.content | contains("AMWS-FAKE-GROK-ONE")) and .native_session_id == $id)] | length == 1' \
  <<<"$grok_first_hits" >/dev/null || {
  printf 'grok chat_history.jsonl sentinel was not imported\n' >&2
  tail -80 "$LOGS/edge-grok-first.log" >&2
  exit 1
}
# Returning to the same grok session must resume it (`--resume <id>`) and,
# because that session already produced every workstream event, receive no
# context packet.
(
  cd "$REPO"
  GROK_HOME="$GROK_FAKE_HOME" \
  AI_MEMORY_ACCEPTANCE_FAKE_MODE=grok \
  AI_MEMORY_ACCEPTANCE_ARGV_LOG="$TMP/grok-second-argv.log" \
  AI_MEMORY_ACCEPTANCE_SENTINEL="AMWS-FAKE-GROK-TWO" \
    "$BIN" --data-dir "$DATA" run --workstream edge-grok --executable "$FAKE" \
      grok-build >"$LOGS/edge-grok-second.log" 2>&1
)
grep -A1 -x -- '--resume' "$TMP/grok-second-argv.log" | grep -qx "$grok_session_id" || {
  printf 'returning grok launch did not resume the linked session\n' >&2
  cat "$TMP/grok-second-argv.log" >&2
  exit 1
}
if grep -qx -- '--rules' "$TMP/grok-second-argv.log"; then
  printf 'same-session grok resume unexpectedly received a context packet\n' >&2
  exit 1
fi
grok_second_hits=$("$BIN" --data-dir "$DATA" workstream-search \
  --workstream-id "$grok_ws_id" --limit 100 --json "AMWS-FAKE-GROK")
jq -e \
  '([.[] | select(.role == "assistant" and (.content | contains("AMWS-FAKE-GROK-ONE")))] | length == 1)
   and ([.[] | select(.role == "assistant" and (.content | contains("AMWS-FAKE-GROK-TWO")))] | length == 1)' \
  <<<"$grok_second_hits" >/dev/null || {
  printf 'grok incremental import duplicated or missed a round sentinel\n' >&2
  tail -80 "$LOGS/edge-grok-second.log" >&2
  exit 1
}

# A fresh grok session joining the established edge-kimi workstream must
# receive that workstream's history through `--rules`, with historical tool
# activity labelled as completed evidence.
grok_cross_before=$(latest_workstream_sequence "$kimi_ws_hex")
grok_cross_delivery=$(current_delivery_cursor "$kimi_ws_hex" grok)
(
  cd "$REPO"
  GROK_HOME="$GROK_FAKE_HOME" \
  AI_MEMORY_ACCEPTANCE_FAKE_MODE=grok \
  AI_MEMORY_ACCEPTANCE_ARGV_LOG="$TMP/grok-cross-argv.log" \
  AI_MEMORY_ACCEPTANCE_SENTINEL="AMWS-FAKE-GROK-CROSS" \
    "$BIN" --data-dir "$DATA" run --workstream edge-kimi --executable "$FAKE" \
      grok >"$LOGS/edge-grok-cross.log" 2>&1
)
grep -qx -- '--session-id' "$TMP/grok-cross-argv.log" || {
  printf 'grok joining an established workstream did not start a fresh session\n' >&2
  cat "$TMP/grok-cross-argv.log" >&2
  exit 1
}
grep -qx -- '--rules' "$TMP/grok-cross-argv.log" || {
  printf 'grok joining an established workstream did not receive --rules context\n' >&2
  cat "$TMP/grok-cross-argv.log" >&2
  exit 1
}
grep -q 'AMWS-FAKE-KIMI' "$TMP/grok-cross-argv.log" || {
  printf 'the grok --rules packet did not carry the prior harness history\n' >&2
  exit 1
}
assert_managed_leg "$kimi_ws_hex" grok "$grok_cross_before" \
  "$grok_cross_delivery" 1 \
  "$LOGS/edge-grok-cross.log" >/dev/null

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

# `ai-memory workstreams` must answer from the same checkout identity `run`
# selects with, without turning that identity into readable output. The list is
# taken from the repository the previous legs launched in, so `edge-adopt` --
# the workstream the Claude adoption leg selected last -- has to lead it.
workstreams_json=$(cd "$REPO" && "$BIN" --data-dir "$DATA" workstreams --json)
jq -e '(type == "array") and (length > 1)' <<<"$workstreams_json" >/dev/null || {
  printf 'workstreams did not list this checkout\n' >&2
  printf '%s\n' "$workstreams_json" >&2
  exit 1
}
jq -e '[.[] | select(.current)] | length == 1' <<<"$workstreams_json" >/dev/null || {
  printf 'workstreams did not mark exactly one current selection\n' >&2
  printf '%s\n' "$workstreams_json" >&2
  exit 1
}
jq -e '.[0].current and .[0].name == "edge-adopt"' <<<"$workstreams_json" >/dev/null || {
  printf 'workstreams did not lead with the checkout current workstream\n' >&2
  printf '%s\n' "$workstreams_json" >&2
  exit 1
}
jq -e --arg name edge-kimi \
  '[.[] | select(.name == $name and (.linked_harnesses | index("kimi-code")))] | length == 1' \
  <<<"$workstreams_json" >/dev/null || {
  printf 'workstreams did not report the harnesses linked to edge-kimi\n' >&2
  printf '%s\n' "$workstreams_json" >&2
  exit 1
}
# Discovery is a read of workstream metadata only: the checkout path, its
# fingerprints, and native session ids must never travel back to the client.
for leaked in "$REPO" "$kimi_session_id" adoption-codex-id; do
  ! grep -qF -- "$leaked" <<<"$workstreams_json" || {
    printf 'workstreams leaked private checkout or native session data: %s\n' \
      "$leaked" >&2
    exit 1
  }
done
limited_json=$(cd "$REPO" && "$BIN" --data-dir "$DATA" workstreams --limit 1 --json)
jq -e 'length == 1 and .[0].current' <<<"$limited_json" >/dev/null || {
  printf 'workstreams --limit did not keep the current selection\n' >&2
  printf '%s\n' "$limited_json" >&2
  exit 1
}
workstreams_human=$(cd "$REPO" && "$BIN" --data-dir "$DATA" workstreams)
grep -q '^\* edge-adopt' <<<"$workstreams_human" || {
  printf 'human workstreams output did not mark the current selection\n' >&2
  printf '%s\n' "$workstreams_human" >&2
  exit 1
}

# A different worktree of the same project shares the workspace and project
# names but must not inherit the first checkout list.
OTHER_REPO="$TMP/repo-elsewhere"
mkdir -p "$OTHER_REPO"
git -C "$OTHER_REPO" init -q
git -C "$OTHER_REPO" config user.name "ai-memory acceptance"
git -C "$OTHER_REPO" config user.email "acceptance@localhost"
printf '# elsewhere\n' >"$OTHER_REPO/README.md"
git -C "$OTHER_REPO" add README.md
git -C "$OTHER_REPO" commit -q --no-gpg-sign -m "acceptance fixture"
other_json=$(cd "$OTHER_REPO" && "$BIN" --data-dir "$DATA" workstreams \
  --project "$(basename "$REPO")" --json)
jq -e 'length == 0' <<<"$other_json" >/dev/null || {
  printf 'workstreams returned another checkout workstreams\n' >&2
  printf '%s\n' "$other_json" >&2
  exit 1
}

# `ai-memory rename-workstream` must correct a name without disturbing anything
# keyed on the workstream. The id is stable, so the ledger, managed runs, and
# linked native sessions follow the rename, and neither the listing order nor
# the workstream a bare `ai-memory run` resumes may move as a side effect of
# relabelling.
before_rename_json=$workstreams_json
adopt_id=$(jq -r '.[] | select(.name == "edge-adopt") | .workstream_id' \
  <<<"$workstreams_json")
rename_json=$(cd "$REPO" && "$BIN" --data-dir "$DATA" rename-workstream \
  --from edge-adopt --to edge-adopt-fixed --json)
jq -e --arg id "$adopt_id" \
  '.workstream_id == $id and .from == "edge-adopt" and .to == "edge-adopt-fixed"' \
  <<<"$rename_json" >/dev/null || {
  printf 'rename-workstream did not report the retitled workstream\n' >&2
  printf '%s\n' "$rename_json" >&2
  exit 1
}
renamed_json=$(cd "$REPO" && "$BIN" --data-dir "$DATA" workstreams --json)
jq -e --arg id "$adopt_id" \
  '.[0].current and .[0].name == "edge-adopt-fixed" and .[0].workstream_id == $id' \
  <<<"$renamed_json" >/dev/null || {
  printf 'rename moved the listing order or the current selection\n' >&2
  printf '%s\n' "$renamed_json" >&2
  exit 1
}
jq -e '[.[] | select(.name == "edge-adopt")] | length == 0' \
  <<<"$renamed_json" >/dev/null || {
  printf 'the name the rename replaced survived in the listing\n' >&2
  printf '%s\n' "$renamed_json" >&2
  exit 1
}

# A name another workstream in this checkout already holds is refused, as is a
# name `run --new` would reject. Both must fail before writing anything.
if (cd "$REPO" && "$BIN" --data-dir "$DATA" rename-workstream \
  --from edge-kimi --to edge-adopt-fixed) >/dev/null 2>&1; then
  printf 'rename-workstream took a name another workstream already holds\n' >&2
  exit 1
fi
if (cd "$REPO" && "$BIN" --data-dir "$DATA" rename-workstream \
  --from edge-kimi --to 'nested/name') >/dev/null 2>&1; then
  printf 'rename-workstream took a name run --new would reject\n' >&2
  exit 1
fi

# The stable id is unique across every checkout, so the scope predicate has to
# repeat on the id selector: an id belonging to another worktree must read as
# absent rather than as a renamable target.
if (cd "$OTHER_REPO" && "$BIN" --data-dir "$DATA" rename-workstream \
  --project "$(basename "$REPO")" --workstream-id "$adopt_id" \
  --to hijacked) >/dev/null 2>&1; then
  printf 'rename-workstream reached a workstream outside the calling checkout\n' >&2
  exit 1
fi

# Renaming to the name it already carries writes nothing and is not an error.
noop_json=$(cd "$REPO" && "$BIN" --data-dir "$DATA" rename-workstream \
  --from edge-adopt-fixed --to edge-adopt-fixed --json)
jq -e '.from == .to and .to == "edge-adopt-fixed"' <<<"$noop_json" >/dev/null || {
  printf 'renaming to the current name was not reported as a no-op\n' >&2
  printf '%s\n' "$noop_json" >&2
  exit 1
}

# Renaming back by id has to restore the listing exactly. Anything the refused
# attempts, the no-op, or the rename itself touched -- a bumped `updated_at`, a
# moved selection, a reordered row -- surfaces as a diff here.
rename_back=$(cd "$REPO" && "$BIN" --data-dir "$DATA" rename-workstream \
  --workstream-id "$adopt_id" --to edge-adopt)
grep -q "Renamed workstream 'edge-adopt-fixed' to 'edge-adopt'" \
  <<<"$rename_back" || {
  printf 'human rename output did not report both names\n' >&2
  printf '%s\n' "$rename_back" >&2
  exit 1
}
after_rename_json=$(cd "$REPO" && "$BIN" --data-dir "$DATA" workstreams --json)
[ "$after_rename_json" = "$before_rename_json" ] || {
  printf 'a rename round trip left the listing changed\n' >&2
  diff <(printf '%s\n' "$before_rename_json") \
    <(printf '%s\n' "$after_rename_json") >&2 || true
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
    commandcode | cmdc | cmd) harness=command-code ;;
    kiro | kiro-cli)
      printf 'skipping kiro in the scripted real-harness phase: noninteractive Kiro uses a different session store; run the documented interactive Kiro acceptance separately\n' >&2
      continue
      ;;
    grok-build) harness=grok ;;
    antigravity-cli | agy) harness=antigravity ;;
    *) harness=$requested_harness ;;
  esac
  harness_command=$harness
  [ "$harness" != antigravity ] || harness_command=agy
  if command -v "$harness_command" >/dev/null 2>&1; then
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
COMMAND_CODE_ACCEPTANCE_HOME="$CONFIG/command-code-home"
COMMAND_CODE_SETTINGS="$COMMAND_CODE_ACCEPTANCE_HOME/.commandcode/settings.json"
GROK_ACCEPTANCE_HOME="$CONFIG/grok-home"
ANTIGRAVITY_ACCEPTANCE_HOME="$CONFIG/antigravity-home"
ANTIGRAVITY_HOOKS="$ANTIGRAVITY_ACCEPTANCE_HOME/.gemini/config/hooks.json"
mkdir -p "$(dirname "$CLAUDE_SETTINGS")" "$(dirname "$CODEX_HOOKS")" \
  "$(dirname "$OPENCODE_PLUGIN")" "$(dirname "$PI_EXTENSION")" \
  "$(dirname "$OMP_EXTENSION")" "$OMP_AGENT_DIR" "$OPENCODE_DATA_HOME/opencode" \
  "$CRUSH_DATA_DIR" "$KIMI_ACCEPTANCE_HOME" "$GROK_ACCEPTANCE_HOME" \
  "$(dirname "$COMMAND_CODE_SETTINGS")" \
  "$(dirname "$ANTIGRAVITY_HOOKS")" \
  "$ANTIGRAVITY_ACCEPTANCE_HOME/.gemini/antigravity-cli"

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

# Command Code resolves its documented user config, credentials, hooks, and
# session catalog through HOME/USERPROFILE. Copy only login/model preferences;
# the acceptance HOME receives a fresh session store and hook file.
for relative in auth.json config.json; do
  if [ -f "$HOME/.commandcode/$relative" ]; then
    cp "$HOME/.commandcode/$relative" \
      "$COMMAND_CODE_ACCEPTANCE_HOME/.commandcode/$relative"
  fi
done

# Grok resolves everything below $GROK_HOME. Seed the isolated home with the
# operator's login state and settings only; native sessions, logs, and caches
# remain isolated.
for relative in auth.json config.toml; do
  if [ -f "$HOME/.grok/$relative" ]; then
    cp "$HOME/.grok/$relative" "$GROK_ACCEPTANCE_HOME/$relative"
  fi
done

# Antigravity resolves login, settings, hooks, and conversation databases below
# ~/.gemini. Copy only the minimum OAuth and settings files into an isolated
# HOME; private history, conversation databases, logs, and caches stay out.
for relative in google_accounts.json oauth_creds.json installation_id; do
  if [ -f "$HOME/.gemini/$relative" ]; then
    cp "$HOME/.gemini/$relative" \
      "$ANTIGRAVITY_ACCEPTANCE_HOME/.gemini/$relative"
  fi
done
for relative in antigravity-oauth-token installation_id jetski_state.pbtxt settings.json; do
  if [ -f "$HOME/.gemini/antigravity-cli/$relative" ]; then
    cp "$HOME/.gemini/antigravity-cli/$relative" \
      "$ANTIGRAVITY_ACCEPTANCE_HOME/.gemini/antigravity-cli/$relative"
  fi
done
if [ -f "$HOME/.gemini/config/config.json" ]; then
  cp "$HOME/.gemini/config/config.json" \
    "$ANTIGRAVITY_ACCEPTANCE_HOME/.gemini/config/config.json"
fi

install_hook() {
  local agent=$1
  local target=$2
  local -a command=(
    "$BIN" --data-dir "$DATA" install-hooks --apply
    --agent "$agent" --server-url "$URL" --auth-token "$TOKEN"
    --config-file "$target"
  )
  case "$agent" in
    claude-code | codex | kimi-code | antigravity-cli)
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
install_hook command-code "$COMMAND_CODE_SETTINGS"
install_hook antigravity-cli "$ANTIGRAVITY_HOOKS"

agent_wire_name() {
  case "$1" in
    claude) printf 'claude-code\n' ;;
    opencode) printf 'open-code\n' ;;
    kimi) printf 'kimi-code\n' ;;
    command-code) printf 'command-code\n' ;;
    antigravity) printf 'antigravity-cli\n' ;;
    *) printf '%s\n' "$1" ;;
  esac
}

uppercase() {
  printf '%s' "$1" | tr '[:lower:]' '[:upper:]'
}

run_harness() {
  local harness=$1
  local current=$2
  local first_run=$3
  local expect_context=$4
  local log="$LOGS/real-$harness-$current.log"
  local prompt
  local expected_agent
  local before_sequence=0
  local before_delivery=0
  local before_observations=0
  local existing_hex
  local -a wrapper_args native_args
  expected_agent=$(agent_wire_name "$harness")
  before_observations=$(agent_observation_count "$expected_agent")
  existing_hex=$(workstream_hex "$WORKSTREAM_NAME")
  if [ -n "$existing_hex" ]; then
    before_sequence=$(latest_workstream_sequence "$existing_hex")
    before_delivery=$(current_delivery_cursor "$existing_hex" "$expected_agent")
  fi
  prompt="Reply with exactly: $current"
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
    command-code)
      native_args=(-p --no-auto-update "$prompt")
      [ -z "${AI_MEMORY_ACCEPTANCE_COMMAND_CODE_MODEL:-}" ] || native_args=(-p --no-auto-update -m "$AI_MEMORY_ACCEPTANCE_COMMAND_CODE_MODEL" "$prompt")
      ;;
    grok)
      native_args=(-p "$prompt")
      [ -z "${AI_MEMORY_ACCEPTANCE_GROK_MODEL:-}" ] || native_args=(-p -m "$AI_MEMORY_ACCEPTANCE_GROK_MODEL" "$prompt")
      ;;
    antigravity)
      native_args=(-p --print-timeout "${AI_MEMORY_ACCEPTANCE_ANTIGRAVITY_TIMEOUT:-5m}" "$prompt")
      [ -z "${AI_MEMORY_ACCEPTANCE_ANTIGRAVITY_MODEL:-}" ] || native_args=(-p --print-timeout "${AI_MEMORY_ACCEPTANCE_ANTIGRAVITY_TIMEOUT:-5m}" --model "$AI_MEMORY_ACCEPTANCE_ANTIGRAVITY_MODEL" "$prompt")
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
  elif [ "$harness" = command-code ]; then
    (cd "$REPO" && HOME="$COMMAND_CODE_ACCEPTANCE_HOME" \
      "$BIN" --data-dir "$DATA" run "${wrapper_args[@]}" "$harness" "${native_args[@]}") \
      >"$log" 2>&1
  elif [ "$harness" = grok ]; then
    (cd "$REPO" && GROK_HOME="$GROK_ACCEPTANCE_HOME" \
      "$BIN" --data-dir "$DATA" run "${wrapper_args[@]}" "$harness" "${native_args[@]}") \
      >"$log" 2>&1
  elif [ "$harness" = antigravity ]; then
    (cd "$REPO" && HOME="$ANTIGRAVITY_ACCEPTANCE_HOME" \
      "$BIN" --data-dir "$DATA" run "${wrapper_args[@]}" "$harness" "${native_args[@]}") \
      >"$log" 2>&1
  else
    (cd "$REPO" && "$BIN" --data-dir "$DATA" run \
      "${wrapper_args[@]}" "$harness" "${native_args[@]}") >"$log" 2>&1
  fi

  local hex native_id
  hex=$(workstream_hex "$WORKSTREAM_NAME")
  [ "${#hex}" -eq 32 ] || {
    printf 'could not resolve workstream %s after the %s leg\n' \
      "$WORKSTREAM_NAME" "$harness" >&2
    tail -120 "$log" >&2
    return 1
  }
  if ! native_id=$(assert_managed_leg "$hex" "$expected_agent" \
    "$before_sequence" "$before_delivery" "$expect_context" "$log" \
    "$before_observations"); then
    return 1
  fi
  printf '%s\n' "$native_id"
}

WORKSTREAM_NAME="native-acceptance-$(date +%s)-$$"
RUN_TAG="$(date +%s)-$$"
first_harness=${harnesses[0]}
first_agent=$(agent_wire_name "$first_harness")
first_native=""
previous_agent=""
index=0
for harness in "${harnesses[@]}"; do
  current="AMWS-$RUN_TAG-$(uppercase "$harness")"
  current_agent=$(agent_wire_name "$harness")
  first_run=0
  expect_context=0
  [ "$index" -ne 0 ] || first_run=1
  if [ -n "$previous_agent" ] && [ "$current_agent" != "$previous_agent" ]; then
    expect_context=1
  fi
  if ! native_id=$(run_harness "$harness" "$current" "$first_run" \
    "$expect_context"); then
    exit 1
  fi
  if [ "$index" -eq 0 ]; then
    first_native=$native_id
  fi
  previous_agent=$current_agent
  index=$((index + 1))
done

return_sentinel="AMWS-$RUN_TAG-$(uppercase "$first_harness")-RETURN"
return_expects_context=0
[ "$first_agent" = "$previous_agent" ] || return_expects_context=1
if ! returned_native=$(run_harness "$first_harness" "$return_sentinel" 0 \
  "$return_expects_context"); then
  exit 1
fi
[ "$returned_native" = "$first_native" ] || {
  printf '%s resumed native session %s, expected %s\n' \
    "$first_harness" "$returned_native" "$first_native" >&2
  exit 1
}

printf 'real managed-workstream acceptance passed: %s\n' "${harnesses[*]}"
printf 'returned to %s native session %s\n' "$first_harness" "$first_native"
printf 'native harness session stores and resume paths were exercised\n'
