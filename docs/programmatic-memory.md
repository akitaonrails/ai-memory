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
native session identity as the harness's remaining integration. See the
[external lifecycle guide](external-lifecycle.md) when sending those events.

Static MCP clients pass `workspace` and `project` together on every
project-scoped call. Read their names from `.ai-memory.toml` or operator
configuration. Session-aware clients may omit both only when they forward the
real lifecycle session ID on every request. See [auto-scope.md](auto-scope.md).

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

## Change a page only if nobody else has

`memory_read_page` returns the page's current version as `page_id` (`/api/v1`
returns it as `id`). Pass it back as `expected_page_id` to `memory_write_page`
or `memory_delete_page` and the call acts only if that is still the latest
version; pass `create_only: true` to a write that must not overwrite an existing
page. When the page changed in between, nothing is written or deleted and the
call fails with JSON-RPC `invalid_request` whose `data` is
`{"reason": "precondition_failed", "path", "expected_page_id",
"current_page_id"}`; re-read the page and decide. A caller without write access
gets the usual authorization error, never the current version.

## Read changed pages

These JSON endpoints require `serve --enable-api` (or `--enable-web`, which
continues to imply the API) and the same machine key. They are read-only;
memory writes remain MCP calls.

```http
GET /api/v1/workspaces/demo/projects/app/recent?updated_since=2026-09-01T00%3A00%3A00Z&limit=100
```

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
