#!/usr/bin/env bash
# Two-operator acceptance run against the live container.
#
# Every assertion below maps to a claim the PR makes. Failures print the actual
# payload rather than just a verdict, because a test that says only "FAIL" on a
# multi-operator boundary is not much better than no test.

PASS=0; FAIL=0
ALICE=8081; BOB=8082; CAROL=8083; RAW=49374

# Must match `[auth].bearer_token` in config.toml. Kept in a variable rather
# than inline so the file carries no `Authorization: Bearer <literal>` pattern
# for a secret scanner to flag — this is a throwaway value for a loopback-only
# container, but a repo-wide scanner cannot know that.
BEARER="${AI_MEMORY_TEST_BEARER:-$(sed -n 's/^bearer_token = "\(.*\)"/\1/p' config.toml)}"
PROXY_SECRET="${AI_MEMORY_TEST_PROXY_SECRET:-$(sed -n 's/^actor_proxy_secret = "\(.*\)"/\1/p' config.toml)}"

mcp() { # mcp <port> <tool> <json-args>
  curl -s -X POST "http://localhost:$1/mcp" \
    -H "Content-Type: application/json" -H "Accept: application/json, text/event-stream" \
    -d "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"$2\",\"arguments\":$3}}"
}
text() { python3 -c "import sys,json;d=json.load(sys.stdin);print(d.get('result',{}).get('content',[{}])[0].get('text','')) if 'result' in d else print(json.dumps(d))" 2>/dev/null; }

check() { # check <name> <condition-result> <evidence>
  if [ "$2" = "yes" ]; then echo "  PASS  $1"; PASS=$((PASS+1));
  else echo "  FAIL  $1"; echo "        evidence: $3"; FAIL=$((FAIL+1)); fi
}

echo "=============================================================="
echo "A. Slots are namespaced per operator, and stay private"
echo "=============================================================="
A_SLOT=$(mcp $ALICE memory_write_page '{"path":"_slots/current-focus.md","title":"Alice focus","body":"ALICE-SECRET-FOCUS","tier":"semantic"}' | text)
echo "  alice wrote -> $(echo "$A_SLOT" | tr -d '\n' | head -c 200)"
echo "$A_SLOT" | grep -q "_slots/alice/" && R=yes || R=no
check "alice's shared-slot write is re-homed to _slots/alice/" "$R" "$A_SLOT"

B_SLOT=$(mcp $BOB memory_write_page '{"path":"_slots/current-focus.md","title":"Bob focus","body":"BOB-SECRET-FOCUS","tier":"semantic"}' | text)
echo "$B_SLOT" | grep -q "_slots/bob/" && R=yes || R=no
check "bob's shared-slot write is re-homed to _slots/bob/" "$R" "$B_SLOT"

# The session brief is the surface that carries slot BODIES into an agent's
# context, so it is the one that matters. `memory_briefing` returns paths and
# titles only — asserting "no body leaked" against it passes trivially.
brief() { curl -s "http://localhost:$1/handoff?workspace=mutest&project=app&cwd=/work&agent=claude-code&briefing=1"; }

A_BRIEF=$(brief $ALICE); B_BRIEF=$(brief $BOB)
echo "$A_BRIEF" | grep -q "ALICE-SECRET-FOCUS" && R=yes || R=no
check "alice's session brief carries HER OWN slot body" "$R" "not injected"
echo "$B_BRIEF" | grep -q "ALICE-SECRET-FOCUS" && R=no || R=yes
check "bob's session brief does NOT carry alice's slot body" "$R" "LEAKED into bob's agent context"
echo "$A_BRIEF" | grep -q "BOB-SECRET-FOCUS" && R=no || R=yes
check "alice's session brief does NOT carry bob's slot body" "$R" "LEAKED into alice's agent context"
echo "$A_BRIEF" | grep -q "FORGED" && R=yes || R=no
check "the SHARED slot still reaches everyone (absent = shared)" "$R" "shared slot went missing"

