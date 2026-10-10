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

ai-memory's own tools report a failure as a JSON-RPC `error` with a code and
message, for example `-32602` for invalid parameters or `-32603` when an
admission webhook rejects a write. `result.isError` is where MCP lets a tool
report its own failure, so check it too. A caller using `jq` can branch on both:

```bash
response=$(mcp <<'JSON'
{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"memory_read_page","arguments":{"workspace":"demo","project":"app","path":"notes/retries.md"}}}
JSON
)
if jq -e '.error' <<<"$response" >/dev/null; then
  jq -r '"rpc error \(.error.code): \(.error.message)"' <<<"$response" >&2
elif jq -e '.result.isError == true' <<<"$response" >/dev/null; then
  jq -r '.result.content[].text' <<<"$response" >&2
else
  jq -r '.result.content[].text | fromjson' <<<"$response"
fi
```

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

## Lifecycle from code

Deleting, restoring and purging memory goes through two surfaces. A page
delete is an MCP tool. Restore and purge are admin HTTP routes. Every
`/admin/*` route is root-only once the deployment has a DB user or
trusted-proxy identities (see [users.md](users.md)); until then they accept
any caller the server's auth lets in.

| Operation | Call | Notes |
|---|---|---|
| Delete a page (there is no separate archive) | MCP `memory_delete_page` with `path` (plus `workspace` and `project`) | Checkpoints the wiki in git first, then removes the file and every indexed version of the page. Returns `{path, deleted, pre_checkpoint, checkpoint}`. Idempotent. |
| Restore a deleted or overwritten page | `POST /admin/restore-page` with `{workspace, project, path, rev}` | `rev` is any git revision that still holds the page: the delete's `pre_checkpoint`, or `<checkpoint>~1` when `pre_checkpoint` is `null` because the tree was already clean. Writes that version back as the latest page and reindexes it. After a delete, only that one version returns. |
| Find a revision | `GET /admin/checkpoints?limit=N` | Recent checkpoints (up to 100) with `oid` and `summary`. A delete's summary is `memory_delete_page: <path>`. |
| Purge one session | `POST /admin/purge-session` with `{workspace, project, session_id, confirm}` | Irreversible. Optional `dry_run` and `compact`. |
| Purge a whole project | `POST /admin/purge-project` with `{workspace, project, confirm}` | Irreversible. Optional `dry_run`, `compact`, and `force` (purge despite a live managed-workstream lease). |
| Read superseded versions | MCP `memory_query` with `include_superseded=true` | Older versions are labelled `superseded: true`. A delete removes them along with the current version. |

Both purge routes refuse with 400 unless `confirm` is `true`, and `dry_run`
wins over `confirm`: a body with both set only previews the counts. `confirm`
is a required field, so a preview sends it as `false`. Once root-only applies,
`ROOT_TOKEN` below must be the root bearer token; a user API key is refused.

```bash
curl --silent --show-error --fail-with-body \
  "${AI_MEMORY_SERVER_URL%/}/admin/purge-session" \
  -H 'Content-Type: application/json' \
  -H "Authorization: Bearer ${ROOT_TOKEN}" \
  -d '{"workspace":"demo","project":"app","session_id":"<session-uuid>","confirm":false,"dry_run":true}'
```

There is no endpoint that lists deleted pages and no MCP tool that restores
one. To recover a page, take the revision from the delete response or from
`/admin/checkpoints`, then call `/admin/restore-page` as root.
[lifecycle-ops.md](lifecycle-ops.md) covers what each purge deletes, the
matching CLI commands, and the cross-project side effects.
