# Troubleshooting

Every error extends `AiMemoryError` and has a stable `code`. Branch with
`instanceof` or `switch (error.code)`. Server text inside an error is cut to
300 characters with control characters replaced, and never contains your
token.

| Class | `code` | Typical cause |
|---|---|---|
| `ConfigError` | `config` | bad server URL or token option |
| `ArgumentError` | `argument` | an argument failed validation before sending |
| `ScopeError` | `scope` | no scope, half a scope, or a scope with `global: true` |
| `TransportError` | `transport` | connection refused, DNS, TLS, or a redirect |
| `TimeoutError` | `timeout` | the client's deadline expired |
| `AbortError` | `aborted` | your `AbortSignal` fired |
| `AuthError` | `unauthorized` | HTTP 401 |
| `ForbiddenError` | `forbidden` | HTTP 403 |
| `NotFoundError` | `not_found` | HTTP 404 |
| `RateLimitedError` | `rate_limited` | HTTP 429 |
| `HttpError` | `http` | any other non-2xx status |
| `RpcError` | `rpc` | the server answered with a JSON-RPC error |
| `InvalidParamsError` | `invalid_params` | JSON-RPC `-32602`: bad arguments or an unknown scope |
| `PreconditionFailedError` | `precondition_failed` | a conditional write or delete found another version |
| `ToolError` | `tool` | the tool reported its own failure |
| `ProtocolError` | `protocol` | the answer did not have the expected shape |
| `UnsupportedServerError` | `unsupported_server` | the server lacks a tool or argument the call needs |

## AuthError

The server did not accept the bearer token.

- Check the token is the server's root token or a live `aim_` user key.
  Rotating a key invalidates the old one immediately.
- A key from one server does not work on another: keys are tied to the
  server's token pepper.
- A bearer never authenticates `/auth/*` (human login). The SDK does not call
  those routes.

## ForbiddenError

You are authenticated but not allowed.

- **Admin routes with a user key.** `/admin/*` is root-only once the server has
  database users, and an `aim_` key is always user-level. Use the root token
  for `AdminClient`.
- **Host not allowed.** A server bound to a non-loopback address checks the
  `Host` header against `AI_MEMORY_ALLOWED_HOSTS`. Use a hostname in that list.

Access refusals on MCP tools (a restricted project without a grant) arrive as a
JSON-RPC error, so they surface as `RpcError` with a message that names the
project and the level needed. Ask the operator for a grant
([users.md](../../../docs/users.md#per-project-access)).

## ScopeError

The call had no scope, or only half of one, or an empty name. Pass
`{ scope: { workspace, project } }` or set `scope` on the client. A global
query takes no scope at all: `query({ query, global: true })`.

## InvalidParamsError on the first read of a new project

Reads never create a workspace or project, so `status`, `pages.read`, `query`
and the other reads fail with `workspace '...' not found` (or project) until
something writes there. Write a page first, or check the names for typos.

`observations()` without a `sessionId` also raises it when the project has no
completed session yet.

## RpcError on pages.read

A page that does not exist is reported as a JSON-RPC internal error (`-32603`,
`page ... not found`). So are access refusals and admission-webhook
rejections. The server does not mark these with a machine-readable reason
yet, so the SDK cannot give them their own classes; inspect `error.message`
if you need to tell them apart, or check with `api.page`, which answers 404.

## NotFoundError from every /api/v1 call

The server is probably running without `--enable-api` (or `--enable-web`).
`(await memory.capabilities()).apiMounted` tells you. MCP calls keep working
either way.

A 404 on a single page or session means that page, session, workspace or
project does not exist or is not visible to you.

## UnsupportedServerError

The server is older than the SDK needs, or it was built without a tool. The
error names the missing `feature`, the `serverVersion` it reported and the
`minimumVersion`. Check the server with `ai-memory --version` or
`(await memory.identity()).version`, and upgrade it
([install.md](../../../docs/install.md)). The SDK refuses before writing
anything: an older server would ignore `expectedPageId` and turn a guarded
write into an unconditional one.

`api.recent` raises it when the server answers the legacy array form.

`jobs.start` and `jobs.status` raise it when the server does not list
`memory_processing_job_start` or `memory_processing_job_status`. That is the
normal state of a server with `[chunking] enabled = false` (the default), and
the message says so; the fix is the server's configuration, not an upgrade.
`jobs.discover()` answers the question without an error. The server's tool
list is cached per client, so after restarting the server with the flag on
call `capabilities({ refresh: true })`. A cached client talking to a server
whose flag was turned off gets a `ToolError` from the server instead: the
server refuses a start explicitly and never falls back to the synchronous
path.

## Uncertain timeouts

A `TimeoutError` or `AbortError` with `uncertain: true` means the call could
change state and the request had already been sent. The server may or may not
have applied it. The SDK never retries on its own. Reconcile first:

- **A write.** Read the page and compare its `pageId` with what you expected.
  If you wrote with `expectedPageId`, retrying with the same id is safe: it
  either applies once or fails with `PreconditionFailedError`.
- **A delete.** Deletes are idempotent; retrying is safe.
- **`handoffs.accept`.** A handoff is claimed once. A blind retry can return
  `none_pending`, or claim the next handoff, while the first claim went
  unnoticed. Check `api.handoffs({ state: "accepted" })` first.
- **`handoffs.begin` or `messages.send`.** A retry may create a duplicate.
  List handoffs or the outbox (`messages.list({ box: "outbox" })`) first.
- **`messages.pop`.** Like accept: the message may already be claimed.

Reads (`uncertain: false`) are always safe to retry.

## TransportError

- **Connection refused.** Is the server running, on that port? The default is
  `http://127.0.0.1:49374`.
- **Redirect refused.** The SDK never follows a redirect, so the token cannot
  reach another origin. Point `serverUrl` at the final address, including any
  base path the server runs under (`https://host/wiki`).

## ConfigError

- **Plain HTTP to a remote host.** A token over `http://` is refused unless the
  host is loopback. Put TLS in front of the server
  ([https-via-proxy.md](../../../docs/https-via-proxy.md)) and use `https://`.
  `allowInsecureHttp: true` exists for networks you trust.
- **Credentials, a query or a fragment in the URL.** Pass the token through the
  `token` option instead.
- **Invalid token.** It must be printable ASCII without spaces.

## ProtocolError

The server answered with something the SDK could not decode: a body without a
JSON-RPC message, tool output that is not JSON, or a result missing a field
the SDK promises. Check that `serverUrl` points at ai-memory and not at a proxy
error page. If it does, the server and SDK versions may have drifted; please
report it.