echo
echo "=============================================================="
echo "B. Handoffs go to their owner"
echo "=============================================================="
mcp $ALICE memory_handoff_begin '{"summary":"ALICE-BATON","next_steps":["finish the alice thing"]}' >/dev/null
BOB_FETCH=$(mcp $BOB memory_handoff_accept '{}' | text)
echo "$BOB_FETCH" | grep -q "ALICE-BATON" && R=no || R=yes
check "bob cannot consume alice's baton" "$R" "$(echo "$BOB_FETCH" | tr -d '\n' | head -c 200)"

ALICE_FETCH=$(mcp $ALICE memory_handoff_accept '{}' | text)
echo "$ALICE_FETCH" | grep -q "ALICE-BATON" && R=yes || R=no
check "alice DOES receive her own baton" "$R" "$(echo "$ALICE_FETCH" | tr -d '\n' | head -c 200)"

echo
echo "=============================================================="
echo "C. The admin gate holds under a trusted proxy (review finding #1)"
echo "=============================================================="
SWEEP=$(mcp $ALICE memory_forget_sweep '{"dry_run":true}')
echo "$SWEEP" | grep -qi "error\|not permitted\|requires\|capability" && R=yes || R=no
check "a proxied non-root operator is DENIED the sweep" "$R" "$(echo "$SWEEP" | tr -d '\n' | head -c 250)"

echo
echo "=============================================================="
echo "D. An OIDC-subject-only ingress names a real operator"
echo "=============================================================="
C_SLOT=$(mcp $CAROL memory_write_page '{"path":"_slots/current-focus.md","title":"Carol focus","body":"CAROL-SECRET-FOCUS","tier":"semantic"}' | text)
echo "  carol wrote -> $(echo "$C_SLOT" | tr -d '\n' | head -c 200)"
echo "$C_SLOT" | grep -q "_slots/oidc-subject-carol/" && R=yes || R=no
check "carol (sub only) gets her own namespace, not the shared slot" "$R" "$C_SLOT"

C_BRIEF=$(brief $CAROL)
echo "$C_BRIEF" | grep -q "CAROL-SECRET-FOCUS" && R=yes || R=no
check "carol (sub only) SEES her own slot body in her brief" "$R" "$(echo "$C_BRIEF" | tr -d '\n' | head -c 200)"
echo "$C_BRIEF" | grep -q "ALICE-SECRET-FOCUS" && R=no || R=yes
check "carol does not see alice's slot" "$R" "leaked"

echo
echo "=============================================================="
echo "E. Header forgery fails closed"
echo "=============================================================="
FORGED=$(curl -s -X POST "http://localhost:$RAW/mcp" \
  -H "Content-Type: application/json" -H "Accept: application/json, text/event-stream" \
  -H "Authorization: Bearer $BEARER" \
  -H "X-Memory-Actor-User: alice" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"memory_write_page","arguments":{"path":"_slots/current-focus.md","title":"forged","body":"FORGED","tier":"semantic"}}}' | text)
echo "$FORGED" | grep -q "_slots/alice/" && R=no || R=yes
check "a forged actor header WITHOUT the proxy secret is ignored" "$R" "$(echo "$FORGED" | tr -d '\n' | head -c 200)"

DUP=$(curl -s -o /dev/null -w "%{http_code}" -X POST "http://localhost:$RAW/mcp" \
  -H "Content-Type: application/json" -H "Accept: application/json, text/event-stream" \
  -H "Authorization: Bearer $BEARER" \
  -H "X-Memory-Actor-Proxy-Secret: $PROXY_SECRET" \
  -H "X-Memory-Actor-User: alice" -H "X-Memory-Actor-User: bob" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"memory_status","arguments":{}}}')
[ "$DUP" = "400" ] && R=yes || R=no
check "a duplicated actor header is refused with 400 (got $DUP)" "$R" "http $DUP"

echo
echo "=============================================================="
echo "RESULT: $PASS passed, $FAIL failed"
echo "=============================================================="
[ "$FAIL" -eq 0 ]
