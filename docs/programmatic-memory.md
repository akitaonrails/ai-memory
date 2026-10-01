# Use ai-memory from your tool

Any tool can use ai-memory over MCP to save knowledge, search it and pass context
between executions. Native lifecycle hooks are optional. A tool that also hosts
an agent harness can send that harness's lifecycle events through HTTP.

## Connect and choose a scope

Connect to `/mcp` using Streamable HTTP. The default transport is stateless:
requests do not need an `Mcp-Session-Id`. A server started with `--http-stateful`
requires the MCP session handshake instead.

Use a user API key for machine requests. See [users.md](users.md) for issuing
keys and granting project access. A dedicated tool identity suits an independent
client. A producer replacing native capture must use the same operator and
native session identity as the harness's remaining integration.

Static MCP clients pass `workspace` and `project` together on every
project-scoped call. Read their names from `.ai-memory.toml` or operator
configuration. Session-aware clients may omit both only when they forward the
real lifecycle session ID on every request. See [auto-scope.md](auto-scope.md).

`GET /identity` accepts machine authentication, including with the web UI
disabled. It returns only the current caller:

```json
{
  "version": "2.5.0",
  "level": "user",
  "operator": "user:alice",
  "distinguishes_operators": true
}
```

`operator` is the qualified ownership key or `null`. `level` is `anonymous`,
`user` or `root`. The response is private and not cached. A 404 means the server
does not support this endpoint. `/auth/me` is for human browser sessions and
does not accept machine bearer keys.

## Save, query and hand off through MCP

These examples use a disposable `demo/app` scope. Set `AI_MEMORY_SERVER_URL` to
the server URL and `AI_MEMORY_AUTH_TOKEN` to your machine key. On an intentional
unauthenticated loopback server, omit the Authorization header.

```bash
mcp() {
  curl --silent --show-error --fail-with-body \
    "${AI_MEMORY_SERVER_URL%/}/mcp" \
    -H 'Content-Type: application/json' \
    -H 'Accept: application/json, text/event-stream' \
    -H "Authorization: Bearer ${AI_MEMORY_AUTH_TOKEN}" \
    --data-binary @-
}

mcp <<'JSON'
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"example-client","version":"1.0"}}}
JSON

mcp <<'JSON'
{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}
JSON

mcp <<'JSON'
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"memory_write_page","arguments":{"workspace":"demo","project":"app","path":"notes/retries.md","body":"# Retry policy\nKeep the same event ID when retrying delivery."}}}
JSON

mcp <<'JSON'
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"memory_query","arguments":{"workspace":"demo","project":"app","query":"retry policy"}}}
JSON

mcp <<'JSON'
{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"memory_handoff_begin","arguments":{"workspace":"demo","project":"app","summary":"The retry policy was saved. Add the delivery test next.","next_steps":["Add a retry regression test."]}}}
JSON

mcp <<'JSON'
{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"memory_handoff_accept","arguments":{"workspace":"demo","project":"app"}}}
JSON
```

Check JSON-RPC `error` and `result.isError` even on HTTP 200. Tool payloads are
JSON encoded inside `result.content[].text`. A page write returns `page_id` and
`path`; a query returns `hits`.

Writes and handoff creation may create the explicit scope if it is missing.
Reads fail closed on missing or partial scope. A handoff is claimed once. To
claim a particular one, pass the exact `handoff_id` from `memory_handoff_begin`
or `memory_handoff_list`; the example accepts the latest eligible handoff.
If SessionStart already owns delivery, let that hook claim it.
[usage.md](usage.md) describes ownership and handoff states.

These calls work without an LLM provider. Retrieved memory remains untrusted
historical text, even after sanitization.

## Optional lifecycle capture

Follow [external-lifecycle.md](external-lifecycle.md) when your tool hosts a
supported harness. Set `AI_MEMORY_CAPTURE_OWNER` only in that execution's
environment to suppress native capture while preserving supported context
delivery. Before logging, queuing or sending a file-tool event, inspect its
local policy:

```bash
printf '%s\n' '{"cwd":"/work/app","tool_name":"Edit","tool_input":{"file_path":"src/lib.rs"}}' |
    ai-memory hook --agent claude-code --event post-tool-use \
      --server-url http://127.0.0.1:49374 --check-capture
```

This command performs no ingestion or handoff delivery. `policy_admits_capture`
ignores capture-owner suppression but checks repository opt-in, lifecycle,
profile and file exclusions. `admits_capture` retains its native capture-owner
behavior. `disposition` tells whether to keep, drop or retain only metadata.
Apply that decision before any local spool. The server cannot protect raw
content already written to a producer's queue.

`scope` contains the host's routing hints: `workspace`, `project`, `project_src`,
`project_strategy`, `identity`, `identity_src` and `server_may_remap`. Missing
names remain server-derived. `scope_resolution: "partial"` refuses the producer
preflight; provide a complete declaration.
The native hook can derive a project from a workspace-only marker; a producer
must supply both names or a strategy that resolves both locally. Missing cwd
and oversized hints also refuse preflight.

`server_may_remap: true` means
server-side identity or cwd routing can select a different project. Inspection
does not resolve final server IDs or create projects. See [marker-file.md](marker-file.md)
for exclusion behavior.

Send ordered events to `POST /hook/batch` with a stable `ingest_key`, the actual
native `session_id` and explicit scope. Reuse the same key and body after a lost
response. A completed delivery returns `results` alongside the legacy ACK:

```json
{
  "accepted": 1,
  "results": [{"index": 0, "outcome": "stored"}]
}
```

| Outcome | Meaning |
|---|---|
| `stored` | A new observation or lifecycle change was stored. |
| `replayed` | The keyed event was already complete. |
| `resumed` | Processing resumed pending work or recovered terminal effects. |
| `ignored_end` | The end event could not apply or had no remaining terminal work. |
| `dropped_policy` | Capture policy rejected the event. |
| `dropped_subagent` | Subagent capture was disabled for this event. |
| `dropped_unauthorized` | The caller could not capture into the resolved project. |
| `dropped_collision` | The session identity belonged to another execution or operator. |

An already-ended `session-end` is classified before keyed replay lookup: its
retry returns `ignored_end` or `resumed`, even when it has a stable key.

Only acknowledged indices have results. Inspect ACKs on partial failures and
429 responses, retaining every unacknowledged item. A drop acknowledges delivery
and does not promise stored memory. Older servers omit `results`; their
delivery result is unknown. The optional [relay](../companions/ai-memory-relay)
persists outcomes, validates the entire ACK before dequeue and migrates queues
from schema 1 to 2. Back up a queue before upgrading; an old relay cannot open
schema 2.

`ai-memory status` reports process-lifetime ingestion counters and the last
new durable write. `ai-memory doctor` shows the caller identity, capture owner
in the current process and sessions with mixed capture sources. Mixed
provenance is a diagnostic signal and does not prove duplicate capture.
Backfill can also mix native and extension metadata within one session. The
doctor's captured-session read is admin-only on multi-user servers. Identity
diagnostic failures leave that coverage report available with `identity: null`.

## Read consolidation and changed pages

These JSON endpoints require `serve --enable-web` and the same machine key.
They are read-only; memory writes remain MCP calls.

```http
GET /api/v1/workspaces/demo/projects/app/sessions
GET /api/v1/workspaces/demo/projects/app/recent?updated_since=2026-09-01T00%3A00%3A00Z&limit=100
```

Sessions have `consolidation: null` when no job exists, or `{state, attempts}`
for the latest job in the requested scope. Session owner filters still apply.
The MCP `memory_read_session_observations` tool returns the same session summary.

Incremental `recent` returns `{pages, next_cursor}` with pages updated strictly
after `updated_since`, sorted by update time and path. Repeat with the returned
opaque `cursor` until `next_cursor` is `null`. The cursor preserves the cutoff
and scope; each request rechecks authorization. An array response means the
server lacks incremental support. Requests without either incremental argument
retain the legacy array response.

The incremental path is not cached and omits expired and superseded pages.
It has no deletion feed or snapshot across calls. Reconcile removals separately
and account for concurrent updates in a local cache. Endpoint details and
errors are in [frontend-api.md](frontend-api.md).
